//! Skill-patch stage: turn a mistake category's evidence into a minimal, validated edit of the skill file,
//! computed from files read through the GitHub API (no clone). Every draft is stored as a `proposed`
//! skill adjustment; `pr` later pushes it as a new `skilliterate/` branch and opens a draft PR.
use crate::adjust;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use skilliterate_core::config::Config;
use skilliterate_gh::api;
use skilliterate_llm::Claude;

/// Priority list: severity-weighted findings per category, over repos that use the skill.
pub fn rank(conn: &Connection, skill: &str) -> Result<()> {
    let mut st = conn.prepare("SELECT c.id, c.name, COALESCE(c.skill_preventable,0), COUNT(*),
        SUM(CASE f.severity WHEN 'Critical' THEN 5 WHEN 'Major' THEN 3 ELSE 1 END) AS score,
        SUM(CASE WHEN f.severity IN ('Major','Critical') THEN 1 ELSE 0 END)
        FROM finding_categories fc JOIN categories c ON c.id=fc.category_id JOIN findings f ON f.id=fc.finding_id
        JOIN prs p ON p.id=f.pr_id JOIN repos r ON r.id=p.repo_id WHERE (?2='*' OR r.skill=?1) AND c.name NOT LIKE 'Other%'
        GROUP BY c.id ORDER BY score DESC")?;
    println!("{:>3}  {:<46}{:>6}{:>9}{:>7}  skill-preventable (Sonnet's guess)", "id", "category", "n", "maj+crit", "score");
    let scope = if load_bundle(conn, skill)?.all_evidence { "*" } else { skill };
    for r in st.query_map([skill, scope], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?)))? {
        let (id, name, sp, n, score, hi) = r?;
        println!("{id:>3}  {:<46}{n:>6}{hi:>9}{score:>7}  {}", name.chars().take(45).collect::<String>(), if sp { "yes" } else { "no" });
    }
    Ok(())
}

/// Publishing bundle for a skill: files that must change together for CI to pass (mirrors, plugin manifests, marketplace entry).
#[derive(Default)]
pub struct Bundle { mirrors: Vec<String>, json_versions: Vec<String>, marketplace: Option<(String, String)>, generic: bool, all_evidence: bool, base: Option<String> }

fn load_bundle(conn: &Connection, skill: &str) -> Result<Bundle> {
    let Ok(raw) = conn.query_row::<String, _, _>("SELECT value FROM meta WHERE key=?1", [format!("bundle:{skill}")], |r| r.get(0)) else { return Ok(Bundle::default()) };
    parse_bundle(&raw)
}

/// Parse and validate a bundle JSON: every path must stay inside the repo.
pub fn parse_bundle(raw: &str) -> Result<Bundle> {
    let v: Value = serde_json::from_str(raw).context("bundle is not valid JSON")?;
    let strs = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let b = Bundle {
        mirrors: strs("mirrors"), json_versions: strs("json_versions"),
        marketplace: v["marketplace"]["file"].as_str().zip(v["marketplace"]["plugin"].as_str()).map(|(f, p)| (f.to_string(), p.to_string())),
        generic: v["generic"].as_bool().unwrap_or(false), all_evidence: v["evidence"].as_str() == Some("all"), base: v["base"].as_str().map(String::from),
    };
    for f in b.mirrors.iter().chain(&b.json_versions).chain(b.marketplace.iter().map(|(f, _)| f)) { skilliterate_core::config::safe_rel("bundle", f)?; }
    Ok(b)
}

/// Bump the first `"version": "x.y.z"` at/after `anchor` (e.g. the plugin's `"name": "my-plugin"`), editing text in place so formatting is untouched.
fn bump_json_version(text: &str, anchor: Option<&str>) -> Result<(String, String, String)> {
    let start = match anchor { Some(a) => text.find(a).with_context(|| format!("anchor {a} not found"))?, None => 0 };
    let k = text[start..].find("\"version\"").context("no \"version\" key")? + start;
    let q1 = text[k + 9..].find('"').context("malformed version")? + k + 9;
    let q2 = text[q1 + 1..].find('"').context("malformed version")? + q1 + 1;
    let (old, new) = (text[q1 + 1..q2].to_string(), bump(&text[q1 + 1..q2]));
    Ok((format!("{}{}{}", &text[..q1 + 1], new, &text[q2..]), old, new))
}

fn bump(v: &str) -> String {
    let mut parts: Vec<String> = v.split('.').map(String::from).collect();
    if let Some(last) = parts.last_mut() { if let Ok(n) = last.parse::<u32>() { *last = (n + 1).to_string(); } }
    parts.join(".")
}

fn current_version(text: &str) -> Option<String> {
    let mut lines = text.lines();
    if lines.next()? != "---" { return None; }
    lines.take_while(|l| *l != "---")
        .find_map(|l| l.trim().strip_prefix("version:").map(|v| v.trim().trim_matches('"').to_string()))
}

fn apply_edits(content: &str, edits: &[(String, String)]) -> std::result::Result<String, String> {
    let mut out = content.to_string();
    for (i, (old, new)) in edits.iter().enumerate() {
        let preview: String = old.chars().take(70).collect();
        if old.trim().is_empty() { return Err(format!("edit {i}: 'old' is empty")); }
        if old == new { return Err(format!("edit {i}: 'old' equals 'new'")); }
        if old.contains("version:") { return Err(format!("edit {i}: do not touch the version line; it is bumped automatically")); }
        let n = out.matches(old.as_str()).count();
        if n != 1 { return Err(format!("edit {i}: 'old' must occur EXACTLY once in the file but occurs {n} times: «{preview}…». Copy a full existing line verbatim.")); }
        out = out.replacen(old.as_str(), new, 1);
    }
    Ok(out)
}

fn slug(s: &str) -> String {
    let mut o = String::new();
    for c in s.to_lowercase().chars() { if c.is_ascii_alphanumeric() { o.push(c) } else if !o.ends_with('-') { o.push('-') } }
    o.trim_matches('-').chars().take(40).collect()
}

pub fn draft(conn: &Connection, cfg: &Config, skill: &str, category_q: &str, base: Option<&str>, check_against: &[String], max_evidence: usize) -> Result<()> {
    let sc = cfg.skill(skill)?;
    let file = sc.path.clone();
    let bundle = load_bundle(conn, skill)?;
    let base = base.map(String::from).or_else(|| bundle.base.clone()).unwrap_or_else(|| sc.base.clone());
    let base = base.as_str();
    let cat = adjust::resolve_category(conn, category_q)?;
    let (cname, cdesc, cbound): (String, String, String) = conn.query_row(
        "SELECT name,COALESCE(description,''),COALESCE(boundary,'') FROM categories WHERE id=?1", [cat], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;

    // evidence: most severe first, de-duplicated by instruction
    let mut st = conn.prepare("SELECT f.id, COALESCE(f.severity,'?'), rc.mistake_type, rc.what_went_wrong, rc.preventing_instruction
        FROM finding_categories fc JOIN findings f ON f.id=fc.finding_id JOIN root_causes rc ON rc.finding_id=f.id
        JOIN prs p ON p.id=f.pr_id JOIN repos r ON r.id=p.repo_id
        WHERE fc.category_id=?1 AND (?2='*' OR r.skill=?2)
        ORDER BY CASE f.severity WHEN 'Critical' THEN 0 WHEN 'Major' THEN 1 ELSE 2 END, f.id DESC")?;
    let all: Vec<(i64, String, String, String, String)> = st.query_map(params![cat, if bundle.all_evidence { "*" } else { skill }], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
    if all.is_empty() { bail!("no categorized findings for '{cname}' in repos using {skill}"); }
    let mut seen = std::collections::HashSet::new();
    let evidence: Vec<_> = all.iter().filter(|e| seen.insert(e.4.clone())).take(max_evidence).collect();
    let ev_text = evidence.iter().map(|e| format!("- [{}] {} — {} → {}", e.1, e.2, e.3, e.4)).collect::<Vec<_>>().join("\n");

    if base.starts_with('-') || base.contains(char::is_whitespace) { bail!("invalid base branch '{base}'"); }
    let content = api::file_at(&sc.repo, &file, base)?.with_context(|| format!("{file} not found on {}@{base}", sc.repo))?;
    // Optional: refuse to draft when the skill file differs on other branches that also release it (`--check-against`).
    for other in check_against.iter().map(String::as_str) {
        if other == base || other.starts_with('-') { continue; }
        if let Some(o) = api::file_at(&sc.repo, &file, other)? {
            if o != content { bail!("{file} differs between {base} and {other} (versions {:?} vs {:?}); reconcile the branches first", current_version(&content), current_version(&o)); }
        }
    }
    let old_version = current_version(&content).context("no version in skill frontmatter")?;
    let new_version = bump(&old_version);

    let schema = json!({"type":"object","properties":{
        "status":{"type":"string","enum":["patch","already_covered","not_skill_addressable"]},
        "summary":{"type":"string"},"rationale":{"type":"string"},
        "edits":{"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"}},"required":["old","new"]}}},
        "required":["status","summary","rationale","edits"]});
    let generic_rule = if bundle.generic { "\n- This skill is PUBLISHED for any GitHub repository, stack and CI. Keep every instruction stack-agnostic: no monorepo, framework, file or tool names from the evidence (describe the check, not the technology). If the evidence only makes sense with one repo's conventions or stack, answer not_skill_addressable." } else { "" };
    let system = format!("You maintain an agent skill (SKILL.md): instructions that AI coding agents follow while they work. CodeRabbit review findings show a recurring class of mistake. Propose the SMALLEST edit to the skill that would make an agent avoid this class of mistake before opening the PR.\nRules:\n- First check whether the skill already instructs this adequately. If so, status=already_covered and say where. (If it is covered but buried or vague, you may instead reword/promote it with status=patch.)\n- If prevention needs project-specific knowledge or cannot be checked by an agent, status=not_skill_addressable.\n- Otherwise status=patch. Match the skill's existing style (checkbox bullets like `- [ ] **Bold lead.** instruction`). Put the change in the step where the agent is when the mistake happens. Add at most ~8 lines in total. Instructions must be imperative and checkable (what to run or inspect), and generalise beyond the examples: no file names or ticket numbers from the evidence.\n- Each edit: `old` = an EXACT existing line (or short multi-line span) copied verbatim from the skill that occurs once; `new` = replacement that keeps the old text and adds or changes content. Never touch the frontmatter.\n- summary: one line describing the change, for a changelog. rationale: 1-3 sentences linking the change to the evidence.{generic_rule}");
    let base_prompt = format!("CATEGORY: {cname}\nDefinition: {cdesc}\nNot to be confused with: {cbound}\n\nEVIDENCE ({} of {} findings in this category, most severe first; format: [severity] mistake — what went wrong → instruction that would have prevented it):\n{ev_text}\n\nCURRENT SKILL FILE ({file} @ {base}):\n<skill>\n{content}\n</skill>", evidence.len(), all.len());

    let mut feedback = String::new();
    let (mut reply, mut cost_total, mut patched) = (Value::Null, 0.0, None);
    for attempt in 1..=3 {
        let prompt = if feedback.is_empty() { base_prompt.clone() } else { format!("{base_prompt}\n\nYour previous answer was rejected: {feedback}\nFix it and answer again.") };
        let rep = Claude::new("sonnet").run_json(&system, &prompt, &schema)?;
        cost_total += rep.cost_usd;
        reply = rep.value;
        if reply["status"] != "patch" { break; }
        let edits: Vec<(String, String)> = reply["edits"].as_array().cloned().unwrap_or_default().iter()
            .map(|e| (e["old"].as_str().unwrap_or("").to_string(), e["new"].as_str().unwrap_or("").to_string())).collect();
        if edits.is_empty() { feedback = "status=patch but no edits".into(); continue; }
        match apply_edits(&content, &edits) {
            Ok(t) => { patched = Some(t); break; }
            Err(e) => { eprintln!("attempt {attempt}: {e}"); feedback = e; }
        }
    }
    let _ = conn.execute("INSERT INTO llm_log(stage,model,finding_id,cost_usd) VALUES('patch','sonnet',0,?1)", [cost_total]);
    let summary = reply["summary"].as_str().unwrap_or("").to_string();
    let rationale = reply["rationale"].as_str().unwrap_or("").to_string();

    let Some(mut new_content) = patched else {
        println!("no patch drafted for '{cname}': {}\n  {}\n(cost ${cost_total:.3})", reply["status"].as_str().unwrap_or("?"), rationale);
        return Ok(());
    };
    // bump frontmatter version (first occurrence)
    let vline_old = format!("version: \"{old_version}\"");
    new_content = new_content.replacen(&vline_old, &format!("version: \"{new_version}\""), 1);

    let instruction = {
        let a: std::collections::HashSet<&str> = content.lines().collect();
        new_content.lines().filter(|l| !a.contains(l) && !l.contains("version: \"")).collect::<Vec<_>>().join("\n")
    };
    conn.execute("INSERT INTO skill_adjustments(skill,category_id,summary,instruction,version_label,status) VALUES(?1,?2,?3,?4,?5,'proposed')",
        params![skill, cat, summary, instruction, new_version])?;
    let id = conn.last_insert_rowid();
    for e in &evidence { conn.execute("INSERT OR IGNORE INTO adjustment_evidence(adjustment_id,finding_id) VALUES(?1,?2)", params![id, e.0])?; }

    // Every file the PR will write, with the blob it was derived from (None = new file) so `pr` can refuse if the base moved.
    let mut files: Vec<(String, String)> = vec![(file.clone(), new_content.clone())];
    let mut bundle_notes = Vec::new();
    for m in bundle.mirrors.iter().chain(&sc.extra_files) { files.push((m.clone(), new_content.clone())); bundle_notes.push(format!("mirror {m}")); }
    for j in bundle.json_versions.iter().chain(&sc.version_files) {
        let text = api::file_at(&sc.repo, j, base)?.with_context(|| format!("{j} not found on {base}"))?;
        let (t, o, n) = bump_json_version(&text, None)?;
        files.push((j.clone(), t)); bundle_notes.push(format!("{j} {o} → {n}"));
    }
    let market = sc.marketplace_file.clone().zip(sc.marketplace_plugin.clone()).or(bundle.marketplace.clone());
    if let Some((f, plugin)) = &market {
        let text = api::file_at(&sc.repo, f, base)?.with_context(|| format!("{f} not found on {base}"))?;
        let (t, o, n) = bump_json_version(&text, Some(&format!("\"name\": \"{plugin}\"")))?;
        files.push((f.clone(), t)); bundle_notes.push(format!("{f} [{plugin}] {o} → {n}"));
    }
    let mut seen_paths = std::collections::HashSet::new();
    files.retain(|(p, _)| seen_paths.insert(p.clone()));

    let mut diff = String::new();
    let mut payload_files = Vec::new();
    for (p, new) in &files {
        let old = api::file_at(&sc.repo, p, base)?;
        diff.push_str(&unified_diff(p, old.as_deref().unwrap_or(""), new)?);
        payload_files.push(json!({"path": p, "content": new, "base_blob": api::blob_sha(&sc.repo, p, base)?}));
    }
    let payload = json!({"repo": sc.repo, "base": base, "files": payload_files, "rationale": rationale, "evidence": evidence.len(), "total": all.len()});
    let branch = format!("skilliterate/adj-{id}-{}", slug(&cname));
    conn.execute("UPDATE skill_adjustments SET branch=?2, payload=?3, diff=?4 WHERE id=?1", params![id, branch, payload.to_string(), diff])?;

    println!("adjustment #{id} drafted for «{cname}»  v{old_version} → v{new_version}  (${cost_total:.3})\n  {summary}\n  why: {rationale}\n  evidence: {} findings (of {})\n  branch (on `patch pr {id}`): {branch}\n  bundle: {}\n\n{diff}\nnext: `skilliterate patch pr {id}` opens a draft PR on {}",
        evidence.len(), all.len(), if bundle_notes.is_empty() { "(single file)".to_string() } else { bundle_notes.join("; ") }, sc.repo);
    Ok(())
}

/// Unified diff of one file via `git diff --no-index` on two temp files (git is only used as a diff engine).
fn unified_diff(path: &str, old: &str, new: &str) -> Result<String> {
    let dir = std::env::temp_dir().join(format!("skilliterate-diff-{}", std::process::id()));
    let (a, b) = (dir.join("a").join(path), dir.join("b").join(path));
    for (f, t) in [(&a, old), (&b, new)] { std::fs::create_dir_all(f.parent().unwrap())?; std::fs::write(f, t)?; }
    let out = std::process::Command::new("git").current_dir(&dir).args(["diff", "--no-index", "--no-color", "--", &format!("a/{path}"), &format!("b/{path}")]).output().context("running git")?;
    let _ = std::fs::remove_dir_all(&dir);
    // exit code 1 just means "files differ"
    if out.status.code().map_or(true, |c| c > 1) { bail!("git diff: {}", String::from_utf8_lossy(&out.stderr)); }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Push a drafted adjustment as a new `skilliterate/` branch through the API and open a DRAFT pull request.
/// Never writes to the base branch, never merges; refuses if any file it would change moved on the base since the draft.
pub fn open_pr(conn: &Connection, id: i64) -> Result<()> {
    let (skill, status, branch, payload, summary, version, cat, pr_url): (String, String, Option<String>, Option<String>, String, Option<String>, Option<String>, Option<String>) = conn.query_row(
        "SELECT a.skill,a.status,a.branch,a.payload,a.summary,a.version_label,(SELECT name FROM categories WHERE id=a.category_id),a.pr_url FROM skill_adjustments a WHERE a.id=?1", [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))).context("no such adjustment")?;
    if status != "proposed" || pr_url.is_some() { bail!("adjustment #{id} is '{status}'{}; only a fresh proposed draft can be opened", pr_url.map(|u| format!(" ({u})")).unwrap_or_default()); }
    let (Some(branch), Some(payload)) = (branch, payload) else { bail!("adjustment #{id} has no stored draft (drafted before API mode?); run `patch draft` again") };
    let p: Value = serde_json::from_str(&payload)?;
    let (repo, base) = (p["repo"].as_str().context("payload: repo")?, p["base"].as_str().context("payload: base")?);
    let mut files = Vec::new();
    for f in p["files"].as_array().context("payload: files")? {
        let path = f["path"].as_str().context("payload: path")?;
        skilliterate_core::config::safe_rel(&skill, path)?;
        if api::blob_sha(repo, path, base)?.as_deref() != f["base_blob"].as_str() {
            bail!("{path} changed on {repo}@{base} since the draft; run `skilliterate patch draft` again (or `patch drop {id}`)");
        }
        files.push((path.to_string(), f["content"].as_str().context("payload: content")?.to_string()));
    }
    let v = version.as_deref().unwrap_or("?");
    let title = format!("skill: {summary}");
    let sha = api::commit_to_new_branch(repo, base, &branch, &format!("{title}\n\nBump to v{v}."), &files)?;
    let body = format!("{summary}\n\n**Category:** {}\n**Why:** {}\n**Evidence:** {} of {} findings in this category.\n**Version:** {v}\n\nDrafted by skilliterate from review-finding evidence. Draft PR: review and merge manually.",
        cat.unwrap_or_default(), p["rationale"].as_str().unwrap_or(""), p["evidence"], p["total"]);
    let url = match api::open_draft_pr(repo, base, &branch, &title, &body) {
        Ok(u) => u,
        Err(e) => { conn.execute("UPDATE skill_adjustments SET status='pushed' WHERE id=?1", [id])?; bail!("branch {branch} was pushed ({sha}) but opening the PR failed: {e}") }
    };
    conn.execute("UPDATE skill_adjustments SET status='pr_open', pr_url=?2 WHERE id=?1", params![id, url])?;
    println!("adjustment #{id}: draft PR opened → {url}\n  branch {branch} @ {sha}\nAfter it merges, run `skilliterate adjust ship {id}`.");
    Ok(())
}

pub fn show(conn: &Connection, id: i64) -> Result<()> {
    let diff: Option<String> = conn.query_row("SELECT diff FROM skill_adjustments WHERE id=?1", [id], |r| r.get(0)).context("no such adjustment")?;
    print!("{}", diff.context("adjustment has no stored diff")?);
    Ok(())
}

/// Discard a draft that has not been pushed. A pushed branch / open PR is left for the user to close on GitHub.
pub fn drop_draft(conn: &Connection, id: i64) -> Result<()> {
    let (status, pr): (String, Option<String>) = conn.query_row("SELECT status,pr_url FROM skill_adjustments WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).context("no such adjustment")?;
    if status != "proposed" { bail!("adjustment #{id} is '{status}'{}; close the PR/branch on GitHub instead", pr.map(|u| format!(" ({u})")).unwrap_or_default()); }
    conn.execute("UPDATE skill_adjustments SET status='dropped', payload=NULL WHERE id=?1", [id])?;
    println!("adjustment #{id} dropped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert_eq!(bump("0.7"), "0.8");
        assert_eq!(bump("1.2.9"), "1.2.10");
        assert_eq!(current_version("---\nname: x\nmetadata:\n  version: \"0.7\"\n---\nversion: \"9\"").as_deref(), Some("0.7"));
        assert_eq!(current_version("no fm"), None);
    }
    #[test]
    fn edits() {
        let f = "a\nb\nc\nb2\n";
        assert_eq!(apply_edits(f, &[("c".into(), "c\nx".into())]).unwrap(), "a\nb\nc\nx\nb2\n");
        assert!(apply_edits(f, &[("b".into(), "z".into())]).is_err()); // occurs twice (b, b2)
        assert!(apply_edits(f, &[("nope".into(), "z".into())]).is_err());
        assert!(apply_edits(f, &[("a".into(), "a".into())]).is_err());
        assert!(apply_edits("version: \"1\"", &[("version: \"1\"".into(), "version: \"2\"".into())]).is_err());
    }
    #[test]
    fn json_bump() {
        let t = "{\n \"plugins\": [\n  {\"name\": \"a\", \"version\": \"1.0.0\"},\n  {\"name\": \"my-plugin\",\n   \"description\": \"x\",\n   \"version\": \"0.1.0\"}\n ]\n}";
        let (n, o, v) = bump_json_version(t, Some("\"name\": \"my-plugin\"")).unwrap();
        assert_eq!((o.as_str(), v.as_str()), ("0.1.0", "0.1.1"));
        assert!(n.contains("\"version\": \"1.0.0\"") && n.contains("\"version\": \"0.1.1\""));
        let (_, o2, _) = bump_json_version(t, None).unwrap();
        assert_eq!(o2, "1.0.0");
    }
    #[test]
    fn diff_shows_change_and_new_file() {
        let d = unified_diff("s/SKILL.md", "a\nb\n", "a\nb\nc\n").unwrap();
        assert!(d.contains("+c") && !d.contains("-b"));
        assert!(unified_diff("n.md", "", "x\n").unwrap().contains("+x"));
        assert_eq!(unified_diff("same.md", "x\n", "x\n").unwrap(), "");
    }
    #[test]
    fn slugs() { assert_eq!(slug("Weak input validation & normalization"), "weak-input-validation-normalization"); }
}
