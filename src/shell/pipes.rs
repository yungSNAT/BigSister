//! Test runners piped into `tail` or `head`, and the markers of their outcome.

use std::sync::LazyLock;

use regex::Regex;

use super::{command_words, is_assignment, program, split, ssh_remote};

/// A test runner at the start of a command, optionally behind a package runner.
static RUNNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?:npx|bunx|uv run|poetry run|python3? -m|py -m) )?(?:cargo (?:nextest run|test)|pytest|vitest|jest|node --test|npm (?:run )?test|go test|dotnet test)\b",
    )
    .expect("valid regex")
});
// shortcut: failure and pass markers cover cargo, pytest, TAP and node output; other
// runners' wording shows as "no pass or fail line". Extend the patterns per runner.
static FAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"test result: FAILED|\bFAILED\b|\b[1-9]\d* failed|# fail [1-9]|\bnot ok \d|panicked at|error: test failed|error: could not compile",
    )
    .expect("valid regex")
});
static PASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)test result: ok|passed|# pass|\bok\b").expect("valid regex"));

/// How a command runs tests, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PipeCheck {
    /// Runs a known test runner.
    pub runner: bool,
    /// Pipes the runner's output straight into `tail` or `head`.
    pub piped: bool,
    /// The piped runner runs over `ssh`, where the remote shell's settings are unknown.
    pub remote: bool,
    /// Sets `pipefail` or reads `PIPESTATUS`.
    pub guarded: bool,
}

/// Judges each segment of a command: a runner whose segment pipes into `tail` or
/// `head`, or an `ssh` segment whose remote command does.
#[must_use]
pub fn pipe_check(cmd: &str) -> PipeCheck {
    let segs = split(cmd);
    let mut check = PipeCheck {
        guarded: segs
            .iter()
            .flat_map(|s| &s.words)
            .any(|w| w.contains("pipefail") || w.contains("PIPESTATUS")),
        ..PipeCheck::default()
    };
    for (i, seg) in segs.iter().enumerate() {
        let words = command_words(&seg.words);
        let into_tail = seg.piped
            && segs.get(i + 1).is_some_and(|next| {
                command_words(&next.words)
                    .first()
                    .is_some_and(|w| matches!(program(w).as_str(), "tail" | "head"))
            });
        let remote = match words.first() {
            Some(w) if program(w) == "ssh" => ssh_remote(&words[1..]).map(|r| pipe_check(&r)),
            _ => None,
        };
        match remote {
            Some(inner) if inner.runner => {
                check.runner = true;
                if into_tail {
                    check.piped = true;
                } else if inner.piped {
                    check.piped = true;
                    check.remote = true;
                }
            }
            Some(_) => {}
            None if runs_tests(&words) => {
                check.runner = true;
                check.piped |= into_tail;
            }
            None => {}
        }
    }
    check
}

/// Programs that run the command after them, as in `agent-heavy -- cargo test`.
const WRAPPERS: [&str; 9] = [
    "agent-heavy",
    "nice",
    "timeout",
    "env",
    "nohup",
    "time",
    "sudo",
    "do",
    "then",
];

/// Whether a simple command runs a test runner, directly or behind wrappers with
/// their options and numeric arguments, such as `timeout 600` or `rtk test`.
fn runs_tests(words: &[&str]) -> bool {
    let mut j = 0;
    while j < words.len() {
        if RUNNER.is_match(&command_line(&words[j..])) {
            return true;
        }
        let w = words[j];
        let p = program(w);
        if p == "rtk"
            && words
                .get(j + 1)
                .is_some_and(|n| matches!(*n, "test" | "err" | "bypass" | "proxy"))
        {
            j += 2;
        } else if WRAPPERS.contains(&p.as_str())
            || w.starts_with('-')
            || is_assignment(w)
            || is_duration(w)
        {
            j += 1;
        } else {
            return false;
        }
    }
    false
}

/// A number with an optional `s`, `m`, `h` or `d` suffix, such as `600` or `10m`.
fn is_duration(w: &str) -> bool {
    let digits = w.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Words joined by spaces, with the first as a bare program name.
fn command_line(words: &[&str]) -> String {
    let mut line = words.first().map(|w| program(w)).unwrap_or_default();
    for w in words.iter().skip(1) {
        line.push(' ');
        line.push_str(w);
    }
    line
}

/// Whether test output shows a failure.
#[must_use]
pub fn shows_failure(text: &str) -> bool {
    FAIL.is_match(text)
}

/// Whether test output shows a pass.
#[must_use]
pub fn shows_pass(text: &str) -> bool {
    PASS.is_match(text)
}
