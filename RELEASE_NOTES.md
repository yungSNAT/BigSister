# v0.2.0

BigSister was called ShaphansAudit until 19 November 2026, and its repository moved from `yungSNAT/ShaphansAudit` to `yungSNAT/BigSister`; GitHub redirects the old address. This release renames the executable, so a v0.1.0 install needs these changes:

- The executable is `bigsister` (`bigsister.exe` on Windows), not `shaphan`. Scripts that run `shaphan` run `bigsister` instead.
- Release archives are named `bigsister-0.2.0-<target>` and hold `bigsister`, where v0.1.0's were `shaphansaudit-0.1.0-<target>` holding `shaphan`. Install it where `shaphan` was, then delete `shaphan`.
- From source, the Cargo package is `bigsister`: `cargo install --locked --git https://github.com/yungSNAT/BigSister bigsister`, then `cargo uninstall shaphansaudit`.
- The `--json` output's `tool` field is `bigsister`, and the text report's first line starts with `bigsister`.
- To check a release's provenance, pass `--repo yungSNAT/BigSister` to `gh attestation verify`; v0.1.0's provenance still names `yungSNAT/ShaphansAudit`.
- A clone keeps working through the redirect; `git remote set-url origin https://github.com/yungSNAT/BigSister.git` points it at the new address.

BigSister keeps no configuration, cache or state file and reads no environment variable of its own, so nothing saved under the old name needs renaming.

# v0.1.0

First release of `shaphan`, which finds where a coding agent's tokens go, from the agent's own Claude Code and Codex transcripts. It prints names, paths, counts, sizes and times, never message text, tool inputs or tool outputs, and makes no network calls.

There are executables for Linux (x86_64, statically linked) and Windows (x86_64), with `SHA256SUMS` and signed build provenance; on macOS, install from source with Cargo. The README's Install section gives the download and checksum steps.
