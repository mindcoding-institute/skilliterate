//! Index a skill file's version history from a local git clone, so PRs can be attributed to the skill version in force.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use std::process::Command;

pub fn git(dir: &str, args: &[&str]) -> Result<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !o.status.success() { bail!("git {:?}: {}", args, String::from_utf8_lossy(&o.stderr)); }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Frontmatter `version:` (top-level or under `metadata:`).
pub fn frontmatter_version(text: &str) -> Option<String> {
    let fm = text.strip_prefix("---")?.split("\n---").next()?;
    fm.lines().find_map(|l| l.trim().strip_prefix("version:").map(|v| v.trim().trim_matches('"').to_string()))
}

/// One row per commit that changed the file's content (all branches).
pub fn index(conn: &Connection, clone: &str, skill: &str, file: &str) -> Result<usize> {
    for (k, v) in [(format!("clone:{skill}"), clone), (format!("file:{skill}"), file)] {
        conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![k, v])?;
    }
    let log = git(clone, &["log", "--all", "--format=%H\t%ct\t%s", "--", file])?;
    let mut n = 0;
    for line in log.lines() {
        let mut it = line.splitn(3, '\t');
        let (sha, ts, subject) = (it.next().unwrap_or(""), it.next().unwrap_or("0"), it.next().unwrap_or(""));
        // file deleted/renamed in this commit -> no blob; skip
        let Ok(blob) = git(clone, &["rev-parse", &format!("{sha}:{file}")]) else { continue };
        let text = git(clone, &["show", &format!("{sha}:{file}")]).unwrap_or_default();
        n += conn.execute(
            "INSERT OR REPLACE INTO skill_versions(skill,file,commit_sha,blob_sha,committed_at,version_label,subject) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![skill, file, sha, blob.trim(), ts.parse::<i64>()?, frontmatter_version(&text), subject])?;
    }
    Ok(n)
}

/// Index the skill's history on its base branch through the GitHub API (no clone needed).
/// One row per commit that changed the file; `blob_sha` is the content identity.
pub fn index_api(conn: &Connection, s: &skilliterate_core::config::SkillCfg) -> Result<usize> {
    use skilliterate_gh::api;
    conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![format!("file:{}", s.name), s.path])?;
    let mut n = 0;
    for c in api::path_history(&s.repo, &s.path, &s.base)? {
        let (Some(blob), Some(text)) = (api::blob_sha(&s.repo, &s.path, &c.sha)?, api::file_at(&s.repo, &s.path, &c.sha)?) else { continue };
        n += conn.execute(
            "INSERT OR REPLACE INTO skill_versions(skill,file,commit_sha,blob_sha,committed_at,version_label,subject) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![s.name, s.path, c.sha, blob, c.epoch, frontmatter_version(&text), c.subject])?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fm_version() {
        assert_eq!(frontmatter_version("---\nname: x\nmetadata:\n  version: \"0.6\"\n---\nbody").as_deref(), Some("0.6"));
        assert_eq!(frontmatter_version("no frontmatter"), None);
    }
}
