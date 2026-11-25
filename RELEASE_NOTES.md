# v0.2.0

BigSister is ShaphansAudit renamed, with 4 new report sections, 4 more agents, priced estimates and CSV output.

## New since v0.1.0

- `context`: each request in Claude Code's request log, rebuilt, with its input's characters by part (system prompt, built-in tools, MCP tools by server, system messages, reminders, tool results, tool calls, thinking, text, other blocks), its recorded input tokens split across them, and the largest requests (Claude).
- `tools`: per tool, calls, the characters their results returned, errors, and image and document blocks; also runs of 3 or more errors in a row and calls repeated with the same input after erring twice (Claude, pi, OpenCode).
- `cache`: cache write tokens split into new context and context cached again after an idle gap, a model switch or otherwise, priced at the cache write price (Claude).
- `models`: input-side cost by model, reasoning effort and subagent type, and how many subagent sessions ran on their parent session's top-priced model (Claude).
- pi's sessions (`~/.pi/agent/sessions`, `--pi-root`) for `pipes`, `polls`, `rereads` and `tools`, and OpenCode's database (`~/.local/share/opencode`, `--opencode-root`) for `pipes`, `polls`, `rereads` and `tools`. OpenCode's database is read from a copy in memory, without a lock or a file beside it; `--agent pi` and `--agent opencode` read one of them alone.
- Gemini CLI's sessions (`~/.gemini/tmp`, `--gemini-root`) and Qwen Code's (`~/.qwen/projects`, `--qwen-root`) for `pipes`, `polls` and `tools`; `--agent gemini` and `--agent qwen` read one of them alone.
- `rereads` also counts files read again after a recorded Qwen Code compaction.
- `rereads` also splits compactions by the trigger Claude Code records (auto or manual), with the median and largest token count in context before them and the re-reads after them, so a manual compaction before a break can be compared with an automatic one.
- Each `tokens est` column is followed by a `usd est` column: the estimate at the bundled price of the model that read it (every active Claude model, OpenAI's GPT-6, GPT-6.2, GPT-5.6 and Codex models, priced by prompt size where the price depends on it). `--usd-per-mtok USD` prices every estimate at one rate instead, and `--prices FILE` reads prices from a file with LiteLLM's field names.
- `--csv` prints the report's sections as one CSV table, a value a record.
- `--by-branch` adds a `models` table of input-side cost by the Git branch Claude Code records on each reply.
- Each section with a dollar column names its price basis in its summary (`usd_basis`: `bundled`, `usd_per_mtok` or `prices_file`).
- `all` starts with `ranked`: polls' output, re-reads after compactions, unused tool schemas and unused skills, each as estimated tokens and dollars over the whole window, most dollars first.
- `--sample` runs any section on sample transcripts built into `bigsister`, so a first run gives a full report before there are transcripts to read; with no transcript folder, the error suggests it.
- `schemas` also counts skills listed against skills used, and `polls` the characters every non-blank command's results returned, by verb class.
- The run header gives the reason each unreadable file or folder was unreadable.
- `--json` prints `schema_version` 2, which adds the dollar and unpriced columns, each root's `unreadable_reasons` and the new sections.

## Fixes

- A Claude record copied into another transcript, as when a session is resumed, counts once, as does a reply another transcript holds under new uuids.
- Codex commands are paired with the result records Codex writes as each ends; a command or read path that a cell builds from values is skipped rather than guessed.
- A tool id that JSON escapes in its result's line is found.

## Renamed from ShaphansAudit

BigSister was called ShaphansAudit until 19 November 2026, and its repository moved from `yungSNAT/ShaphansAudit` to `yungSNAT/BigSister`; GitHub redirects the old address. This release renames the executable, so a v0.1.0 install needs these changes:

- The executable is `bigsister` (`bigsister.exe` on Windows), not `shaphan`. Scripts that run `shaphan` run `bigsister` instead.
- Release archives are named `bigsister-0.2.0-<target>` and hold `bigsister`, where v0.1.0's were `shaphansaudit-0.1.0-<target>` holding `shaphan`. Install it where `shaphan` was, then delete `shaphan`.
- From source, the Cargo package is `bigsister`: `cargo install --locked --git https://github.com/yungSNAT/BigSister bigsister`, then `cargo uninstall shaphansaudit`.
- The `--json` output's `tool` field is `bigsister`, and the text report's first line starts with `bigsister`.
- To check a release's provenance, pass `--repo yungSNAT/BigSister` to `gh attestation verify`; v0.1.0's provenance still names `yungSNAT/ShaphansAudit`.
- A clone keeps working through the redirect; `git remote set-url origin https://github.com/yungSNAT/BigSister.git` points it at the new address.

BigSister keeps no configuration, cache or state file and reads no environment variable of its own, so nothing saved under the old name needs renaming.

## Licence

From this release BigSister is under Apache-2.0. v0.1.0 stays under FSL-1.1-ALv2, and becomes available under Apache-2.0 2 years after its release.

## Unreleased (since v0.2.0)

- `cache`: sub-TTL (< 5 minute) cache-bust table classifying unexpected prefix rebuilds (model swap, tool load / schema change via `shapeHash`, mid-history edit / rewind, or proxy or prefix change) with avoidable USD penalty and token-share vs dollar-share divergence.
- `rereads`: compaction health metrics covering post/pre retention ratio (flagging > 95% drops), compaction thrashing (< 10% saved across 2+ consecutive passes) and post-compaction file-churn/revert detection (including pi `edit`/`write` `path` mapping and Codex `exec`-cell `apply_patch`, `writeFile` and `writeFileSync` edits).
- `polls`: truncation-triggered command re-run table flagging identical shell commands re-run within 2 turns of a truncated or stashed tool output.
- `diff <before> <after>`: side-by-side comparison of turns, tokens, cache hit %, cost, sub-TTL cache busts, compactions and waste across 2 session folders or `START..END` time windows.
- A scan that finds no record in the window prints `Nothing scanned` above the sections instead of zeros that read as a clean result; `--json` adds `records_scanned` and each root's `files_with_records`.
- `scan`: scope cross-file UUID deduplication to Claude Code and pi so Gemini CLI, Qwen Code and OpenCode per-session short IDs never collide across files or agents.
