//! Compaction health metrics: post/pre retention ratio (flagging > 95% drops),
//! compaction thrashing (< 10% saved across 2+ consecutive passes) and immediate
//! post-compaction file-churn/revert loops.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};

use super::startup::input_tokens;
use super::{input_str, name_of};
use crate::report::{Opts, Table};
use crate::scan::{Agent, Record, Session};
use crate::shell::normalise_path;

/// Turns immediately following a compaction boundary in which re-reading or re-editing
/// a previously edited file counts as post-compaction churn or revert activity.
const POST_COMPACT_WINDOW: u64 = 5;

const CLAUDE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

const HEALTH_COLUMNS: [&str; 11] = [
    "agent",
    "scope",
    "compactions",
    "measured",
    "median_post_pre_ratio",
    "min_post_pre_ratio",
    "extreme_loss_gt95",
    "thrashing_passes",
    "churn_rereads",
    "churn_reedits",
    "revert_cycles",
];

#[derive(Default)]
struct Group {
    compactions: u64,
    measured: u64,
    ratios: Vec<f64>,
    extreme_loss: u64,
    thrashing_passes: u64,
    churn_rereads: u64,
    churn_reedits: u64,
    revert_cycles: u64,
}

/// Per-session and aggregate compaction health tracking, shared with [`super::rereads::Rereads`].
#[derive(Default)]
pub(super) struct Health {
    pending_pre: Option<(u64, bool)>,
    low_savings_streak: u64,
    has_compacted: bool,
    post_turns: u64,
    last_msg_id: Option<String>,
    edited_before: HashSet<String>,
    edited_current: HashSet<String>,
    post_reread_edited: HashSet<String>,
    post_reedited: HashSet<String>,
    groups: BTreeMap<(Agent, &'static str), Group>,
}

impl Health {
    pub(super) fn begin(&mut self) {
        self.pending_pre = None;
        self.low_savings_streak = 0;
        self.has_compacted = false;
        self.post_turns = 0;
        self.last_msg_id = None;
        self.edited_before.clear();
        self.edited_current.clear();
        self.post_reread_edited.clear();
        self.post_reedited.clear();
    }

    pub(super) fn branch(&mut self) {
        self.pending_pre = None;
        self.low_savings_streak = 0;
        self.has_compacted = false;
        self.post_turns = 0;
        self.last_msg_id = None;
        self.edited_before.clear();
        self.edited_current.clear();
        self.post_reread_edited.clear();
        self.post_reedited.clear();
    }

    pub(super) fn wants_claude(&self, line: &str) -> bool {
        (self.pending_pre.is_some()
            || (self.has_compacted && self.post_turns <= POST_COMPACT_WINDOW))
            && line.contains("\"type\":\"assistant\"")
    }

    pub(super) fn on_boundary(&mut self, s: &Session, r: &Record<'_>) {
        if r.in_window {
            self.groups
                .entry((s.agent, s.scope()))
                .or_default()
                .compactions += 1;
        }
        if self.pending_pre.take().is_some() {
            self.low_savings_streak = 0;
        }
        let meta = r.value.get("compactMetadata");
        let pre = meta
            .and_then(|m| m.get("preTokens"))
            .and_then(Value::as_u64)
            .filter(|&p| p > 0);
        let post = meta
            .and_then(|m| m.get("postTokens"))
            .and_then(Value::as_u64);
        match pre {
            Some(pre) => match post {
                Some(post) => self.record_measurement(s, pre, post, r.in_window),
                None => self.pending_pre = Some((pre, r.in_window)),
            },
            None => self.low_savings_streak = 0,
        }
        let current = std::mem::take(&mut self.edited_current);
        self.edited_before.extend(current);
        self.has_compacted = true;
        self.post_turns = 0;
        self.last_msg_id = None;
        self.post_reread_edited.clear();
        self.post_reedited.clear();
    }

    pub(super) fn on_assistant(&mut self, s: &Session, r: &Record<'_>) {
        if r.kind() != "assistant"
            || r.value.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
        {
            return;
        }
        let msg_id = r.value.pointer("/message/id").and_then(Value::as_str);
        if msg_id.is_none() || self.last_msg_id.as_deref() != msg_id {
            self.last_msg_id = msg_id.map(str::to_owned);
            if self.has_compacted {
                self.post_turns += 1;
            }
        }
        if let Some((pre, in_win)) = self.pending_pre
            && let Some(post) = input_tokens(r.value)
        {
            self.pending_pre = None;
            self.record_measurement(s, pre, post, in_win);
        }
    }

    pub(super) fn on_codex_turn(&mut self) {
        if self.has_compacted {
            self.post_turns += 1;
        }
    }

    pub(super) fn on_read(&mut self, s: &Session, norm_path: &str, in_window: bool) {
        if !in_window || !self.in_post_window() || !self.edited_before.contains(norm_path) {
            return;
        }
        if self.post_reread_edited.insert(norm_path.to_owned()) {
            self.groups
                .entry((s.agent, s.scope()))
                .or_default()
                .churn_rereads += 1;
        }
    }

    pub(super) fn on_edit(&mut self, s: &Session, raw_path: &str, in_window: bool) {
        let path = normalise_path(raw_path);
        if path.is_empty() {
            return;
        }
        if in_window
            && self.in_post_window()
            && self.edited_before.contains(&path)
            && self.post_reedited.insert(path.clone())
        {
            let g = self.groups.entry((s.agent, s.scope())).or_default();
            g.churn_reedits += 1;
            if self.post_reread_edited.contains(&path) {
                g.revert_cycles += 1;
            }
        }
        self.edited_current.insert(path);
    }

    pub(super) fn summary(&self) -> Vec<(String, Value)> {
        let sum = |f: fn(&Group) -> u64| self.groups.values().map(f).sum::<u64>();
        vec![
            (
                "compactions_measured".to_owned(),
                json!(sum(|g| g.measured)),
            ),
            (
                "extreme_loss_compactions".to_owned(),
                json!(sum(|g| g.extreme_loss)),
            ),
            (
                "compaction_thrashing_passes".to_owned(),
                json!(sum(|g| g.thrashing_passes)),
            ),
            (
                "post_compaction_churn_rereads".to_owned(),
                json!(sum(|g| g.churn_rereads)),
            ),
            (
                "post_compaction_churn_reedits".to_owned(),
                json!(sum(|g| g.churn_reedits)),
            ),
            (
                "post_compaction_revert_cycles".to_owned(),
                json!(sum(|g| g.revert_cycles)),
            ),
        ]
    }

    pub(super) fn table(&self, opts: &Opts) -> Table {
        let mut table = Table::new("Compaction health", HEALTH_COLUMNS.to_vec());
        for ((agent, scope), g) in &self.groups {
            table.rows.push(vec![
                json!(agent.name()),
                json!(scope),
                json!(g.compactions),
                json!(g.measured),
                median_ratio(&g.ratios),
                min_ratio(&g.ratios),
                json!(g.extreme_loss),
                json!(g.thrashing_passes),
                json!(g.churn_rereads),
                json!(g.churn_reedits),
                json!(g.revert_cycles),
            ]);
        }
        table.truncate(opts.limit);
        table
    }

    fn in_post_window(&self) -> bool {
        self.has_compacted && self.post_turns <= POST_COMPACT_WINDOW
    }

    fn record_measurement(&mut self, s: &Session, pre: u64, post: u64, in_window: bool) {
        let low_savings = u128::from(post) * 10 > u128::from(pre) * 9;
        if low_savings {
            self.low_savings_streak += 1;
        } else {
            self.low_savings_streak = 0;
        }
        if !in_window {
            return;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let ratio = post as f64 / pre as f64;
        let g = self.groups.entry((s.agent, s.scope())).or_default();
        g.measured += 1;
        g.ratios.push(ratio);
        if u128::from(post) * 20 < u128::from(pre) {
            g.extreme_loss += 1;
        }
        if self.low_savings_streak >= 2 {
            g.thrashing_passes += 1;
        }
    }
}

/// File paths edited by one Claude-shaped `tool_use` block.
pub(super) fn claude_edit_paths(b: &Value) -> Vec<String> {
    if !CLAUDE_EDIT_TOOLS.contains(&name_of(b)) {
        return Vec::new();
    }
    input_str(b, "file_path")
        .or_else(|| input_str(b, "notebook_path"))
        .map(str::to_owned)
        .into_iter()
        .collect()
}

/// File paths patched by a Codex `apply_patch` call.
pub(super) fn codex_patch_paths(r: &Record<'_>) -> Vec<String> {
    let Some(raw) = r
        .value
        .get("payload")
        .filter(|p| {
            p.get("type").and_then(Value::as_str) == Some("function_call")
                && name_of(p) == "apply_patch"
        })
        .and_then(|p| p.get("arguments"))
        .and_then(Value::as_str)
    else {
        return Vec::new();
    };
    let text = serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("input")
                .or_else(|| v.get("patch"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| raw.to_owned());
    patch_headers(&text)
}

fn patch_headers(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            line.strip_prefix("*** Update File: ")
                .or_else(|| line.strip_prefix("*** Add File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("*** Move to: "))
                .map(|p| p.trim().to_owned())
        })
        .collect()
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

fn median_ratio(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!(round3(sorted[(sorted.len() - 1) / 2]))
}

fn min_ratio(values: &[f64]) -> Value {
    values
        .iter()
        .copied()
        .min_by(f64::total_cmp)
        .map_or(Value::Null, |v| json!(round3(v)))
}

/// Note explaining compaction health metrics.
pub(super) const HEALTH_NOTE: &str = "Compaction health measures post/pre retention ratio (postTokens when recorded on the boundary, else the next non-synthetic reply's input tokens, divided by preTokens), flagging extreme loss (> 95% dropped, post/pre < 0.05) and compaction thrashing (< 10% saved across 2 or more consecutive passes). Churn rereads, churn reedits and revert cycles count files edited before a compaction that are re-read, re-edited or re-read then re-edited within 5 turns after the compaction.";
