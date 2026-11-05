//! LLM-backed pipeline stages: verdict escalation and root-cause analysis.
use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use skilliterate_llm::{par_for_each, Claude, OpenRouter};

const ROOTCAUSE_PROMPT_VERSION: i64 = 1;

struct Row {
    id: i64, pr_id: i64, pr_number: i64, repo: String, path: Option<String>,
    title: Option<String>, body: String, thread_json: String, patch: Option<String>,
}

fn trim(s: &str, n: usize) -> String { s.chars().take(n).collect() }

/// Drop CodeRabbit's boilerplate (agent prompt, analysis chains) to keep prompts small.
fn clean_body(b: &str) -> String {
    let cut = ["<!-- ai-agent-prompt:start -->", "<summary>🧩 Analysis chain", "<summary>Analysis chain"]
        .iter().filter_map(|m| b.find(m)).min().unwrap_or(b.len());
    trim(&b[..cut], 3500)
}

fn human_replies(thread_json: &str) -> String {
    let v: Vec<Value> = serde_json::from_str(thread_json).unwrap_or_default();
    v.iter().skip(1)
        .map(|c| format!("[{}] {}", c["author"]["login"].as_str().unwrap_or("?"), trim(c["body"].as_str().unwrap_or(""), 700)))
        .collect::<Vec<_>>().join("\n---\n")
}

fn load(conn: &Connection, sql: &str) -> Result<Vec<Row>> {
    let mut st = conn.prepare(sql)?;
    let rows = st.query_map([], |r| Ok(Row {
        id: r.get(0)?, pr_id: r.get(1)?, pr_number: r.get(2)?, repo: r.get(3)?, path: r.get(4)?,
        title: r.get(5)?, body: r.get(6)?, thread_json: r.get(7)?, patch: None,
    }))?.collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn attach_patches(conn: &Connection, rows: &mut [Row]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for r in rows.iter() {
        if seen.insert(r.pr_id) {
            if let Err(e) = skilliterate_gh::ensure_patches(conn, &r.repo, r.pr_id, r.pr_number) {
                eprintln!("patch fetch failed for {}#{}: {e}", r.repo, r.pr_number);
            }
        }
    }
    for r in rows.iter_mut() {
        r.patch = conn.query_row("SELECT patch FROM pr_patches WHERE pr_id=?1 AND path=?2", params![r.pr_id, r.path], |x| x.get(0)).ok();
    }
    Ok(())
}

const BASE_SELECT: &str = "SELECT f.id,f.pr_id,p.number,r.slug,f.path,f.title,f.body,f.thread_json FROM findings f
    JOIN prs p ON p.id=f.pr_id JOIN repos r ON r.id=p.repo_id JOIN verdicts v ON v.finding_id=f.id";

fn log(conn: &Connection, stage: &str, model: &str, id: i64, cost: f64) {
    let _ = conn.execute("INSERT INTO llm_log(stage,model,finding_id,cost_usd) VALUES(?1,?2,?3,?4)", params![stage, model, id, cost]);
}

/// Escalate verdicts the rules couldn't settle. Short threads -> Haiku; "outdated, no reply" -> Sonnet with the final file patch.
pub fn verdict_llm(conn: &Connection, limit: usize, workers: usize) -> Result<()> {
    let mut rows = load(conn, &format!("{BASE_SELECT} WHERE v.source IN ('rule:none','rule:outdated-no-reply') ORDER BY f.id LIMIT {limit}"))?;
    attach_patches(conn, &mut rows)?;
    let schema = json!({"type":"object","properties":{
        "verdict":{"type":"string","enum":["accepted","rebutted","ignored"]},
        "confidence":{"type":"number"},"reason":{"type":"string"}},
        "required":["verdict","confidence","reason"]});
    let system = "You judge whether a code-review comment from the CodeRabbit bot was ACCEPTED (the author changed code to address it), REBUTTED (the author explicitly disagreed or explained why not), or IGNORED (nothing was done). Judge from the thread and the PR's final patch for that file. Be strict: 'accepted' requires evidence in the thread or the final patch that the concern was addressed.";
    let work = |r: &Row| {
        let model = if r.patch.is_some() { "sonnet" } else { "haiku" };
        let prompt = format!("COMMENT TITLE: {}\nFILE: {}\n\nCOMMENT:\n{}\n\nREPLIES:\n{}\n\nFINAL PATCH OF THIS FILE IN THE PR:\n{}",
            r.title.clone().unwrap_or_default(), r.path.clone().unwrap_or_default(), clean_body(&r.body),
            human_replies(&r.thread_json), trim(r.patch.as_deref().unwrap_or("(not available)"), 9000));
        (model, Claude::new(model).run_json(system, &prompt, &schema))
    };
    let (mut ok, mut failed, mut cost) = (0, 0, 0.0);
    par_for_each(&rows, workers, work, |i, (model, res)| match res {
        Ok(rep) => {
            let v = &rep.value;
            let _ = conn.execute("UPDATE verdicts SET verdict=?2,source=?3,confidence=?4,evidence=?5 WHERE finding_id=?1",
                params![rows[i].id, v["verdict"].as_str(), format!("llm:{model}"), v["confidence"].as_f64(), v["reason"].as_str()]);
            log(conn, "verdict", model, rows[i].id, rep.cost_usd);
            ok += 1; cost += rep.cost_usd;
        }
        Err(e) => { failed += 1; eprintln!("finding {}: {e}", rows[i].id); }
    });
    println!("verdict-llm: {ok} ok, {failed} failed, ${cost:.3}");
    Ok(())
}

/// For accepted findings: what was done wrong, and what instruction would have prevented it.
pub fn root_cause(conn: &Connection, limit: usize, workers: usize, min_conf: f64) -> Result<()> {
    let mut rows = load(conn, &format!("{BASE_SELECT} LEFT JOIN root_causes rc ON rc.finding_id=f.id
        WHERE v.verdict='accepted' AND v.confidence>={min_conf} AND rc.finding_id IS NULL ORDER BY f.id LIMIT {limit}"))?;
    attach_patches(conn, &mut rows)?;
    let schema = json!({"type":"object","properties":{
        "what_went_wrong":{"type":"string"},"mistake_type":{"type":"string"},
        "preventing_instruction":{"type":"string"},"skill_relevant":{"type":"boolean"}},
        "required":["what_went_wrong","mistake_type","preventing_instruction","skill_relevant"]});
    let system = "You study code-review findings to find the root cause of mistakes made by a developer working with an AI coding agent. The finding below was ACCEPTED: the author fixed it, so it was a real problem in their change. Answer: what did the author do wrong? Describe the class of mistake (a decision, an omission, a wrong assumption), not just the symptom, in 1-2 sentences, without file-specific detail. mistake_type: a short 2-5 word label for the class. preventing_instruction: ONE imperative, checkable sentence that a pre-PR checklist in a coding-workflow skill could state to prevent this class of mistake. skill_relevant: true if a generic workflow checklist could plausibly prevent it, false if it needed project-specific knowledge or was a one-off slip.";
    let work = |r: &Row| {
        let prompt = format!("COMMENT TITLE: {}\nFILE: {}\n\nREVIEW COMMENT:\n{}\n\nAUTHOR REPLY:\n{}\n\nFINAL PATCH OF THIS FILE IN THE PR:\n{}",
            r.title.clone().unwrap_or_default(), r.path.clone().unwrap_or_default(), clean_body(&r.body),
            human_replies(&r.thread_json), trim(r.patch.as_deref().unwrap_or("(not available)"), 7000));
        Claude::new("sonnet").run_json(system, &prompt, &schema)
    };
    let (mut ok, mut failed, mut cost) = (0, 0, 0.0);
    par_for_each(&rows, workers, work, |i, res| match res {
        Ok(rep) => {
            let v = &rep.value;
            let _ = conn.execute("INSERT OR REPLACE INTO root_causes(finding_id,what_went_wrong,mistake_type,preventing_instruction,skill_relevant,model,prompt_version) VALUES(?1,?2,?3,?4,?5,'sonnet',?6)",
                params![rows[i].id, v["what_went_wrong"].as_str(), v["mistake_type"].as_str(), v["preventing_instruction"].as_str(), v["skill_relevant"].as_bool(), ROOTCAUSE_PROMPT_VERSION]);
            log(conn, "rootcause", "sonnet", rows[i].id, rep.cost_usd);
            ok += 1; cost += rep.cost_usd;
        }
        Err(e) => { failed += 1; eprintln!("finding {}: {e}", rows[i].id); }
    });
    println!("rootcause: {ok} ok, {failed} failed, ${cost:.3}");
    Ok(())
}

/// Sonnet reads every root cause and proposes a category taxonomy (status = proposed, awaiting human approval).
pub fn taxonomy_propose(conn: &Connection) -> Result<()> {
    let mut st = conn.prepare("SELECT rc.finding_id, rc.mistake_type, rc.preventing_instruction FROM root_causes rc ORDER BY rc.finding_id")?;
    let lines: Vec<String> = st.query_map([], |r| Ok(format!("{} | {} | {}", r.get::<_, i64>(0)?,
        r.get::<_, String>(1).unwrap_or_default(), trim(&r.get::<_, String>(2).unwrap_or_default(), 170))))?
        .collect::<Result<_, _>>()?;
    let schema = json!({"type":"object","properties":{"categories":{"type":"array","items":{"type":"object","properties":{
        "name":{"type":"string"},"description":{"type":"string"},"boundary":{"type":"string"},"skill_preventable":{"type":"boolean"}},
        "required":["name","description","boundary","skill_preventable"]}}},"required":["categories"]});
    let system = "You design taxonomies. You are given root-cause records of real code-review findings (one per line: id | free-text mistake label | instruction that would have prevented it). Create 12-20 categories that group COMMON MISTAKES AND BAD DECISIONS made by developers working with an AI coding agent, so counting findings per category shows which habits to fix first. Rules: categories are about what the author did wrong (the decision/omission), not the code area or file type; mutually exclusive and collectively exhaustive (include one 'Other / one-off' category); each should cover roughly 3-15% of the records, none above 25%; names are short noun phrases; description says what mistakes belong; boundary says how it differs from its nearest neighbour category; skill_preventable is true when a generic pre-PR checklist in a workflow skill could plausibly prevent most of its findings.";
    let prompt = format!("{} root-cause records:\n\n{}", lines.len(), lines.join("\n"));
    let rep = Claude::new("sonnet").run_json(system, &prompt, &schema)?;
    conn.execute("DELETE FROM categories WHERE status='proposed'", [])?;
    for c in rep.value["categories"].as_array().cloned().unwrap_or_default() {
        conn.execute("INSERT OR REPLACE INTO categories(name,description,boundary,skill_preventable,status) VALUES(?1,?2,?3,?4,'proposed')",
            params![c["name"].as_str(), c["description"].as_str(), c["boundary"].as_str(), c["skill_preventable"].as_bool()])?;
    }
    log(conn, "taxonomy", "sonnet", 0, rep.cost_usd);
    println!("proposed {} categories (${:.3})", rep.value["categories"].as_array().map_or(0, |a| a.len()), rep.cost_usd);
    Ok(())
}

pub fn taxonomy_show(conn: &Connection) -> Result<()> {
    let mut st = conn.prepare("SELECT id,name,status,skill_preventable,description,boundary FROM categories ORDER BY id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<bool>>(3)?,
        r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?)))? {
        let (id, name, status, sp, d, b) = r?;
        println!("[{id}] {name}  ({status}{})\n    {}\n    vs neighbour: {}\n", if sp == Some(true) { ", skill-preventable" } else { "" },
            d.unwrap_or_default(), b.unwrap_or_default());
    }
    Ok(())
}

const JEV_MODEL: &str = "typesafe/jev-router";

/// Jev assigns every root-caused finding to one approved category.
pub fn categorize(conn: &Connection, limit: usize, workers: usize, recategorize: bool) -> Result<()> {
    let cats: Vec<(i64, String, String, String)> = conn.prepare("SELECT id,name,COALESCE(description,''),COALESCE(boundary,'') FROM categories WHERE status='approved' ORDER BY id")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
    if cats.is_empty() { anyhow::bail!("no approved categories; run `skilliterate taxonomy approve`"); }
    if recategorize { conn.execute("DELETE FROM finding_categories", [])?; }
    let ids: Vec<i64> = cats.iter().map(|c| c.0).collect();
    let catlist = cats.iter().map(|(id, n, d, b)| format!("{id}: {n}\n   {d}\n   Not to be confused with: {b}")).collect::<Vec<_>>().join("\n");
    let system = format!("Classify a code-review finding by the root-cause MISTAKE the author made, into exactly one category. Use the 'Other / one-off' category only if no other fits. Categories:\n{catlist}");
    let schema = json!({"type":"object","properties":{"category_id":{"type":"integer","enum":ids},"confidence":{"type":"number"}},
        "required":["category_id","confidence"],"additionalProperties":false});

    let mut st = conn.prepare(&format!("SELECT f.id,f.title,COALESCE(f.cr_category,''),rc.mistake_type,rc.what_went_wrong,rc.preventing_instruction
        FROM root_causes rc JOIN findings f ON f.id=rc.finding_id LEFT JOIN finding_categories fc ON fc.finding_id=f.id
        WHERE fc.finding_id IS NULL ORDER BY f.id LIMIT {limit}"))?;
    let rows: Vec<(i64, String)> = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, format!(
        "Review title: {}\nCodeRabbit category: {}\nMistake label: {}\nWhat went wrong: {}\nPreventing instruction: {}",
        r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))))?
        .collect::<Result<_, _>>()?;
    let jev = OpenRouter::from_env()?;
    let work = |r: &(i64, String)| jev.run_json(JEV_MODEL, &system, &r.1, "categorization", &schema);
    let (mut ok, mut failed, mut cost) = (0, 0, 0.0);
    par_for_each(&rows, workers, work, |i, res| match res {
        Ok(rep) => {
            let _ = conn.execute("INSERT OR REPLACE INTO finding_categories(finding_id,category_id,model,confidence) VALUES(?1,?2,?3,?4)",
                params![rows[i].0, rep.value["category_id"].as_i64(), JEV_MODEL, rep.value["confidence"].as_f64()]);
            log(conn, "categorize", JEV_MODEL, rows[i].0, rep.cost_usd);
            ok += 1; cost += rep.cost_usd;
        }
        Err(e) => { failed += 1; eprintln!("finding {}: {e}", rows[i].0); }
    });
    println!("categorize: {ok} ok, {failed} failed, ${cost:.4}");
    Ok(())
}
