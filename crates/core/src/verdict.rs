//! Stage 2 rule-based verdicts. Anything the rules can't decide stays `unclear`
//! and is escalated to an LLM later.
use regex::Regex;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Debug, Deserialize, Clone)]
pub struct Comment {
    pub author: String,
    pub body: String,
}

#[derive(Debug, PartialEq, Clone)]
pub struct Verdict {
    pub verdict: &'static str, // accepted | rebutted | ignored | unclear
    pub source: &'static str,
    pub confidence: f64,
    pub evidence: String,
}

static FIXED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(fixed|addressed|done|applied|resolved|implemented|updated|corrected)\b.{0,40}\b(in|by|with)\b\s+`?[0-9a-f]{7,40}|\b(fixed|addressed|applied|done)\b").unwrap()
});
static REBUT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)won'?t fix|will not|not going to|by design|intentional|false positive|disagree|not applicable|out of scope|declin|no change needed|deliberate").unwrap()
});
static CR_ADDRESSED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)✅ Addressed in commits?|Addressed in commit").unwrap());

pub fn decide(thread: &[Comment], is_resolved: bool, is_outdated: bool) -> Verdict {
    let human: Vec<&Comment> = thread.iter().skip(1).filter(|c| !c.author.starts_with("coderabbit")).collect();
    let cr_after: Vec<&Comment> = thread.iter().skip(1).filter(|c| c.author.starts_with("coderabbit")).collect();

    if cr_after.iter().any(|c| CR_ADDRESSED.is_match(&c.body)) {
        return v("accepted", "rule:cr-addressed", 0.95, "CodeRabbit marked it addressed in a commit");
    }
    for c in human.iter().rev() {
        if REBUT.is_match(&c.body) && !FIXED.is_match(&c.body) {
            return v("rebutted", "rule:reply-rebut", 0.75, &trunc(&c.body));
        }
        if FIXED.is_match(&c.body) {
            return v("accepted", "rule:reply-fixed", 0.85, &trunc(&c.body));
        }
    }
    if human.is_empty() && is_outdated {
        return v("accepted", "rule:outdated-no-reply", 0.5, "thread outdated (code changed) with no reply; needs fix-commit confirmation");
    }
    if human.is_empty() {
        return v("ignored", "rule:no-reply", 0.6, if is_resolved { "resolved without reply" } else { "no reply, still open" });
    }
    v("unclear", "rule:none", 0.0, &trunc(&human.last().unwrap().body))
}

fn v(verdict: &'static str, source: &'static str, confidence: f64, evidence: &str) -> Verdict {
    Verdict { verdict, source, confidence, evidence: evidence.to_string() }
}
fn trunc(s: &str) -> String {
    s.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(a: &str, b: &str) -> Comment { Comment { author: a.into(), body: b.into() } }
    #[test]
    fn fixed_reply() {
        let t = vec![c("coderabbitai", "x"), c("me", "@coderabbitai Fixed in a6e95e4.")];
        assert_eq!(decide(&t, true, false).verdict, "accepted");
    }
    #[test]
    fn rebut_reply() {
        let t = vec![c("coderabbitai", "x"), c("me", "This is intentional, by design.")];
        assert_eq!(decide(&t, true, false).verdict, "rebutted");
    }
    #[test]
    fn no_reply_open() {
        assert_eq!(decide(&[c("coderabbitai", "x")], false, false).verdict, "ignored");
    }
}
