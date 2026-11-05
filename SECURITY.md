# Security

skilliterate is a local, single-user command-line tool. It trusts its own config file, its database and the output of the `gh` and `claude` CLIs it runs. It is not designed to run as a service or on untrusted input.

## What it touches
- Reads pull requests and review threads through `gh`, using your existing login.
- Sends review text and skill files to the `claude` CLI and to OpenRouter.
- Writes a local SQLite database that contains review text from your repos.
- Never pushes, writes to a base branch or merges. Skill edits are drafted in a local git worktree on a `skilliterate/` branch; you review, push and open the PR yourself. (Opening draft PRs through the API is planned and will keep the same rule: only new `skilliterate/` branches, never the base.)

## Keys
The OpenRouter key is read from the environment variable named in the config, or from a `.env` file in the working directory. It is sent only in the request's `Authorization` header and is not logged or stored.

## Hardening in place
- Subprocesses (`gh`, `git`, `claude`) are started with argument lists, never through a shell.
- Paths from the config, from the bundle settings in the database and from command-line flags must be relative to the repo, without `..`, and repo names must be `owner/name` (checked before any file write or API call).
- The database is created by `skilliterate init`, which refuses to overwrite an existing file.
- Model output cannot run commands: `claude` is started with tools, MCP servers and settings disabled.
- Pull-request text is untrusted. It can influence the text of a proposed skill edit, so review every draft PR before merging.

## Reporting a vulnerability
Open a private security advisory on the repository's Security tab.
