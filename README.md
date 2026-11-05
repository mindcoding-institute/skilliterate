# skilliterate

Learns from the code-review findings on your pull requests and turns the recurring mistakes into improvements to your own agent skills.

A local, single-user command-line tool. It mines review-bot threads (CodeRabbit by default) from the repos where a skill is used, decides which findings were accepted, finds the root cause of each, groups causes into categories, counts findings per 1000 changed lines per category, drafts a skill edit for the worst categories (opening it as a draft pull request is planned). Each skill version is tracked, so you can see whether a change reduced the findings.

Status: early. See [DESIGN.md](DESIGN.md) for the pipeline and the plan.

## Prerequisites
- [`claude`](https://claude.com/claude-code) CLI, logged in (Codex support is planned)
- [`gh`](https://cli.github.com) logged in; your skills live in GitHub repos
- An [OpenRouter](https://openrouter.ai) key in an environment variable, used for the categorizer
- A Rust toolchain (`cargo build --release` produces `target/release/skilliterate`)

## Quick start
```
cargo build --release
export OPEN_ROUTER_TOKEN=...        # or put it in .env
skilliterate init                    # scaffolds data/skilliterate.db and a default skilliterate.toml (fails if the db exists)
skilliterate setup --skill my-skill --repo owner/repo --path skills/my-skill/SKILL.md --usage owner/repo
skilliterate setup --check           # re-run the checks only
```

See `skilliterate.example.toml` for the config. The tool never writes to a base branch and never merges: improvements are drafted as local worktrees on `skilliterate/…` branches. Opening them as draft pull requests through the GitHub API is planned; until then you push and open the PR yourself.

## Privacy
Everything stays on your machine. The database holds review text from your private repos, so `data/` and `skilliterate.toml` are git-ignored; keep them that way. Review text is sent to the `claude` CLI and to OpenRouter for analysis.

## License
MIT, see [LICENSE](LICENSE). See [SECURITY.md](SECURITY.md) for the trust model.
