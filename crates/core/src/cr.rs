//! Parsing of CodeRabbit review-comment bodies.
use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Default, PartialEq)]
pub struct Header {
    pub severity: Option<String>,
    pub category: Option<String>,
    pub effort: Option<String>,
    /// Comment kind in the older layout: "Potential issue", "Refactor suggestion", ...
    pub kind: Option<String>,
    pub title: Option<String>,
}

static BOLD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\*\*([^*\n]+?)\*\*").unwrap());
static PICTURE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<picture>.*?</picture>").unwrap());

const SEVERITIES: &[&str] = &["critical", "major", "minor", "trivial", "nitpick", "info"];
const EFFORTS: &[&str] = &["quick win", "heavy lift", "moderate lift", "medium lift"];
const KINDS: &[&str] = &["potential issue", "refactor suggestion", "nitpick", "verification", "assertive"];

/// Drop markdown emphasis and leading emoji/punctuation: `_🎯 Functional Correctness_` -> `Functional Correctness`.
fn clean(tok: &str) -> String {
    tok.trim_matches(|c: char| c == '*' || c == '_' || c.is_whitespace())
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim()
        .to_string()
}

/// Two layouts exist:
///  new: `**<picture/> Minor** · **Functional Correctness** · **Quick win**`
///  old: `_🎯 Functional Correctness_ | _🟠 Major_ | _⚡ Quick win_` (or `_⚠️ Potential issue_ | ...`)
/// Tokens are classified by content, not position. The title is the first bold line after the header.
pub fn parse_header(body: &str) -> Header {
    let body = PICTURE.replace_all(body, "");
    let mut lines = body.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("<!--"));
    let mut h = Header::default();
    if let Some(first) = lines.next() {
        for tok in first.split(|c| c == '|' || c == '·').map(clean).filter(|t| !t.is_empty()) {
            let l = tok.to_lowercase();
            if SEVERITIES.contains(&l.as_str()) { h.severity.get_or_insert(tok); }
            else if EFFORTS.contains(&l.as_str()) { h.effort.get_or_insert(tok); }
            else if KINDS.contains(&l.as_str()) { h.kind.get_or_insert(tok); }
            else { h.category.get_or_insert(tok); }
        }
    }
    if let Some(t) = lines.next() {
        h.title = BOLD.captures(t).map(|c| c[1].trim().to_string());
    }
    h
}

static MARKER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<!--\s*(?:[A-Za-z0-9_.-]+-)?skill:\s*([A-Za-z0-9._@#+-]+)").unwrap());

/// Skill-version marker agents leave in the PR body, e.g. `<!-- my-skill-skill: 0.7 -->` (the name part is optional).
pub fn skill_marker(pr_body: &str) -> Option<String> {
    MARKER.captures(pr_body).map(|c| c[1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn header() {
        let b = "<!-- coderabbit-ai-generated:v1 -->\n\n**<picture><img src=\"x\" alt=\"\"></picture> Minor** · **Functional Correctness** · **Quick win**\n\n**Identify the rerun.**\n\nbody";
        let h = parse_header(b);
        assert_eq!(h.severity.as_deref(), Some("Minor"));
        assert_eq!(h.category.as_deref(), Some("Functional Correctness"));
        assert_eq!(h.effort.as_deref(), Some("Quick win"));
        assert_eq!(h.title.as_deref(), Some("Identify the rerun."));
    }
    #[test]
    fn old_layout() {
        let b = "_🎯 Functional Correctness_ | _🟠 Major_ | _⚡ Quick win_\n\n**Fail open when refresh fails.**\n\ntext";
        let h = parse_header(b);
        assert_eq!(h.category.as_deref(), Some("Functional Correctness"));
        assert_eq!(h.severity.as_deref(), Some("Major"));
        assert_eq!(h.effort.as_deref(), Some("Quick win"));
        assert_eq!(h.title.as_deref(), Some("Fail open when refresh fails."));
    }
    #[test]
    fn old_layout_kind_only() {
        let h = parse_header("_⚠️ Potential issue_ | _🟡 Minor_ | _⚡ Quick win_\n\n**T.**");
        assert_eq!(h.kind.as_deref(), Some("Potential issue"));
        assert_eq!(h.category, None);
        assert_eq!(h.severity.as_deref(), Some("Minor"));
    }
    #[test]
    fn marker() {
        assert_eq!(skill_marker("x\n<!-- my-skill-skill: my-skill@0.7 -->").as_deref(), Some("my-skill@0.7"));
        assert_eq!(skill_marker("Skill: x\n<!-- my-skill-skill: 0.7#a587b94 -->").as_deref(), Some("0.7#a587b94"));
        assert_eq!(skill_marker("no marker"), None);
    }
}
