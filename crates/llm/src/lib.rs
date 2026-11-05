//! Runs Claude agents through the `claude` CLI (`claude -p`), minimal context, structured JSON output.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{mpsc, atomic::{AtomicUsize, Ordering}};

#[derive(Clone)]
pub struct Claude {
    /// `haiku` or `sonnet` (aliases understood by the CLI)
    pub model: String,
}

pub struct Reply {
    pub value: Value,
    pub cost_usd: f64,
}

impl Claude {
    pub fn new(model: &str) -> Self { Self { model: model.into() } }

    pub fn run_json(&self, system: &str, prompt: &str, schema: &Value) -> Result<Reply> {
        let mut last = None;
        for _ in 0..3 {
            match self.once(system, prompt, schema) {
                Ok(r) => return Ok(r),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap())
    }

    fn once(&self, system: &str, prompt: &str, schema: &Value) -> Result<Reply> {
        // Stripped-down session: no plugins/MCP/skills (a default session costs ~$0.30 per call in cache writes).
        let mut child = Command::new("claude")
            .args(["-p", "--model", &self.model, "--output-format", "json", "--json-schema", &schema.to_string(),
                   "--tools", "", "--no-session-persistence", "--setting-sources", "", "--strict-mcp-config",
                   "--disable-slash-commands", "--system-prompt", system])
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().context("spawning claude")?;
        child.stdin.take().unwrap().write_all(prompt.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() { bail!("claude exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr)); }
        let d: Value = serde_json::from_slice(&out.stdout).context("parsing claude output")?;
        let Some(value) = d.get("structured_output").filter(|v| !v.is_null()).cloned() else {
            bail!("no structured_output: {}", d["result"].as_str().unwrap_or("?"));
        };
        Ok(Reply { value, cost_usd: d["total_cost_usd"].as_f64().unwrap_or(0.0) })
    }
}

/// Run `work` over items on `workers` threads; `on_done` runs on the calling thread as each finishes
/// (so the caller can write to SQLite incrementally and survive interruption).
pub fn par_for_each<T: Sync, R: Send>(
    items: &[T], workers: usize, work: impl Fn(&T) -> R + Sync, mut on_done: impl FnMut(usize, R),
) {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            let tx = tx.clone();
            let (next, work) = (&next, &work);
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() { break; }
                if tx.send((i, work(&items[i]))).is_err() { break; }
            });
        }
        drop(tx);
        for (i, r) in rx { on_done(i, r); }
    });
}

/// OpenRouter chat client (used for the Jev categorizer).
pub struct OpenRouter { token: String }

impl OpenRouter {
    /// Reads OPEN_ROUTER_TOKEN from the environment or a `.env` file (CRLF-tolerant).
    pub fn from_env() -> Result<Self> { Self::from_env_var("OPEN_ROUTER_TOKEN") }

    /// Reads the key from the environment variable `var`, or from `var=` in a `.env` file.
    pub fn from_env_var(var: &str) -> Result<Self> {
        if let Ok(t) = std::env::var(var) { return Ok(Self { token: t.trim().into() }); }
        let text = std::fs::read_to_string(".env").with_context(|| format!("{var} not set and no .env"))?;
        let prefix = format!("{var}=");
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix(prefix.as_str()) {
                return Ok(Self { token: v.trim().trim_matches('"').into() });
            }
        }
        bail!("{var} not found in environment or .env")
    }

    /// True if OpenRouter accepts the key (GET /auth/key).
    pub fn check(&self) -> Result<()> {
        ureq::get("https://openrouter.ai/api/v1/auth/key").set("Authorization", &format!("Bearer {}", self.token)).call()
            .map(|_| ()).map_err(|e| anyhow::anyhow!("OpenRouter rejected the key: {e}"))
    }

    pub fn run_json(&self, model: &str, system: &str, prompt: &str, schema_name: &str, schema: &Value) -> Result<Reply> {
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role":"system","content":system},{"role":"user","content":prompt}],
            "response_format": {"type":"json_schema","json_schema":{"name":schema_name,"strict":true,"schema":schema}},
            "usage": {"include": true}
        });
        let mut last = None;
        for attempt in 0..3 {
            if attempt > 0 { std::thread::sleep(std::time::Duration::from_secs(2 * attempt)); }
            let res = ureq::post("https://openrouter.ai/api/v1/chat/completions")
                .set("Authorization", &format!("Bearer {}", self.token))
                .timeout(std::time::Duration::from_secs(180))
                .send_json(body.clone());
            match res {
                Ok(r) => {
                    let d: Value = r.into_json()?;
                    let Some(text) = d["choices"][0]["message"]["content"].as_str() else { last = Some(anyhow::anyhow!("no content: {d}")); continue };
                    match serde_json::from_str::<Value>(text) {
                        Ok(value) => return Ok(Reply { value, cost_usd: d["usage"]["cost"].as_f64().unwrap_or(0.0) }),
                        Err(e) => last = Some(anyhow::anyhow!("bad json {e}: {text}")),
                    }
                }
                Err(e) => last = Some(anyhow::anyhow!("openrouter: {e}")),
            }
        }
        Err(last.unwrap())
    }
}
