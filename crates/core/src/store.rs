use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos(
  id INTEGER PRIMARY KEY, slug TEXT UNIQUE NOT NULL, role TEXT NOT NULL DEFAULT 'source');
CREATE TABLE IF NOT EXISTS prs(
  id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL REFERENCES repos(id), number INTEGER NOT NULL,
  title TEXT, state TEXT, merged_at TEXT, UNIQUE(repo_id, number));
CREATE TABLE IF NOT EXISTS findings(
  id INTEGER PRIMARY KEY, pr_id INTEGER NOT NULL REFERENCES prs(id), thread_id TEXT UNIQUE NOT NULL,
  path TEXT, line INTEGER, severity TEXT, cr_category TEXT, effort TEXT, title TEXT, cr_kind TEXT,
  body TEXT NOT NULL, diff_hunk TEXT, is_resolved INTEGER, is_outdated INTEGER, thread_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS verdicts(
  finding_id INTEGER PRIMARY KEY REFERENCES findings(id), verdict TEXT NOT NULL,
  source TEXT NOT NULL, confidence REAL, evidence TEXT);
CREATE TABLE IF NOT EXISTS pr_patches(
  pr_id INTEGER NOT NULL REFERENCES prs(id), path TEXT NOT NULL, patch TEXT, PRIMARY KEY(pr_id, path));
CREATE TABLE IF NOT EXISTS pr_patches_fetched(pr_id INTEGER PRIMARY KEY REFERENCES prs(id));
CREATE TABLE IF NOT EXISTS root_causes(
  finding_id INTEGER PRIMARY KEY REFERENCES findings(id), what_went_wrong TEXT NOT NULL,
  mistake_type TEXT, preventing_instruction TEXT, skill_relevant INTEGER, model TEXT, prompt_version INTEGER);
CREATE TABLE IF NOT EXISTS categories(
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, description TEXT, boundary TEXT,
  skill_preventable INTEGER, status TEXT NOT NULL DEFAULT 'proposed');
CREATE TABLE IF NOT EXISTS finding_categories(
  finding_id INTEGER PRIMARY KEY REFERENCES findings(id), category_id INTEGER NOT NULL REFERENCES categories(id),
  model TEXT, confidence REAL);
CREATE TABLE IF NOT EXISTS skill_versions(
  id INTEGER PRIMARY KEY, skill TEXT NOT NULL, file TEXT NOT NULL, commit_sha TEXT NOT NULL, blob_sha TEXT NOT NULL,
  committed_at INTEGER NOT NULL, version_label TEXT, subject TEXT, UNIQUE(skill, commit_sha));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS skill_adjustments(
  id INTEGER PRIMARY KEY, skill TEXT NOT NULL, category_id INTEGER REFERENCES categories(id),
  summary TEXT NOT NULL, instruction TEXT, version_label TEXT, blob_sha TEXT, pr_url TEXT,
  status TEXT NOT NULL DEFAULT 'proposed', created_at TEXT DEFAULT CURRENT_TIMESTAMP, shipped_at INTEGER);
CREATE TABLE IF NOT EXISTS adjustment_evidence(
  adjustment_id INTEGER NOT NULL REFERENCES skill_adjustments(id), finding_id INTEGER NOT NULL REFERENCES findings(id),
  PRIMARY KEY(adjustment_id, finding_id));
CREATE TABLE IF NOT EXISTS llm_log(
  id INTEGER PRIMARY KEY, stage TEXT, model TEXT, finding_id INTEGER, cost_usd REAL, created_at TEXT DEFAULT CURRENT_TIMESTAMP);
"#;

/// Create a new database. Fails if the file already exists, so a scaffold never overwrites data.
pub fn create(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    if path.exists() { anyhow::bail!("{} already exists; refusing to overwrite it", path.display()); }
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) { std::fs::create_dir_all(p)?; }
    prepare(Connection::open(path)?)
}

/// Open an existing database. Fails if it is missing (run `skilliterate init` first).
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let path = path.as_ref();
    if !path.is_file() { anyhow::bail!("database {} not found; run `skilliterate init` first", path.display()); }
    prepare(Connection::open(path)?)
}

fn prepare(c: Connection) -> Result<Connection> {
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.execute_batch(SCHEMA)?;
    // additive migrations for databases created by earlier versions
    for (table, col, ty) in [
        ("prs", "created_at", "TEXT"), ("prs", "additions", "INTEGER"), ("prs", "deletions", "INTEGER"),
        ("prs", "changed_files", "INTEGER"), ("prs", "skill_marker", "TEXT"), ("repos", "skill", "TEXT"), ("skill_adjustments", "branch", "TEXT"), ("skill_adjustments", "worktree", "TEXT"),
    ] {
        let has: i64 = c.query_row(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name='{col}'"), [], |r| r.get(0))?;
        if has == 0 { c.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {ty}"), [])?; }
    }
    Ok(c)
}
