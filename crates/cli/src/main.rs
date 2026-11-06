use anyhow::{Context, Result};
mod adjust;
mod patch;
mod report;
mod setup;
mod skills;
mod stages;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "skilliterate", about = "Learn from code-review findings and improve your skills")]
struct Cli {
    /// Database file (default: `db` from the config, else data/skilliterate.db)
    #[arg(long)]
    db: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Tax {
    /// Sonnet proposes categories from all root causes
    Propose,
    Show,
    /// Approve proposed categories (all, or by id)
    Approve { ids: Vec<i64> },
    /// Retire a category
    Retire { id: i64 },
    /// Rename a category
    Rename { id: i64, name: String },
}

#[derive(Subcommand)]
enum Skills {
    /// Index a skill file's version history from a local git clone
    Index { #[arg(long)] clone: String, #[arg(long)] skill: String, #[arg(long)] file: String },
    /// Publishing bundle (JSON): {"mirrors":[..],"json_versions":[..],"marketplace":{"file":..,"plugin":..},"generic":true,"evidence":"all"}
    Bundle { skill: String, json: String },
    /// Say which skill a repo's PRs are made with
    Assign { repo: String, skill: String },
}

#[derive(Subcommand)]
enum Cut {
    /// Set the baseline cut (default: now). PRs created before it form one baseline group.
    Set { #[arg(long)] at: Option<String> },
    Show,
}

#[derive(Subcommand)]
enum Adjust {
    /// Record a skill edit aimed at a mistake category (omit --category for non-category changes)
    Add { #[arg(long)] skill: String, #[arg(long)] category: Option<String>, #[arg(long)] summary: String,
          #[arg(long)] instruction: Option<String>, #[arg(long)] version: Option<String>, #[arg(long)] pr: Option<String> },
    /// Mark an adjustment shipped (ship time from the indexed skill version, or --at)
    Ship { id: i64, #[arg(long)] version: Option<String>, #[arg(long)] at: Option<String>, #[arg(long)] pr: Option<String> },
    List,
    /// Category rate per kLOC: baseline vs after each shipped adjustment
    Effect,
}

#[derive(Subcommand)]
enum PatchCmd {
    /// Priority of categories for a skill (severity-weighted)
    Rank { skill: String },
    /// Draft a skill edit for a category, read through the GitHub API (nothing is written to GitHub)
    Draft { skill: String, category: String, #[arg(long)] base: Option<String>, #[arg(long)] check_against: Vec<String>,
            #[arg(long, default_value_t = 40)] max_evidence: usize },
    /// Show the diff of a drafted adjustment
    Show { id: i64 },
    /// Push a draft as a new skilliterate/ branch via the API and open a DRAFT pull request
    Pr { id: i64 },
    /// Discard a draft that has not been pushed
    Drop { id: i64 },
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a new database (and a default config if there is none). Fails if the database already exists
    Init,
    /// Check prerequisites, add a skill to the local config, detect versioning, index history
    Setup(setup::SetupArgs),
    /// Fetch CodeRabbit review threads for a repo (owner/name)
    Ingest { repo: String, #[arg(long, default_value = "source")] role: String },
    /// Apply rule-based accepted/rebutted verdicts
    Verdict,
    /// Escalate unsettled verdicts to Haiku/Sonnet
    VerdictLlm { #[arg(long, default_value_t = 10_000)] limit: usize, #[arg(long, default_value_t = 6)] workers: usize },
    /// Root-cause analysis of accepted findings (Sonnet)
    Rootcause { #[arg(long, default_value_t = 10_000)] limit: usize, #[arg(long, default_value_t = 6)] workers: usize,
                #[arg(long, default_value_t = 0.8)] min_confidence: f64 },
    /// Manage the mistake-category taxonomy
    Taxonomy { #[command(subcommand)] cmd: Tax },
    /// Jev assigns findings to approved categories
    Categorize { #[arg(long, default_value_t = 10_000)] limit: usize, #[arg(long, default_value_t = 6)] workers: usize,
                 #[arg(long)] redo: bool },
    /// Skill version history and repo -> skill assignment
    Skills { #[command(subcommand)] cmd: Skills },
    /// Findings per 1000 changed lines
    Rate { /// month (default) or skill
           #[arg(long, default_value = "month")] by: String,
           /// only categories whose name contains this
           #[arg(long)] category: Option<String> },
    /// Baseline cut for progress tracking
    Cut { #[command(subcommand)] cmd: Cut },
    /// Track skill adjustments per category and their effect
    Adjust { #[command(subcommand)] cmd: Adjust },
    /// Draft skill edits from category evidence
    Patch { #[command(subcommand)] cmd: PatchCmd },
    /// Re-parse CodeRabbit headers from stored bodies
    Reparse,
    /// Counts overview
    Stats,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Init = cli.cmd {
        let found = skilliterate_core::config::Config::find();
        let cfg = found.as_ref().map(|p| skilliterate_core::config::Config::load(p)).transpose()?;
        let db = cli.db.clone().or(cfg.as_ref().map(|c| c.db.clone())).unwrap_or_else(|| skilliterate_core::config::Config::default().db);
        skilliterate_core::store::create(&db)?;
        println!("created {db}");
        if found.is_none() { skilliterate_core::config::Config::default().save(std::path::Path::new("skilliterate.toml"))?; println!("wrote skilliterate.toml"); }
        println!("next: skilliterate setup --skill NAME --repo OWNER/REPO --path PATH/TO/SKILL.md --usage OWNER/REPO");
        return Ok(());
    }
    if let Cmd::Setup(a) = cli.cmd {
        let forced = cli.db.clone();
        return setup::run(|cfg_db| skilliterate_core::store::open(forced.as_deref().unwrap_or(cfg_db)), a);
    }
    let cfg = skilliterate_core::config::Config::find().map(|p| skilliterate_core::config::Config::load(&p)).transpose()?;
    let db = cli.db.clone().or(cfg.as_ref().map(|c| c.db.clone())).unwrap_or_else(|| "data/skilliterate.db".into());
    let conn = skilliterate_core::store::open(&db)?;
    match cli.cmd {
        Cmd::Setup(_) | Cmd::Init => unreachable!(),
        Cmd::Ingest { repo, role } => {
            let s = skilliterate_gh::ingest(&conn, &repo, &role, cfg.as_ref().map(|c| c.review.bot.as_str()).unwrap_or("coderabbitai"))?;
            println!("ingested {} PRs, {} CodeRabbit threads", s.prs, s.findings);
        }
        Cmd::Verdict => println!("verdicts written: {}", skilliterate_gh::run_verdicts(&conn)?),
        Cmd::VerdictLlm { limit, workers } => stages::verdict_llm(&conn, limit, workers)?,
        Cmd::Rootcause { limit, workers, min_confidence } => stages::root_cause(&conn, limit, workers, min_confidence)?,
        Cmd::Taxonomy { cmd } => match cmd {
            Tax::Propose => stages::taxonomy_propose(&conn)?,
            Tax::Show => stages::taxonomy_show(&conn)?,
            Tax::Approve { ids } => {
                let n = if ids.is_empty() { conn.execute("UPDATE categories SET status='approved' WHERE status='proposed'", [])? }
                    else { ids.iter().map(|i| conn.execute("UPDATE categories SET status='approved' WHERE id=?1", [i])).sum::<Result<usize, _>>()? };
                println!("approved {n}");
            }
            Tax::Retire { id } => { conn.execute("UPDATE categories SET status='retired' WHERE id=?1", [id])?; }
            Tax::Rename { id, name } => { conn.execute("UPDATE categories SET name=?2 WHERE id=?1", rusqlite::params![id, name])?; }
        },
        Cmd::Categorize { limit, workers, redo } => stages::categorize(&conn, limit, workers, redo)?,
        Cmd::Skills { cmd } => match cmd {
            Skills::Index { clone, skill, file } => {
                skilliterate_core::config::safe_rel(&skill, &file)?;
                println!("indexed {} versions of {skill}", skills::index(&conn, &clone, &skill, &file)?)
            }
            Skills::Bundle { skill, json } => {
                patch::parse_bundle(&json)?;
                conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![format!("bundle:{skill}"), json])?;
                println!("bundle saved for {skill}");
            }
            Skills::Assign { repo, skill } => { println!("{}", conn.execute("UPDATE repos SET skill=?2 WHERE slug=?1", rusqlite::params![repo, skill])?); }
        },
        Cmd::Rate { by, category } => match by.as_str() {
            "skill" => report::by_skill(&conn)?,
            _ => report::by_month(&conn, category.as_deref())?,
        },
        Cmd::Cut { cmd } => match cmd {
            Cut::Set { at } => adjust::set_cut(&conn, at)?,
            Cut::Show => println!("{}", adjust::show_cut(&conn)?.unwrap_or_else(|| "no cut set".into())),
        },
        Cmd::Adjust { cmd } => match cmd {
            Adjust::Add { skill, category, summary, instruction, version, pr } => adjust::add(&conn, &skill, category, &summary, instruction, version, pr)?,
            Adjust::Ship { id, version, at, pr } => adjust::ship(&conn, id, version, at, pr)?,
            Adjust::List => adjust::list(&conn)?,
            Adjust::Effect => adjust::effect(&conn)?,
        },
        Cmd::Patch { cmd } => match cmd {
            PatchCmd::Rank { skill } => patch::rank(&conn, &skill)?,
            PatchCmd::Draft { skill, category, base, check_against, max_evidence } => patch::draft(&conn, cfg.as_ref().context("no config found; run `skilliterate setup`")?, &skill, &category, base.as_deref(), &check_against, max_evidence)?,
            PatchCmd::Show { id } => patch::show(&conn, id)?,
            PatchCmd::Pr { id } => patch::open_pr(&conn, id)?,
            PatchCmd::Drop { id } => patch::drop_draft(&conn, id)?,
        },
        Cmd::Reparse => println!("reparsed {}", skilliterate_gh::reparse_headers(&conn)?),
        Cmd::Stats => {
            for (label, sql) in [
                ("by verdict", "SELECT v.verdict||' ('||v.source||')', COUNT(*) FROM verdicts v GROUP BY 1 ORDER BY 2 DESC"),
                ("by CR category (accepted)", "SELECT COALESCE(f.cr_category,'?'), COUNT(*) FROM findings f JOIN verdicts v ON v.finding_id=f.id WHERE v.verdict='accepted' GROUP BY 1 ORDER BY 2 DESC"),
                ("by severity (accepted)", "SELECT COALESCE(f.severity,'?'), COUNT(*) FROM findings f JOIN verdicts v ON v.finding_id=f.id WHERE v.verdict='accepted' GROUP BY 1 ORDER BY 2 DESC"),
                ("LLM spend by stage (US cents)", "SELECT stage||' / '||model, CAST(ROUND(SUM(cost_usd)*100) AS INTEGER) FROM llm_log GROUP BY 1"),
            ] {
                println!("\n== {label}");
                let mut st = conn.prepare(sql)?;
                for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                    let (k, n) = r?;
                    println!("{n:>6}  {k}");
                }
            }
        }
    }
    Ok(())
}
