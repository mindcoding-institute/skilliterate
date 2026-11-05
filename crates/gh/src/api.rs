//! Thin read-only GitHub helpers over `gh api` (reuses its auth). Nothing here writes to GitHub.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::process::Command;

fn run(args: &[&str]) -> Result<String> {
    let o = Command::new("gh").args(args).output().context("running gh (is it installed?)")?;
    if !o.status.success() { bail!("gh {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()); }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn slug_ok(slug: &str) -> Result<()> {
    if skilliterate_core::config::valid_slug(slug) { Ok(()) } else { bail!("invalid repo '{slug}': expected owner/name") }
}

pub fn get(path: &str) -> Result<Value> { Ok(serde_json::from_str(&run(&["api", path])?)?) }

pub fn auth_ok() -> Result<String> {
    let o = Command::new("gh").args(["api", "user", "--jq", ".login"]).output().context("gh is not installed")?;
    if !o.status.success() { bail!("gh is not logged in: {}", String::from_utf8_lossy(&o.stderr).trim()); }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub struct RepoInfo { pub default_branch: String, pub can_push: bool }
pub fn repo(slug: &str) -> Result<RepoInfo> {
    slug_ok(slug)?;
    let v = get(&format!("repos/{slug}"))?;
    Ok(RepoInfo { default_branch: v["default_branch"].as_str().unwrap_or("main").into(), can_push: v["permissions"]["push"].as_bool().unwrap_or(false) })
}

/// Raw file text at `git_ref`; `None` if the file does not exist there.
pub fn file_at(slug: &str, path: &str, git_ref: &str) -> Result<Option<String>> {
    slug_ok(slug)?;
    let o = Command::new("gh").args(["api", "-H", "Accept: application/vnd.github.raw+json", &format!("repos/{slug}/contents/{path}?ref={git_ref}")])
        .output().context("running gh")?;
    if o.status.success() { return Ok(Some(String::from_utf8_lossy(&o.stdout).into_owned())); }
    let err = String::from_utf8_lossy(&o.stderr);
    if err.contains("404") || err.contains("Not Found") { Ok(None) } else { bail!("reading {path}@{git_ref}: {}", err.trim()) }
}

/// Git blob sha of the file at `git_ref` (the identity of its content).
pub fn blob_sha(slug: &str, path: &str, git_ref: &str) -> Result<Option<String>> {
    slug_ok(slug)?;
    match get(&format!("repos/{slug}/contents/{path}?ref={git_ref}")) {
        Ok(v) => Ok(v["sha"].as_str().map(String::from)),
        Err(e) if e.to_string().contains("404") => Ok(None),
        Err(e) => Err(e),
    }
}

pub struct Commit { pub sha: String, pub epoch: i64, pub subject: String }

/// Commits on `branch` that touched `path`, newest first.
pub fn path_history(slug: &str, path: &str, branch: &str) -> Result<Vec<Commit>> {
    slug_ok(slug)?;
    let out = run(&["api", "--paginate", &format!("repos/{slug}/commits?sha={branch}&path={path}&per_page=100"),
        "--jq", ".[] | {sha, date: .commit.committer.date, msg: .commit.message}"])?;
    let mut v = Vec::new();
    for line in out.lines().filter(|l| !l.trim().is_empty()) {
        let c: Value = serde_json::from_str(line)?;
        v.push(Commit { sha: c["sha"].as_str().unwrap_or("").into(), epoch: iso_epoch(c["date"].as_str().unwrap_or("")).unwrap_or(0),
            subject: c["msg"].as_str().unwrap_or("").lines().next().unwrap_or("").into() });
    }
    Ok(v)
}

/// Paths of every `SKILL.md` in the repo's default branch tree.
pub fn find_skill_files(slug: &str, branch: &str) -> Result<Vec<String>> {
    slug_ok(slug)?;
    let v = get(&format!("repos/{slug}/git/trees/{branch}?recursive=1"))?;
    Ok(v["tree"].as_array().map(|t| t.iter().filter_map(|n| n["path"].as_str()).filter(|p| p.ends_with("SKILL.md")).map(String::from).collect()).unwrap_or_default())
}

/// `2026-11-05T15:52:40Z` -> unix seconds (UTC).
pub fn iso_epoch(s: &str) -> Option<i64> {
    let (d, t) = s.trim_end_matches('Z').split_once('T')?;
    let mut dp = d.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (dp.next()?.ok()?, dp.next()?.ok()?, dp.next()?.ok()?);
    let mut tp = t.split(|c| c == ':' || c == '+').map(|x| x.parse::<i64>());
    let (h, mi, sec) = (tp.next()?.ok()?, tp.next()?.ok()?, tp.next()?.ok()?);
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + h * 3600 + mi * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn epoch() {
        assert_eq!(iso_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso_epoch("2026-11-05T15:52:40Z"), Some(1793893960));
        assert_eq!(iso_epoch("garbage"), None);
    }
}
