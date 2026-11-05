//! GitHub access via the `gh` CLI (reuses its auth; no token handling here).
pub mod api;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use skilliterate_core::{cr, verdict};
use std::process::Command;

const QUERY: &str = r#"
query($owner:String!,$name:String!,$endCursor:String){
  repository(owner:$owner,name:$name){
    pullRequests(first:25,after:$endCursor,orderBy:{field:CREATED_AT,direction:DESC}){
      pageInfo{hasNextPage endCursor}
      nodes{ number title state mergedAt createdAt additions deletions changedFiles body
        reviewThreads(first:100){ nodes{ id isResolved isOutdated path line
          comments(first:30){ nodes{ author{login} body diffHunk } } } } } } } }"#;

fn si_valid_slug(s: &str) -> bool { skilliterate_core::config::valid_slug(s) }

pub struct IngestStats { pub prs: usize, pub findings: usize }

pub fn ingest(conn: &Connection, slug: &str, role: &str, bot: &str) -> Result<IngestStats> {
    if !si_valid_slug(slug) { bail!("invalid repo '{slug}': expected owner/name"); }
    let (owner, name) = slug.split_once('/').context("slug must be owner/name")?;
    let out = Command::new("gh")
        .args(["api", "graphql", "--paginate", "--slurp", "-f", &format!("query={QUERY}"),
               "-f", &format!("owner={owner}"), "-f", &format!("name={name}")])
        .output().context("running gh")?;
    if !out.status.success() { bail!("gh failed: {}", String::from_utf8_lossy(&out.stderr)); }
    let pages: Vec<Value> = serde_json::from_slice(&out.stdout)?;

    conn.execute("INSERT INTO repos(slug,role) VALUES(?1,?2) ON CONFLICT(slug) DO UPDATE SET role=excluded.role", params![slug, role])?;
    let repo_id: i64 = conn.query_row("SELECT id FROM repos WHERE slug=?1", [slug], |r| r.get(0))?;

    let (mut prs, mut findings) = (0, 0);
    let tx = conn.unchecked_transaction()?;
    for page in &pages {
        for pr in page["data"]["repository"]["pullRequests"]["nodes"].as_array().into_iter().flatten() {
            prs += 1;
            tx.execute(
                "INSERT INTO prs(repo_id,number,title,state,merged_at,created_at,additions,deletions,changed_files,skill_marker) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(repo_id,number) DO UPDATE SET title=excluded.title,state=excluded.state,merged_at=excluded.merged_at,
                   created_at=excluded.created_at,additions=excluded.additions,deletions=excluded.deletions,
                   changed_files=excluded.changed_files,skill_marker=excluded.skill_marker",
                params![repo_id, pr["number"].as_i64(), pr["title"].as_str(), pr["state"].as_str(), pr["mergedAt"].as_str(),
                        pr["createdAt"].as_str(), pr["additions"].as_i64(), pr["deletions"].as_i64(), pr["changedFiles"].as_i64(),
                        cr::skill_marker(pr["body"].as_str().unwrap_or_default())])?;
            let pr_id: i64 = tx.query_row("SELECT id FROM prs WHERE repo_id=?1 AND number=?2", params![repo_id, pr["number"].as_i64()], |r| r.get(0))?;
            for t in pr["reviewThreads"]["nodes"].as_array().into_iter().flatten() {
                let cs = t["comments"]["nodes"].as_array().cloned().unwrap_or_default();
                let Some(first) = cs.first() else { continue };
                if first["author"]["login"].as_str() != Some(bot) { continue; }
                let body = first["body"].as_str().unwrap_or_default();
                let h = cr::parse_header(body);
                findings += 1;
                tx.execute(
                    "INSERT INTO findings(pr_id,thread_id,path,line,severity,cr_category,effort,title,cr_kind,body,diff_hunk,is_resolved,is_outdated,thread_json)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                     ON CONFLICT(thread_id) DO UPDATE SET is_resolved=excluded.is_resolved,is_outdated=excluded.is_outdated,thread_json=excluded.thread_json",
                    params![pr_id, t["id"].as_str(), t["path"].as_str(), t["line"].as_i64(), h.severity, h.category, h.effort, h.title, h.kind,
                            body, first["diffHunk"].as_str(), t["isResolved"].as_bool(), t["isOutdated"].as_bool(), serde_json::to_string(&cs)?])?;
            }
        }
    }
    tx.commit()?;
    Ok(IngestStats { prs, findings })
}

/// Apply rule-based verdicts to findings that don't have one yet.
pub fn run_verdicts(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT f.id,f.thread_json,f.is_resolved,f.is_outdated FROM findings f LEFT JOIN verdicts v ON v.finding_id=f.id WHERE v.finding_id IS NULL")?;
    let rows: Vec<(i64, String, bool, bool)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
    for (id, json, resolved, outdated) in &rows {
        let raw: Vec<Value> = serde_json::from_str(json)?;
        let thread: Vec<verdict::Comment> = raw.iter().map(|c| verdict::Comment {
            author: c["author"]["login"].as_str().unwrap_or("ghost").to_string(),
            body: c["body"].as_str().unwrap_or_default().to_string() }).collect();
        let v = verdict::decide(&thread, *resolved, *outdated);
        conn.execute("INSERT INTO verdicts(finding_id,verdict,source,confidence,evidence) VALUES(?1,?2,?3,?4,?5)",
            params![id, v.verdict, v.source, v.confidence, v.evidence])?;
    }
    Ok(rows.len())
}

/// Recompute header fields from stored bodies (use after parser changes; no refetch).
pub fn reparse_headers(conn: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String)> = conn.prepare("SELECT id, body FROM findings")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let tx = conn.unchecked_transaction()?;
    for (id, body) in &rows {
        let h = cr::parse_header(body);
        tx.execute("UPDATE findings SET severity=?2,cr_category=?3,effort=?4,title=?5,cr_kind=?6 WHERE id=?1",
            params![id, h.severity, h.category, h.effort, h.title, h.kind])?;
    }
    tx.commit()?;
    Ok(rows.len())
}

/// Fetch and store the final per-file patches of a PR (once).
pub fn ensure_patches(conn: &Connection, slug: &str, pr_id: i64, number: i64) -> Result<()> {
    if !si_valid_slug(slug) { bail!("invalid repo '{slug}': expected owner/name"); }
    if conn.query_row("SELECT COUNT(*) FROM pr_patches_fetched WHERE pr_id=?1", [pr_id], |r| r.get::<_, i64>(0))? > 0 {
        return Ok(());
    }
    let out = Command::new("gh")
        .args(["api", &format!("repos/{slug}/pulls/{number}/files"), "--paginate", "--jq", ".[] | {filename, patch}"])
        .output().context("running gh")?;
    if !out.status.success() { bail!("gh failed: {}", String::from_utf8_lossy(&out.stderr)); }
    let tx = conn.unchecked_transaction()?;
    for line in String::from_utf8_lossy(&out.stdout).lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line)?;
        tx.execute("INSERT OR REPLACE INTO pr_patches(pr_id,path,patch) VALUES(?1,?2,?3)",
            params![pr_id, v["filename"].as_str(), v["patch"].as_str()])?;
    }
    tx.execute("INSERT OR IGNORE INTO pr_patches_fetched(pr_id) VALUES(?1)", [pr_id])?;
    tx.commit()?;
    Ok(())
}
