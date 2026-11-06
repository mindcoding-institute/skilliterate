# skilliterate — design

Goal: fewer review findings on future pull requests, by feeding recurring root causes back into the agent skills that guide the work.

A local, single-user tool. It improves skills for one person from one local config and one local database. It is not a collaborative or hosted system.

## Pipeline

```
ingest → verdict → root-cause → taxonomy → categorize → report → skill-patch → measure
```

| # | Stage | What it does | Model |
|---|-------|--------------|-------|
| 1 | ingest | List PRs of the repos where a skill is used; fetch the review bot's threads, PR size and body | none (`gh`) |
| 2 | verdict | Decide `accepted` / `rebutted` / `ignored` / `unclear` per thread | rules, then a small model, then a larger one for ambiguous threads |
| 3 | root-cause | For accepted findings: what was done wrong, and which skill instruction would have prevented it | `claude` (Sonnet) |
| 4 | taxonomy | Propose mistake categories from the root causes; a human approves them | `claude` (Sonnet) + human |
| 5 | categorize | Assign every finding to an approved category | OpenRouter (Jev) |
| 6 | report | Accepted findings per 1000 changed lines, per category, per month and per skill version | none (SQL) |
| 7 | skill-patch | Draft a minimal skill edit for a category and open it as a draft PR | `claude` (Sonnet) |
| 8 | measure | Compare a category's rate before and after each shipped adjustment | none (SQL) |

Each stage is incremental and resumable: it only processes rows that lack its output, and every model call is logged with its cost.

## Verdict signals

Review bots start comments with a header that carries severity and a native category. Two header layouts are parsed; tokens are classified by content, not position. Verdict signals, cheapest first:
1. Rules: the thread is resolved and the author replied "fixed in <sha>", or the bot itself marked it addressed in a commit → `accepted`; replies such as "by design" or "false positive" → `rebutted`.
2. A small model for short or unusual replies.
3. A larger model only for long or ambiguous threads.

The verdict source and confidence are stored so rules can be audited and tuned.

## Measurement

- Rate = accepted findings per 1000 changed lines (additions + deletions) of the PRs created in the period.
- **Baseline cut.** Everything created before the cut is one baseline group. After the cut, each PR is attributed to a skill version: from a marker the agent leaves in the PR body (`<!-- my-skill-skill: 1.2 -->`) if present, otherwise from the newest skill version that existed when the PR was created. The git blob hash is the identity of a version, since version labels can be reused.
- **Adjustments.** Each skill edit made from a category is recorded with its evidence. The effect view compares that category's rate in the baseline with PRs created after the edit shipped. It reports "too early" below 10 PRs and 10 kLOC. It is an association, not proof of cause: sizes, authors and review settings change too.

## Prerequisites (checked by `skilliterate setup`)
- `claude` CLI, logged in. Codex support is planned behind the same interface.
- `gh`, logged in. Skills live in GitHub repos and are read through the API.
- An OpenRouter key in an environment variable. The config stores the variable name, never the key.
- A Rust toolchain, or a prebuilt binary.

## Local layout and config

```
skilliterate.toml      your config (git-ignored; see skilliterate.example.toml)
data/skilliterate.db   SQLite database (git-ignored)
```

`skilliterate init` scaffolds the database and fails if it already exists. `skilliterate setup` checks prerequisites, adds a skill to the config, detects versioning and indexes the skill's history.

Config file lookup: `$SKILLITERATE_CONFIG`, `./skilliterate.toml`, `~/.config/skilliterate/config.toml`. Per skill: name, repo, path, base branch, optional mirror and version files, and the repos where it is used. Paths must stay inside the repo (no `..`).

**Versioning.** If the skill file has a version in its frontmatter, its history is indexed and the baseline cut is set to now. If it has none, everything up to and after setup counts as iteration 0; the first PR the tool opens adds a version and the PR-footer instruction, and becomes iteration 1.

## Patching

Status: `patch draft` and `patch pr` work through the GitHub API; no clone or worktree is needed.

Flow:
1. `patch draft <skill> <category>` fetches the skill file and its related files at the base branch, asks the model for the smallest edit, validates that each edit matches exactly once, bumps the versions, and stores the result.
2. `patch pr <id>` creates a branch `skilliterate/adj-<id>-<slug>` from the base head through the git-data API (one commit for all files), then opens a **draft** PR. It never writes to the base branch or any other ref, never merges, and refuses if any file it would change moved on the base since the draft. `patch drop` only discards unpushed drafts.
3. After the merge, the next `run` records the ship time.

## Crates

```
crates/core   store (SQLite), config, review-comment parsing, verdict rules
crates/gh     GitHub access through the `gh` CLI (reuses its auth; no token handling)
crates/llm    `claude` subprocess runner and OpenRouter client
crates/cli    the `skilliterate` binary
```

## Open points
- Wording of the PR-footer instruction for skills that have no "open a PR" step.
- Mirror and version files are manual config; there is no auto-detection.
- Skills used in repos without a review bot yield no data; `setup` should warn.
- Optional Turso backend and a terminal UI are later.
