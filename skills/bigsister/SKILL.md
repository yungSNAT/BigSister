---
name: bigsister
description: Answer plain-language questions about where a coding agent's tokens went (what loads before the first message, which MCP server or tool schema costs most, whether the prompt cache keeps expiring, slow hooks, hidden test failures) by running the local bigsister CLI on the transcripts Claude Code, Codex, pi, OpenCode, Gemini CLI and Qwen Code save on this machine.
---

# BigSister

Use this skill when the user asks where their coding agent's usage or tokens went, or why sessions cost more than expected. `bigsister` must be installed (see the README's Install section); with the v0.1.0 release the executable is `shaphan`, which predates `context`, `tools`, `cache`, `models` and `--sample`.

## Run it

```bash
bigsister all --json
```

This reads the last 7 days of the agents' default transcript folders and prints one JSON object (`schema_version` 2). Narrow it when the question is narrow:

- `bigsister <section> --json` runs one section: `startup`, `context`, `schemas`, `pipes`, `polls`, `rereads`, `tools`, `cache`, `models` or `hooks`.
- `--since 24h` (or `30d`, a date, or an RFC 3339 time) sets the window; `--agent claude` (or `codex`, `pi`, `opencode`, `gemini`, `qwen`) limits it to one agent; `--limit N` sets the rows per table.
- `--sample` runs on synthetic transcripts built into `bigsister`, to show the user a report before reading theirs.

If none of the folders exists, it exits 1 and names what is missing; pass the right folder with `--claude-root`, `--codex-root`, `--pi-root`, `--opencode-root`, `--gemini-root` or `--qwen-root`.

## Which section answers which question

The JSON holds `roots` (what was scanned, with lines skipped), `since` and `detectors`, keyed by section; each section has a `summary`, `tables` (each a `title` and `rows`) and `notes`.

| Question | Section | Look at |
|---|---|---|
| Where did my usage go? | `ranked` (in `all` only) | Rows by `waste_usd_est`: polls' output, re-reads after compactions, idle tool schemas and unused skills |
| What loads before my first message? | `startup` | `summary.median_total_chars` and the 'By source' table (instruction files, hook output, listings, tool schemas, MCP instructions) |
| Which MCP server costs most? | `context` | The 'By MCP server' table, by `est_input_tokens` (needs the request log recent Claude Code versions write) |
| Which tools or skills load but are never used? | `schemas` | Rows by `idle_chars`, `idle_tokens_est` and `idle_usd_est`, against `sessions_loaded` and `sessions_called` |
| Is my cache expiring? | `cache` | `summary.idle_rebuild_share` and `idle_rebuilds`; the table splits write tokens by `cause` |
| Which model or subagent costs most? | `models` | Input-side cost by model, effort and subagent type; `subagent_sessions_on_top_model` |
| Which tool results are largest or keep erroring? | `tools` | Per tool calls, result characters, errors, cascades and retry churn |
| Is the agent polling or re-reading files? | `polls`, `rereads` | Repeated commands with no edit between; files read again after a compaction |
| Are tests failing behind a pipe? | `pipes` | Test runs piped into `tail` or `head` without `pipefail` |
| Are hooks slow? | `hooks` | Runs, cancellations, timeouts and median, p90 and max milliseconds by hook |

Most sections cover Claude Code only; `bigsister --help` lists the agents each one reads.

## Reading the numbers

Fields ending `_tokens_est` are characters divided by 4; fields ending `_usd_est` price them once as uncached input at the bundled prices (`usd_basis` says which basis). They are estimates for ranking, not a bill. Say so when you report them, and quote the section and field each figure comes from.

## Privacy

`bigsister` reads only local transcripts, makes no network calls and never prints message text, tool inputs or tool outputs. Output holds names, paths, counts, sizes and times; paths under the home folder start with `~`. Re-read file paths appear only with `--paths` and Git branch names only with `--by-branch`, so add those only when the user asks. Ask the user to read the output before sharing it.

## What it does not do

- It is not a usage or bill tracker: every dollar figure is an estimate from token counts and prices, none read from a bill.
- It changes nothing: it does not edit settings, remove MCP servers or skills, or fix hooks. Suggest changes and let the user make them.
- It reads only transcript formats it knows; it skips lines it cannot parse and reports how many in `roots`.
