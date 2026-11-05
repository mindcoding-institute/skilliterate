//! Skill adjustments: which edits we made to a skill, which mistake category each targets, and whether it worked.
use anyhow::{bail, Result};
use rusqlite::{params, Connection};

pub fn set_cut(conn: &Connection, at: Option<String>) -> Result<()> {
    let at = match at {
        Some(a) => a,
        None => conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ','now')", [], |r| r.get(0))?,
    };
    conn.execute("INSERT INTO meta(key,value) VALUES('cut_at',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&at])?;
    let (n, k): (i64, f64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(additions+deletions),0)/1000.0 FROM prs WHERE created_at < ?1", [&at], |r| Ok((r.get(0)?, r.get(1)?)))?;
    println!("baseline cut at {at}: {n} PRs, {k:.1} kLOC before it form the baseline");
    Ok(())
}

pub fn show_cut(conn: &Connection) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key='cut_at'", [], |r| r.get(0)).ok())
}

pub fn resolve_category(conn: &Connection, q: &str) -> Result<i64> {
    if let Ok(id) = q.parse::<i64>() { if conn.query_row("SELECT COUNT(*) FROM categories WHERE id=?1", [id], |r| r.get::<_, i64>(0))? > 0 { return Ok(id); } }
    let mut st = conn.prepare("SELECT id,name FROM categories WHERE name LIKE '%'||?1||'%' AND status!='retired'")?;
    let hits: Vec<(i64, String)> = st.query_map([q], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    match hits.len() {
        1 => Ok(hits[0].0),
        0 => bail!("no category matches '{q}'"),
        _ => bail!("'{q}' is ambiguous: {}", hits.iter().map(|h| format!("[{}] {}", h.0, h.1)).collect::<Vec<_>>().join("; ")),
    }
}

pub fn add(conn: &Connection, skill: &str, category: Option<String>, summary: &str, instruction: Option<String>, version: Option<String>, pr: Option<String>) -> Result<()> {
    let cat = category.map(|c| resolve_category(conn, &c)).transpose()?;
    conn.execute("INSERT INTO skill_adjustments(skill,category_id,summary,instruction,version_label,pr_url) VALUES(?1,?2,?3,?4,?5,?6)",
        params![skill, cat, summary, instruction, version, pr])?;
    println!("adjustment #{} recorded (proposed)", conn.last_insert_rowid());
    Ok(())
}

/// Mark shipped. Ship time = --at (ISO) or, if the target version is indexed, that version's commit time.
pub fn ship(conn: &Connection, id: i64, version: Option<String>, at: Option<String>, pr: Option<String>) -> Result<()> {
    let (skill, cur_version): (String, Option<String>) = conn.query_row("SELECT skill,version_label FROM skill_adjustments WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let version = version.or(cur_version);
    let indexed: Option<(i64, String)> = version.as_ref().and_then(|v| conn.query_row(
        "SELECT committed_at, blob_sha FROM skill_versions WHERE skill=?1 AND version_label=?2 ORDER BY committed_at DESC LIMIT 1",
        params![skill, v], |r| Ok((r.get(0)?, r.get(1)?))).ok());
    let at_epoch: Option<i64> = match &at {
        Some(a) => Some(conn.query_row("SELECT CAST(strftime('%s', ?1) AS INTEGER)", [a], |r| r.get::<_, Option<i64>>(0))?.ok_or_else(|| anyhow::anyhow!("bad --at '{a}' (use ISO, e.g. 2026-11-06T09:00:00Z)"))?),
        None => indexed.as_ref().map(|i| i.0),
    };
    let Some(ts) = at_epoch else { bail!("can't determine ship time: version not indexed (run `skilliterate skills index`) — pass --at") };
    conn.execute("UPDATE skill_adjustments SET status='shipped', shipped_at=?2, version_label=?3, blob_sha=COALESCE(?4,blob_sha), pr_url=COALESCE(?5,pr_url) WHERE id=?1",
        params![id, ts, version, indexed.map(|i| i.1), pr])?;
    println!("adjustment #{id} shipped");
    Ok(())
}

pub fn list(conn: &Connection) -> Result<()> {
    let mut st = conn.prepare("SELECT a.id,a.skill,COALESCE(c.name,'(instrumentation)'),a.summary,a.status,COALESCE(a.version_label,'-'),
        COALESCE(date(a.shipped_at,'unixepoch'),'-'),COALESCE(a.pr_url,'') FROM skill_adjustments a LEFT JOIN categories c ON c.id=a.category_id ORDER BY a.id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
        r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?)))? {
        let (id, skill, cat, sum, status, ver, shipped, pr) = r?;
        println!("#{id} [{status}] {skill} v{ver} shipped:{shipped}  → {cat}\n    {sum}{}", if pr.is_empty() { String::new() } else { format!("\n    {pr}") });
    }
    Ok(())
}

/// Per shipped adjustment: the target category's rate per kLOC in the baseline vs. PRs created after it shipped.
pub fn effect(conn: &Connection) -> Result<()> {
    let Some(cut) = show_cut(conn)? else { bail!("no baseline cut set; run `skilliterate cut set`") };
    let mut st = conn.prepare("SELECT a.id,a.skill,a.category_id,c.name,a.summary,a.version_label,a.shipped_at FROM skill_adjustments a
        JOIN categories c ON c.id=a.category_id WHERE a.status='shipped' ORDER BY a.shipped_at")?;
    let adj: Vec<(i64, String, i64, String, String, Option<String>, i64)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?;
    if adj.is_empty() { println!("no shipped category adjustments yet"); return Ok(()); }
    let window = |cond: &str, cat: i64, skill: &str, bind: &dyn rusqlite::ToSql| -> Result<(i64, f64, i64)> {
        let sql = format!("SELECT COUNT(*), COALESCE(SUM(p.additions+p.deletions),0)/1000.0,
            (SELECT COUNT(*) FROM finding_categories fc JOIN findings f ON f.id=fc.finding_id WHERE fc.category_id={cat} AND f.pr_id IN
               (SELECT p.id FROM prs p JOIN repos r ON r.id=p.repo_id WHERE r.skill=?1 AND p.created_at IS NOT NULL AND {cond}))
            FROM prs p JOIN repos r ON r.id=p.repo_id WHERE r.skill=?1 AND p.created_at IS NOT NULL AND {cond}");
        Ok(conn.query_row(&sql, params![skill, bind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    };
    println!("{:<4}{:<42}{:<9}{:>16}{:>18}{:>9}  note", "#", "category (adjustment)", "version", "baseline /kLOC", "after /kLOC", "change");
    for (id, skill, cat, name, summary, ver, shipped) in adj {
        let (bp, bk, bf) = window("p.created_at < ?2", cat, &skill, &cut)?;
        let (ap, ak, af) = window("CAST(strftime('%s', p.created_at) AS INTEGER) >= ?2", cat, &skill, &shipped)?;
        let (br, ar) = (if bk > 0.0 { bf as f64 / bk } else { 0.0 }, if ak > 0.0 { af as f64 / ak } else { 0.0 });
        let change = if br > 0.0 && ak > 0.0 { format!("{:+.0}%", (ar - br) / br * 100.0) } else { "-".into() };
        let note = if ap < 10 || ak < 10.0 { format!("too early ({ap} PRs, {ak:.1} kLOC after; want ≥10 PRs and ≥10 kLOC)") } else { String::new() };
        println!("{:<4}{:<42}{:<9}{:>9.2} ({:>3}){:>11.2} ({:>3}){:>9}  {}", id, name.chars().take(40).collect::<String>(),
            ver.unwrap_or_default(), br, bf, ar, af, change, note);
        println!("    {summary}  [baseline {bp} PRs / {bk:.1} kLOC]");
    }
    Ok(())
}
