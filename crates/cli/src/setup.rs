//! `skilliterate setup`: check the prerequisites, record the skill(s) in the local config, detect versioning, index history.
use anyhow::{bail, Result};
use rusqlite::Connection;
use skilliterate_core::config::{Config, SkillCfg};
use skilliterate_gh::api;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

#[derive(clap::Args, Default)]
pub struct SetupArgs {
    /// Only check the existing config and prerequisites; change nothing
    #[arg(long)] pub check: bool,
    /// Add or update a skill with this name
    #[arg(long)] pub skill: Option<String>,
    /// Repo (owner/name) that holds the skill file
    #[arg(long)] pub repo: Option<String>,
    /// Path of the skill file inside the repo
    #[arg(long)] pub path: Option<String>,
    /// Base branch for improvement PRs (default: the repo's default branch)
    #[arg(long)] pub base: Option<String>,
    /// Repos whose PRs used the skill (comma separated)
    #[arg(long, value_delimiter = ',')] pub usage: Vec<String>,
    /// Files that must stay identical to the skill file (comma separated)
    #[arg(long, value_delimiter = ',')] pub extra_files: Vec<String>,
    /// JSON files whose "version" is bumped with the skill (comma separated)
    #[arg(long, value_delimiter = ',')] pub version_files: Vec<String>,
    #[arg(long)] pub marketplace_file: Option<String>,
    #[arg(long)] pub marketplace_plugin: Option<String>,
    /// Where to write the config (default: ./skilliterate.toml, or the existing file)
    #[arg(long)] pub config: Option<PathBuf>,
}

struct Report { failed: usize }
impl Report {
    fn ok(&self, what: &str, detail: &str) { println!("  ✓ {what}{}", if detail.is_empty() { String::new() } else { format!(" — {detail}") }); }
    fn bad(&mut self, what: &str, detail: &str) { self.failed += 1; println!("  ✗ {what} — {detail}"); }
    fn note(&self, what: &str) { println!("  · {what}"); }
}

fn ask(label: &str, default: Option<&str>) -> Result<String> {
    if !std::io::stdin().is_terminal() { bail!("{label} is required (pass it as a flag; no terminal to ask on)"); }
    print!("{label}{}: ", default.map(|d| format!(" [{d}]")).unwrap_or_default());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let v = line.trim();
    Ok(if v.is_empty() { default.unwrap_or("").to_string() } else { v.to_string() })
}

pub fn run(conn_for: impl Fn(&str) -> Result<Connection>, a: SetupArgs) -> Result<()> {
    let path = a.config.clone().or_else(Config::find).unwrap_or_else(|| "skilliterate.toml".into());
    let mut cfg = if path.is_file() { Config::load(&path)? } else { Config::default() };
    let mut r = Report { failed: 0 };

    println!("Prerequisites");
    match std::process::Command::new("claude").arg("--version").output() {
        Ok(o) if o.status.success() => r.ok("claude CLI", String::from_utf8_lossy(&o.stdout).trim()),
        _ => r.bad("claude CLI", "not found on PATH (install Claude Code and log in)"),
    }
    match api::auth_ok() { Ok(u) => r.ok("gh", &format!("logged in as {u}")), Err(e) => r.bad("gh", &e.to_string()) }
    match skilliterate_llm::OpenRouter::from_env_var(&cfg.openrouter.key_env).and_then(|o| o.check()) {
        Ok(()) => r.ok("OpenRouter key", &format!("{} accepted (model {})", cfg.openrouter.key_env, cfg.openrouter.model)),
        Err(e) => r.bad("OpenRouter key", &format!("{e} — set {} in the environment or a .env file", cfg.openrouter.key_env)),
    }

    // add / update a skill from flags or prompts
    if !a.check && (a.skill.is_some() || cfg.skills.is_empty()) {
        let name = match a.skill.clone() { Some(n) => n, None => ask("Skill name", None)? };
        let existing = cfg.skills.iter().position(|s| s.name == name);
        let prev = existing.map(|i| cfg.skills[i].clone());
        let repo = match a.repo.clone().or(prev.as_ref().map(|p| p.repo.clone())) { Some(v) => v, None => ask("Repo that holds the skill (owner/name)", None)? };
        let info = api::repo(&repo)?;
        let path_in_repo = match a.path.clone().or(prev.as_ref().map(|p| p.path.clone())) {
            Some(v) => v,
            None => {
                let found = api::find_skill_files(&repo, &info.default_branch).unwrap_or_default();
                if !found.is_empty() { println!("SKILL.md files in {repo}:\n  {}", found.join("\n  ")); }
                ask("Path of the skill file", found.first().map(String::as_str))?
            }
        };
        let base = a.base.clone().or(prev.as_ref().map(|p| p.base.clone())).unwrap_or_else(|| info.default_branch.clone());
        let usage = if !a.usage.is_empty() { a.usage.clone() } else if let Some(p) = &prev { p.usage_repos.clone() }
            else { ask("Repos whose PRs use this skill (comma separated)", Some(&repo))?.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect() };
        let s = SkillCfg {
            name, repo, path: path_in_repo, base,
            extra_files: if a.extra_files.is_empty() { prev.as_ref().map(|p| p.extra_files.clone()).unwrap_or_default() } else { a.extra_files.clone() },
            version_files: if a.version_files.is_empty() { prev.as_ref().map(|p| p.version_files.clone()).unwrap_or_default() } else { a.version_files.clone() },
            marketplace_file: a.marketplace_file.clone().or(prev.as_ref().and_then(|p| p.marketplace_file.clone())),
            marketplace_plugin: a.marketplace_plugin.clone().or(prev.as_ref().and_then(|p| p.marketplace_plugin.clone())),
            usage_repos: usage,
        };
        match existing { Some(i) => cfg.skills[i] = s, None => cfg.skills.push(s) }
        cfg.validate()?;
        cfg.save(&path)?;
        println!("\nWrote {}", path.display());
    }
    if cfg.skills.is_empty() { bail!("no skills configured; run `skilliterate setup --skill NAME --repo OWNER/REPO --path PATH`"); }

    let conn = conn_for(&cfg.db)?;
    for s in &cfg.skills {
        println!("\nSkill «{}» ({} : {} @ {})", s.name, s.repo, s.path, s.base);
        match api::repo(&s.repo) {
            Ok(i) if i.can_push => r.ok("repo access", "push allowed (PRs are opened from skilliterate/ branches; the base branch is never written)"),
            Ok(_) => r.bad("repo access", "no push permission — a fork-based flow is not supported yet"),
            Err(e) => { r.bad("repo access", &e.to_string()); continue; }
        }
        let Some(text) = api::file_at(&s.repo, &s.path, &s.base)? else { r.bad("skill file", &format!("{} not found on {}", s.path, s.base)); continue };
        r.ok("skill file", &format!("{} bytes", text.len()));
        for f in s.extra_files.iter().chain(s.version_files.iter()).chain(s.marketplace_file.iter()) {
            match api::file_at(&s.repo, f, &s.base)? { Some(_) => r.ok("related file", f), None => r.bad("related file", &format!("{f} not found on {}", s.base)) }
        }
        for u in &s.usage_repos {
            match api::repo(u) { Ok(_) => r.ok("usage repo", u), Err(e) => r.bad("usage repo", &format!("{u}: {e}")) }
        }
        let versioned = crate::skills::frontmatter_version(&text);
        conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![format!("versioned:{}", s.name), if versioned.is_some() { "yes" } else { "no" }])?;
        match &versioned {
            Some(v) => {
                let n = crate::skills::index_api(&conn, s)?;
                r.ok("versioning", &format!("version {v}; indexed {n} commits of history"));
                if conn.query_row("SELECT COUNT(*) FROM meta WHERE key='cut_at'", [], |x| x.get::<_, i64>(0))? == 0 {
                    crate::adjust::set_cut(&conn, None)?;
                    r.note("baseline cut set to now: PRs created before it are one baseline group");
                }
            }
            None => {
                r.note("no version in the skill's frontmatter: everything up to and after setup counts as iteration 0 (baseline)");
                r.note("the first PR `skilliterate` opens adds a version and the PR footer instruction, and becomes iteration 1");
            }
        }
    }
    println!();
    if r.failed > 0 { bail!("{} check(s) failed", r.failed); }
    println!("All checks passed.");
    Ok(())
}
