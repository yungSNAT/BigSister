//! Identical shell commands re-run within 1-2 turns of a truncated or stashed tool output.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::est_tokens;
use crate::prices::Priced;
use crate::report::{Opts, Table};
use crate::scan::{Agent, Record};

/// Maximum turn gap after a truncated/stashed output for an identical repeat to count.
const MAX_TURN_GAP: u64 = 2;

/// Bytes scanned at the start and end of a tool result for truncation or stash markers.
const SCAN_WINDOW: usize = 4096;

const MARKERS: [&str; 17] = [
    "<persisted-output>",
    "output too long",
    "output truncated",
    "output was truncated",
    "[truncated",
    "[output truncated",
    "lines truncated",
    "characters truncated",
    "chars truncated",
    "bytes truncated",
    "lines omitted",
    "characters omitted",
    "chars omitted",
    "bytes omitted",
    "output saved to",
    "full output saved to",
    "full output stashed",
];

#[derive(Default)]
struct VerbRow {
    truncated_outputs: u64,
    reruns: u64,
    rerun_chars: u64,
    rerun_priced: Priced,
}

/// Tracks truncated/stashed command outputs and identical repeats within 1-2 turns.
#[derive(Default)]
pub(super) struct Tracker {
    turn: u64,
    last_msg_id: Option<String>,
    stashed: HashMap<String, u64>,
    unread: Vec<(Agent, String, u64)>,
    by_verb: BTreeMap<(Agent, String), VerbRow>,
}

impl Tracker {
    pub(super) fn begin(&mut self) {
        self.turn = 0;
        self.last_msg_id = None;
        self.stashed.clear();
        self.unread.clear();
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.stashed.is_empty()
    }

    pub(super) fn note_claude_turn(&mut self, r: &Record<'_>) {
        if r.kind() != "assistant"
            || r.value.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
        {
            return;
        }
        let msg_id = r.value.pointer("/message/id").and_then(Value::as_str);
        if msg_id.is_none() || self.last_msg_id.as_deref() != msg_id {
            self.last_msg_id = msg_id.map(str::to_owned);
            self.turn += 1;
        }
    }

    pub(super) fn next_turn(&mut self) {
        self.turn += 1;
    }

    /// Checks whether `cmd` repeats a command whose output was truncated within 1-2 turns.
    pub(super) fn observe_command(&mut self, agent: Agent, cmd: &str, class: Option<&str>) -> bool {
        let Some(&prev_turn) = self.stashed.get(cmd) else {
            return false;
        };
        self.stashed.remove(cmd);
        if self.turn.saturating_sub(prev_turn) > MAX_TURN_GAP {
            return false;
        }
        if let Some(verb) = class {
            self.by_verb
                .entry((agent, verb.to_owned()))
                .or_default()
                .reruns += 1;
        }
        true
    }

    /// Records the result of an awaited command for truncation and re-run sizing.
    pub(super) fn observe_result(
        &mut self,
        agent: Agent,
        cmd: &str,
        class: Option<&str>,
        rerun: bool,
        truncated: bool,
        size: u64,
    ) {
        if truncated {
            self.stashed.insert(cmd.to_owned(), self.turn);
            if let Some(verb) = class {
                self.by_verb
                    .entry((agent, verb.to_owned()))
                    .or_default()
                    .truncated_outputs += 1;
            }
        }
        if rerun && let Some(verb) = class {
            self.by_verb
                .entry((agent, verb.to_owned()))
                .or_default()
                .rerun_chars += size;
            self.unread.push((agent, verb.to_owned(), size));
        }
    }

    pub(super) fn read_by(&mut self, usd_per_mtok: Option<f64>) {
        for (agent, verb, size) in self.unread.drain(..) {
            self.by_verb
                .entry((agent, verb))
                .or_default()
                .rerun_priced
                .add(size, usd_per_mtok);
        }
    }

    pub(super) fn summary(&self) -> Vec<(String, Value)> {
        let truncated: u64 = self.by_verb.values().map(|r| r.truncated_outputs).sum();
        let reruns: u64 = self.by_verb.values().map(|r| r.reruns).sum();
        vec![
            ("truncated_outputs".to_owned(), json!(truncated)),
            ("truncation_reruns".to_owned(), json!(reruns)),
        ]
    }

    pub(super) fn table(&self, opts: &Opts) -> Table {
        let mut rows: Vec<(u64, u64, Vec<Value>)> = self
            .by_verb
            .iter()
            .map(|((agent, verb), r)| {
                let row = vec![
                    json!(agent.name()),
                    json!(verb),
                    json!(r.truncated_outputs),
                    json!(r.reruns),
                    json!(r.rerun_chars),
                    json!(est_tokens(r.rerun_chars)),
                    json!(r.rerun_priced.usd()),
                    json!(r.rerun_priced.unpriced_tokens(r.rerun_chars)),
                ];
                (r.reruns, r.truncated_outputs, row)
            })
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let mut table = Table::new(
            "Truncation-triggered command re-runs",
            vec![
                "agent",
                "verb",
                "truncated_outputs",
                "truncation_reruns",
                "rerun_chars",
                "rerun_tokens_est",
                "rerun_usd_est",
                "unpriced_tokens_est",
            ],
        );
        table.rows = rows.into_iter().map(|(_, _, r)| r).collect();
        table.truncate(opts.limit);
        table
    }
}

/// Whether a Claude-shaped `tool_result` block holds a truncated or stashed output.
pub(super) fn is_claude_truncated(block: &Value) -> bool {
    if block.get("truncated").and_then(Value::as_bool) == Some(true)
        || block.get("stashed").and_then(Value::as_bool) == Some(true)
        || block
            .get("persistedOutputPath")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    {
        return true;
    }
    match block.get("content") {
        Some(Value::String(s)) => text_looks_truncated(s),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .any(text_looks_truncated),
        _ => false,
    }
}

/// Whether plain command output text carries a truncation or stash marker.
pub(super) fn text_looks_truncated(text: &str) -> bool {
    if text.len() <= SCAN_WINDOW * 2 {
        return has_marker(&text.to_ascii_lowercase());
    }
    let head = &text[..text.floor_char_boundary(SCAN_WINDOW)];
    let tail = &text[text.ceil_char_boundary(text.len() - SCAN_WINDOW)..];
    has_marker(&head.to_ascii_lowercase()) || has_marker(&tail.to_ascii_lowercase())
}

fn has_marker(lower: &str) -> bool {
    if MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    lower.contains("recall ")
        && (lower.contains("omitted")
            || lower.contains("truncated")
            || lower.contains("stashed")
            || lower.contains("full output"))
}

/// Note explaining truncation-triggered command re-runs.
pub(super) const TRUNCATION_NOTE: &str = "Truncation-triggered command re-runs count identical shell commands executed again within 2 turns of a truncated or stashed tool output (structured truncated/stashed/persistedOutputPath fields or bounded output markers such as persisted-output, output too long, omitted/truncated counts, saved/stashed notices or recall hints).";
