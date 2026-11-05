//! Findings per 1000 changed lines, by month / category / skill version.
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// Skill version a PR was made under. PRs created before the baseline cut form one group; after it, the PR-body
/// marker if the agent logged one, else the newest indexed version of the repo's skill committed before the PR.
pub fn pr_skill_expr(_conn: &Connection) -> String {
    // A PR-body marker may be a bare version ("0.4"); resolve it to the same "version#hash7" label the date-based fallback uses.
    let marker = "CASE WHEN p.skill_marker LIKE '%#%' THEN p.skill_marker ELSE COALESCE((SELECT sv.version_label||'#'||substr(sv.blob_sha,1,7) FROM skill_versions sv
        WHERE sv.skill=r.skill AND sv.version_label=p.skill_marker ORDER BY sv.committed_at DESC LIMIT 1), p.skill_marker) END";
    let tracked = &format!("COALESCE({marker}, (SELECT COALESCE(sv.version_label,'?')||'#'||substr(sv.blob_sha,1,7) FROM skill_versions sv
        WHERE sv.skill=r.skill AND sv.committed_at <= CAST(strftime('%s', p.created_at) AS INTEGER) ORDER BY sv.committed_at DESC LIMIT 1), 'none')");
    // The cut is read inside SQL, so no stored value is ever spliced into the query text. With no cut it is NULL and the CASE falls through.
    format!("CASE WHEN p.created_at < (SELECT value FROM meta WHERE key='cut_at') THEN 'baseline (pre-cut)' ELSE {tracked} END")
}

fn kloc_by(conn: &Connection, key: &str) -> Result<BTreeMap<String, (i64, f64)>> {
    let sql = format!("SELECT {key}, COUNT(*), COALESCE(SUM(p.additions+p.deletions),0) FROM prs p JOIN repos r ON r.id=p.repo_id
        WHERE p.created_at IS NOT NULL GROUP BY 1");
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)? as f64 / 1000.0)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?.into_iter().map(|(k, n, k2)| (k, (n, k2))).collect())
}

pub fn by_month(conn: &Connection, only_category: Option<&str>) -> Result<()> {
    let denom = kloc_by(conn, "substr(p.created_at,1,7)")?;
    let mut st = conn.prepare("SELECT c.name, substr(p.created_at,1,7), COUNT(*) FROM finding_categories fc
        JOIN categories c ON c.id=fc.category_id JOIN findings f ON f.id=fc.finding_id JOIN prs p ON p.id=f.pr_id
        GROUP BY 1,2")?;
    let mut cells: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
        let (c, m, n) = r?;
        cells.entry(c).or_default().insert(m, n);
    }
    let months: Vec<&String> = denom.keys().collect();
    println!("Accepted CodeRabbit findings per 1000 changed lines (additions+deletions), by PR creation month\n");
    print!("{:<46}", "category");
    for m in &months { print!("{m:>9}"); }
    println!("{:>9}", "all");
    let mut total: BTreeMap<&String, i64> = BTreeMap::new();
    let (all_kloc, ) = (denom.values().map(|d| d.1).sum::<f64>(),);
    let mut rows: Vec<(&String, i64)> = cells.iter().map(|(c, m)| (c, m.values().sum())).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    for (cat, sum) in rows {
        if only_category.map_or(false, |o| !cat.to_lowercase().contains(&o.to_lowercase())) { continue; }
        print!("{:<46}", cat.chars().take(45).collect::<String>());
        for m in &months {
            let n = cells[cat].get(*m).copied().unwrap_or(0);
            *total.entry(m).or_default() += n;
            let k = denom[*m].1;
            print!("{:>9}", if k > 0.0 { format!("{:.2}", n as f64 / k) } else { "-".into() });
        }
        println!("{:>9.2}", sum as f64 / all_kloc);
    }
    print!("{:<46}", "TOTAL");
    for m in &months { let k = denom[*m].1; print!("{:>9}", if k > 0.0 { format!("{:.2}", total.get(*m).copied().unwrap_or(0) as f64 / k) } else { "-".into() }); }
    println!("{:>9.2}", total.values().sum::<i64>() as f64 / all_kloc);
    print!("{:<46}", "kLOC changed");
    for m in &months { print!("{:>9.1}", denom[*m].1); }
    println!("{:>9.1}", all_kloc);
    print!("{:<46}", "PRs");
    for m in &months { print!("{:>9}", denom[*m].0); }
    println!("{:>9}", denom.values().map(|d| d.0).sum::<i64>());
    Ok(())
}

pub fn by_skill(conn: &Connection) -> Result<()> {
    let expr = pr_skill_expr(conn);
    let mut st = conn.prepare(&format!("SELECT r.slug, {expr} AS sv, p.id, p.additions+p.deletions, substr(p.created_at,1,10),
        (SELECT COUNT(*) FROM findings f JOIN finding_categories fc ON fc.finding_id=f.id WHERE f.pr_id=p.id)
        FROM prs p JOIN repos r ON r.id=p.repo_id WHERE p.created_at IS NOT NULL"))?;
    #[derive(Default)] struct A { prs: i64, lines: i64, findings: i64, first: String, last: String }
    let mut agg: BTreeMap<(String, String), A> = BTreeMap::new();
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?)))? {
        let (slug, sv, lines, day, f) = r?;
        let a = agg.entry((slug, sv)).or_default();
        a.prs += 1; a.lines += lines; a.findings += f;
        if a.first.is_empty() || day < a.first { a.first = day.clone(); }
        if day > a.last { a.last = day; }
    }
    println!("Accepted findings per 1000 changed lines, by skill version in force\n");
    println!("{:<30}{:<22}{:>5}{:>9}{:>10}{:>9}  {}", "repo", "skill version", "PRs", "kLOC", "findings", "per kLOC", "PR dates");
    let set: BTreeSet<_> = agg.keys().cloned().collect();
    for k in set {
        let a = &agg[&k];
        let kl = a.lines as f64 / 1000.0;
        println!("{:<30}{:<22}{:>5}{:>9.1}{:>10}{:>9.2}  {}..{}", k.0, k.1, a.prs, kl, a.findings,
            if kl > 0.0 { a.findings as f64 / kl } else { 0.0 }, a.first, a.last);
    }
    Ok(())
}
