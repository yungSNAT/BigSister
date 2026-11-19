//! Sample transcripts built into the binary, for `--sample`: the repository's synthetic
//! test fixtures, so a first run shows every section without transcripts of its own.

use crate::scan::{Agent, Memory};

/// Where `--sample` reads from, as the report shows it.
pub const ROOT: &str = "(bundled sample)";

/// The window the sample's records fall in: local midnight, as `--since` reads a date.
pub const SINCE: &str = "2026-11-01";

macro_rules! fixture {
    ($agent:expr, $root:literal, $path:literal) => {
        Memory {
            agent: $agent,
            path: $path,
            text: include_str!(concat!("../tests/fixtures/", $root, "/", $path)),
        }
    };
}

/// shortcut: the sample is the test fixtures themselves, so a fixture changed for a test
/// changes the sample too; `tests/cli.rs` checks that `--sample` reports what the fixtures
/// do. A sample of its own would be the upgrade if the two need to differ.
pub const FILES: &[Memory] = &[
    fixture!(Agent::Claude, "claude", "proj-a/s-main.jsonl"),
    fixture!(
        Agent::Claude,
        "claude",
        "proj-a/s-main/subagents/agent-1.jsonl"
    ),
    fixture!(Agent::Claude, "claude", "proj-a/s-old.jsonl"),
    fixture!(Agent::Claude, "context/claude", "proj-x/s-ctx.jsonl"),
    fixture!(
        Agent::Claude,
        "context/claude",
        "proj-x/s-ctx/subagents/agent-1.jsonl"
    ),
    fixture!(Agent::Codex, "codex", "2026/11/08/rollout-main.jsonl"),
    fixture!(Agent::Codex, "codex", "2026/11/08/rollout-sub.jsonl"),
    fixture!(
        Agent::Pi,
        "pi",
        "--work-proj--/2026-11-09T06-00-00-000Z_0b6f2a4e-0000-4000-8000-000000000001.jsonl"
    ),
    fixture!(
        Agent::Pi,
        "pi",
        "--work-proj--/2026-11-09T06-30-00-000Z_0b6f2a4e-0000-4000-8000-000000000002.jsonl"
    ),
];
