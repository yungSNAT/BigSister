//! Files read again after a context compaction that were read before it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::compaction::{self, Health};
use super::tools::printed;
use super::{
    ClaudeCalls, CodexCalls, EST_NOTE, codex_call, codex_call_line, codex_output, est_tokens,
    input_str, name_of, result_chars, tool_results, tool_uses,
};
use crate::prices::{self, Priced};
use crate::report::{Opts, Section, Table, display_path, median, ratio};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{normalise_path, read_targets};

const COLUMNS: [&str; 12] = [
    "agent",
    "scope",
    "sessions_compacted",
    "compactions",
    "reads",
    "rereads",
    "reread_chars",
    "reread_tokens_est",
    "reread_usd_est",
    "unpriced_tokens_est",
    "rereads_per_compaction",
    "reread_share",
];

/// The columns of the table by compaction trigger.
const TRIGGER_COLUMNS: [&str; 11] = [
    "agent",
    "trigger",
    "compactions",
    "median_pre_compaction_tokens",
    "max_pre_compaction_tokens",
    "rereads",
    "reread_chars",
    "reread_tokens_est",
    "reread_usd_est",
    "unpriced_tokens_est",
    "rereads_per_compaction",
];

/// The trigger of a compaction whose record names none.
const NOT_RECORDED: &str = "not recorded";

/// Compactions of one trigger, and the re-reads after them.
#[derive(Default)]
struct Trigger {
    compactions: u64,
    /// The `preTokens` of each compaction that records it.
    // nosemgrep: codacy.yaml.security.hard-coded-tokens -- a count of model tokens, not a credential
    pre_tokens: Vec<u64>,
    rereads: u64,
    reread_chars: u64,
    reread_priced: Priced,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    sessions: u64,
    compactions: u64,
    reads: u64,
    rereads: u64,
    reread_chars: u64,
    /// The part of `reread_chars` a priced model read.
    reread_priced: Priced,
}

/// Re-reads per compaction.
#[derive(Default)]
pub struct Rereads {
    before: HashSet<String>,
    current: HashSet<String>,
    compacted_in_window: bool,
    /// Claude calls that re-read and whose result has not been read yet, by
    /// `tool_use` id.
    claude: ClaudeCalls<String>,
    /// Codex `exec` calls that re-read and whose output has not been read yet.
    codex: CodexCalls<()>,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    paths: HashMap<String, u64>,
    /// Re-read characters of this file that no model has read yet, with their rows.
    unread: Vec<(Agent, &'static str, String, u64)>,
    /// The trigger of the session's latest compaction, which its re-reads count toward.
    trigger: String,
    triggers: BTreeMap<(Agent, String), Trigger>,
    health: Health,
}

fn rereads_prefilter(s: &Session, line: &str) -> bool {
    match s.agent {
        Agent::Codex => {
            line.contains("\"type\":\"compacted\"")
                || super::tool_prefilter(s, line)
                || super::codex_turn_line(line)
        }
        _ => {
            line.contains("\"compact_boundary\"")
                || line.contains("\"type\":\"assistant\"")
                || super::tool_prefilter(s, line)
        }
    }
}

impl Detector for Rereads {
    fn agents(&self) -> &'static [Agent] {
        &[
            Agent::Claude,
            Agent::Codex,
            Agent::Pi,
            Agent::OpenCode,
            Agent::Qwen,
        ]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen => {
                line.contains("\"compact_boundary\"")
                    || line.contains("\"tool_use\"")
                    || self.claude.wants(line)
                    || self.health.wants_claude(line)
            }
            Agent::Codex => {
                line.contains("\"type\":\"compacted\"")
                    || codex_call_line(line)
                    || self.codex.wants(line)
                    || self.health.wants_codex(line)
            }
        }
    }

    fn scans_whole_file(&self) -> bool {
        true
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Superset(rereads_prefilter))
    }

    fn begin(&mut self, _s: &Session) {
        self.before.clear();
        self.current.clear();
        self.compacted_in_window = false;
        self.claude.clear();
        self.unread.clear();
        self.trigger.clear();
        self.health.begin();
    }

    fn branch(&mut self, _s: &Session) {
        // The session stays compacted if its left path was.
        self.before.clear();
        self.current.clear();
        self.claude.clear();
        self.unread.clear();
        self.health.branch();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>, _in_window: bool) {
        for (agent, scope, trigger, n) in self.unread.drain(..) {
            let c = self.totals.entry((agent, scope)).or_default();
            c.reread_priced.add(n, usd_per_mtok);
            let t = self.triggers.entry((agent, trigger)).or_default();
            t.reread_priced.add(n, usd_per_mtok);
        }
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        let boundary = match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen => {
                r.kind() == "system"
                    && r.value.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
            }
            Agent::Codex => r.kind() == "compacted",
        };
        if boundary {
            self.boundary(s, r);
            return;
        }
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen => {
                self.health.on_assistant(s, r);
                for b in tool_uses(r) {
                    let id = b.get("id").and_then(Value::as_str);
                    let fresh = self.claude.note(id);
                    let mut reread = false;
                    for path in claude_read_paths(b) {
                        reread |= self.read(s, &path, r.in_window);
                    }
                    for path in compaction::claude_edit_paths(b) {
                        self.health.on_edit(s, &path, r.in_window);
                    }
                    if reread
                        && fresh
                        && let Some(id) = id
                    {
                        self.claude.wait(id, self.trigger.clone());
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if let Some(trigger) = self.claude.take(id) {
                        let size = result_chars(b.get("content").unwrap_or(&Value::Null));
                        self.returned(s, &trigger, size);
                    }
                }
            }
            Agent::Codex => self.codex(s, r),
        }
    }

    fn end(&mut self, s: &Session) {
        self.codex.end();
        if self.compacted_in_window {
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .sessions += 1;
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut table = Table::new("By agent and scope", COLUMNS.to_vec());
        for ((agent, scope), c) in &self.totals {
            table.rows.push(vec![
                json!(agent.name()),
                json!(scope),
                json!(c.sessions),
                json!(c.compactions),
                json!(c.reads),
                json!(c.rereads),
                json!(c.reread_chars),
                json!(est_tokens(c.reread_chars)),
                json!(c.reread_priced.usd()),
                json!(c.reread_priced.unpriced_tokens(c.reread_chars)),
                ratio(c.rereads, c.compactions),
                ratio(c.rereads, c.reads),
            ]);
        }
        table.truncate(opts.limit);
        let mut tables = vec![table];
        if opts.paths {
            tables.push(self.path_table(opts));
        }
        tables.push(self.trigger_table(opts));
        tables.push(self.health.table(opts));
        Section {
            name: "rereads",
            title: "files read again after a context compaction",
            summary: std::iter::once((
                "codex_outputs_not_paired".to_owned(),
                json!(self.codex.unpaired),
            ))
            .chain(self.health.summary())
            .collect(),
            tables,
            notes: vec![
                "A re-read is a file read after a compaction that was read before it and not yet since.".to_owned(),
                "Reread share divides re-reads by every read in the window, in compacted sessions or not.".to_owned(),
                compaction::HEALTH_NOTE.to_owned(),
                "By compaction trigger: the trigger is the compactMetadata trigger Claude Code records on each compaction, such as auto or manual, printed as tool names are ((other) for any other text); Codex, pi, OpenCode and Qwen record none. Pre-compaction tokens are the preTokens it records, measured, not estimated: the median (by nearest rank, so the lower of 2 middle values) and largest of the row's compactions that record them; a row none of whose compactions records them shows null. A re-read, with its result, counts toward the trigger of the latest compaction before its call in its session.".to_owned(),
                "Reread chars are the characters of text in the results of calls that re-read, each call counted once; a result holds what the tool returned, which can be more or less than the file. A Codex call's result is all the text its exec cell printed; Codex outputs not paired counts the calls in the window that re-read but were a function_call, whose output is not read, or an exec cell that had no call id, shared it with another exec cell in the session or whose output never came.".to_owned(),
                EST_NOTE.to_owned(),
                prices::NOTE.to_owned(),
            ],
        }
    }
}

impl Rereads {
    /// Unrounded aggregate re-read waste USD across all agent/scope totals.
    pub(crate) fn exact_reread_usd(&self, rate: Option<f64>) -> f64 {
        self.totals
            .values()
            .map(|c| c.reread_priced.exact_usd(c.reread_chars, rate))
            .sum()
    }

    /// Reads a Codex call's reads, or the output of a call that re-read.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        self.health.on_codex_record(r);
        if let Some(call) = codex_call(r) {
            for path in compaction::codex_patch_paths(r) {
                self.health.on_edit(s, &path, r.in_window);
            }
            let from_commands = call.commands().flat_map(read_targets);
            let mut reread = false;
            for path in from_commands.chain(call.reads.iter().cloned()) {
                reread |= self.read(s, &path, r.in_window);
            }
            self.codex.call(r, &call, reread.then(Vec::new));
        } else if !self.codex.record(r)
            && let Some(out) = codex_output(r)
            && self.codex.take(&out)
        {
            // Codex records no trigger, so its current one is the call's.
            let trigger = self.trigger.clone();
            self.returned(s, &trigger, out.chars);
        }
    }

    fn boundary(&mut self, s: &Session, r: &Record<'_>) {
        self.health.on_boundary(s, r);
        let meta = r.value.get("compactMetadata");
        self.trigger = meta
            .and_then(|m| m.get("trigger"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map_or_else(|| NOT_RECORDED.to_owned(), printed);
        if r.in_window {
            self.compacted_in_window = true;
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .compactions += 1;
            let t = self.trigger_counts(s);
            t.compactions += 1;
            t.pre_tokens.extend(
                meta.and_then(|m| m.get("preTokens"))
                    .and_then(Value::as_u64),
            );
        }
        let current = std::mem::take(&mut self.current);
        self.before.extend(current);
    }

    /// Adds the size of the result of a call that re-read under `trigger`, the trigger of
    /// the compaction before the call.
    fn returned(&mut self, s: &Session, trigger: &str, size: u64) {
        self.totals
            .entry((s.agent, s.scope()))
            .or_default()
            .reread_chars += size;
        self.triggers
            .entry((s.agent, trigger.to_owned()))
            .or_default()
            .reread_chars += size;
        self.unread
            .push((s.agent, s.scope(), trigger.to_owned(), size));
    }

    /// Notes one read and says whether it is a re-read counted in the window.
    fn read(&mut self, s: &Session, raw: &str, in_window: bool) -> bool {
        let path = normalise_path(raw);
        if path.is_empty() {
            return false;
        }
        self.health.on_read(s, &path, in_window);
        let mut reread = false;
        if in_window {
            let c = self.totals.entry((s.agent, s.scope())).or_default();
            c.reads += 1;
            if !self.current.contains(&path) && self.before.contains(&path) {
                c.rereads += 1;
                *self.paths.entry(path.clone()).or_default() += 1;
                reread = true;
            }
        }
        if reread {
            self.trigger_counts(s).rereads += 1;
        }
        self.current.insert(path);
        reread
    }

    /// The counts of the trigger of the session's latest compaction.
    fn trigger_counts(&mut self, s: &Session) -> &mut Trigger {
        self.triggers
            .entry((s.agent, self.trigger.clone()))
            .or_default()
    }

    /// Compactions and the re-reads after them by agent and trigger, most compactions
    /// first.
    fn trigger_table(&self, opts: &Opts) -> Table {
        let mut triggers: Vec<_> = self.triggers.iter().collect();
        triggers.sort_by(|a, b| {
            b.1.compactions
                .cmp(&a.1.compactions)
                .then_with(|| a.0.cmp(b.0))
        });
        let mut table = Table::new("By compaction trigger", TRIGGER_COLUMNS.to_vec());
        table.rows = triggers
            .into_iter()
            .map(|((agent, trigger), t)| {
                let pre = |v: u64| {
                    if t.pre_tokens.is_empty() {
                        Value::Null
                    } else {
                        json!(v)
                    }
                };
                vec![
                    json!(agent.name()),
                    json!(trigger),
                    json!(t.compactions),
                    pre(median(&t.pre_tokens)),
                    pre(t.pre_tokens.iter().copied().max().unwrap_or(0)),
                    json!(t.rereads),
                    json!(t.reread_chars),
                    json!(est_tokens(t.reread_chars)),
                    json!(t.reread_priced.usd()),
                    json!(t.reread_priced.unpriced_tokens(t.reread_chars)),
                    ratio(t.rereads, t.compactions),
                ]
            })
            .collect();
        table.truncate(opts.limit);
        table
    }

    fn path_table(&self, opts: &Opts) -> Table {
        let mut paths: Vec<(&String, &u64)> = self.paths.iter().collect();
        paths.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let mut table = Table::new("Most re-read paths", vec!["path", "rereads"]);
        table.rows = paths
            .into_iter()
            .map(|(p, n)| vec![json!(display_path(p, opts.home.as_deref())), json!(n)])
            .collect();
        table.truncate(opts.limit);
        table
    }
}

/// The paths one Claude `tool_use` block reads.
fn claude_read_paths(b: &Value) -> Vec<String> {
    match name_of(b) {
        "Read" => input_str(b, "file_path")
            .map(str::to_owned)
            .into_iter()
            .collect(),
        "Bash" => read_targets(input_str(b, "command").unwrap_or("")),
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[path = "rereads_tests.rs"]
mod tests;
