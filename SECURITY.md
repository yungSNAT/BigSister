# Security

BigSister reads the transcripts your coding agents save on your machine. Its report is limited to names, paths, counts, sizes and times, leaving out message text, tool inputs and tool outputs, and it makes no network calls, so a flaw that breaks either limit matters.

Please report a vulnerability privately through this repository's private vulnerability reporting (Security tab, 'Report a vulnerability', or <https://github.com/yungSNAT/BigSister/security/advisories/new>), not in a public issue. Include the version or commit, your platform, what you did and what you saw, with any transcript content removed. You will get an acknowledgement, and a fix or a decision, as soon as the maintainer can review it; there is no fixed response time.

In scope: output that carries message text, a tool input, a tool output, or a credential, key or token; any network call; a transcript or configuration file that makes `bigsister` run code or write a file you did not ask for; or a release archive that does not match its `SHA256SUMS` line or its signed build provenance.

Out of scope:

- A program already running as you on the same machine. It can read the same transcripts.
- The names and paths the report prints by design, as the README lists under Privacy.
- What a coding agent does, or what it saves in its own transcripts. Report that to its project.

## Supported versions

The latest release. Fixes land on `main` and ship in the next release.
