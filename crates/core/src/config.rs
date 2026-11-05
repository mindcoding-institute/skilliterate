//! Local configuration (`skilliterate.toml`): which skills to improve, where they live, where they are used.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(default = "d_db")]
    pub db: String,
    #[serde(default)]
    pub openrouter: OpenRouterCfg,
    #[serde(default)]
    pub review: ReviewCfg,
    #[serde(default, rename = "skill")]
    pub skills: Vec<SkillCfg>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OpenRouterCfg {
    /// Name of the environment variable that holds the key. The key itself is never stored.
    pub key_env: String,
    pub model: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReviewCfg {
    /// GitHub login of the review bot whose threads are mined.
    pub bot: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SkillCfg {
    pub name: String,
    /// Repo (`owner/name`) that holds the skill file; improvement PRs are opened here.
    pub repo: String,
    pub path: String,
    #[serde(default = "d_base")]
    pub base: String,
    /// Files that must stay byte-identical to the skill file (e.g. a plugin mirror).
    #[serde(default)]
    pub extra_files: Vec<String>,
    /// JSON files whose `"version"` must be bumped together with the skill.
    #[serde(default)]
    pub version_files: Vec<String>,
    /// A marketplace-style JSON file with a `plugins[]` entry to bump, as `file` + `plugin`.
    #[serde(default)]
    pub marketplace_file: Option<String>,
    #[serde(default)]
    pub marketplace_plugin: Option<String>,
    /// Repos whose PRs used the skill (review findings are mined from these).
    #[serde(default)]
    pub usage_repos: Vec<String>,
}

fn d_db() -> String { "data/skilliterate.db".into() }
fn d_base() -> String { "main".into() }
impl Default for OpenRouterCfg { fn default() -> Self { Self { key_env: "OPEN_ROUTER_TOKEN".into(), model: "typesafe/jev-router".into() } } }
impl Default for Config { fn default() -> Self { Self { db: d_db(), openrouter: OpenRouterCfg::default(), review: ReviewCfg::default(), skills: Vec::new() } } }
impl Default for ReviewCfg { fn default() -> Self { Self { bot: "coderabbitai".into() } } }

/// A path inside the repo: relative, no `..`, not starting with `-`.
pub fn safe_rel(skill: &str, p: &str) -> Result<()> {
    let bad = p.is_empty() || p.starts_with('/') || p.starts_with('-') || p.contains('\\') || p.split('/').any(|c| c == ".." || c.is_empty());
    if bad { bail!("skill '{skill}': '{p}' must be a relative path inside the repo (no '..', no leading '/' or '-')"); }
    Ok(())
}

/// `owner/name` with only characters GitHub allows.
pub fn valid_slug(s: &str) -> bool {
    let ok = |p: &str| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    !s.starts_with('-') && matches!(s.split_once('/'), Some((o, n)) if ok(o) && ok(n) && !n.contains('/'))
}

impl Config {
    /// `$SKILLITERATE_CONFIG`, then `./skilliterate.toml`, then `~/.config/skilliterate/config.toml`.
    pub fn candidates() -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Ok(p) = std::env::var("SKILLITERATE_CONFIG") { v.push(p.into()); }
        v.push("skilliterate.toml".into());
        if let Ok(h) = std::env::var("HOME") { v.push(Path::new(&h).join(".config/skilliterate/config.toml")); }
        v
    }

    pub fn find() -> Option<PathBuf> { Self::candidates().into_iter().find(|p| p.is_file()) }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent().filter(|d| !d.as_os_str().is_empty()) { std::fs::create_dir_all(d)?; }
        std::fs::write(path, toml::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.skills {
            if s.name.is_empty() || !seen.insert(&s.name) { bail!("skill names must be non-empty and unique (got '{}')", s.name); }
            if !valid_slug(&s.repo) { bail!("skill '{}': repo must be owner/name, got '{}'", s.name, s.repo); }
            for f in std::iter::once(&s.path).chain(&s.extra_files).chain(&s.version_files).chain(s.marketplace_file.iter()) { safe_rel(&s.name, f)?; }
            if s.base.is_empty() || s.base.starts_with('-') || s.base.contains(char::is_whitespace) { bail!("skill '{}': invalid base branch '{}'", s.name, s.base); }
            if s.marketplace_file.is_some() != s.marketplace_plugin.is_some() { bail!("skill '{}': marketplace_file and marketplace_plugin go together", s.name); }
        }
        Ok(())
    }

    pub fn skill(&self, name: &str) -> Result<&SkillCfg> {
        self.skills.iter().find(|s| s.name == name).with_context(|| format!("no skill '{name}' in the config (run `skilliterate setup`)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_and_defaults() {
        let c: Config = toml::from_str("[[skill]]\nname=\"a\"\nrepo=\"o/r\"\npath=\"s/SKILL.md\"\nusage_repos=[\"o/r\"]\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.skills[0].base, "main");
        assert_eq!(c.review.bot, "coderabbitai");
        assert_eq!(c.openrouter.key_env, "OPEN_ROUTER_TOKEN");
        let again: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(again.skills[0].usage_repos, vec!["o/r".to_string()]);
    }
    #[test]
    fn slugs() {
        assert!(valid_slug("owner/repo.name-1"));
        for bad in ["", "a", "a/b/c", "-x/y", "a/..", "a b/c", "a/b;rm", "../x"] { assert!(!valid_slug(bad), "{bad}"); }
    }
    #[test]
    fn rejects_traversal() {
        for bad in ["../x/SKILL.md", "/etc/passwd", "a/../b", "-rf", "a//b"] {
            let mut c = Config::default();
            c.skills.push(SkillCfg { name: "a".into(), repo: "o/r".into(), path: bad.into(), base: "main".into(), ..Default::default() });
            assert!(c.validate().is_err(), "{bad}");
        }
    }
    #[test]
    fn rejects_bad() {
        let mut c = Config::default();
        c.skills.push(SkillCfg { name: "a".into(), repo: "nope".into(), path: "x".into(), ..Default::default() });
        assert!(c.validate().is_err());
    }
}
