//! Files read again after a context compaction that were read before it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

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
    claude: ClaudeCalls<()>,
    /// Codex `exec` calls that re-read and whose output has not been read yet.
    codex: CodexCalls<()>,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    paths: HashMap<String, u64>,
    /// Re-read characters of this file that no model has read yet, with their rows.
    unread: Vec<(Agent, &'static str, String, u64)>,
    /// The trigger of the session's latest compaction, which its re-reads count toward.
    trigger: String,
    triggers: BTreeMap<(Agent, String), Trigger>,
}

fn rereads_prefilter(s: &Session, line: &str) -> bool {
    match s.agent {
        Agent::Codex => line.contains("\"type\":\"compacted\"") || super::tool_prefilter(s, line),
        _ => line.contains("\"compact_boundary\"") || super::tool_prefilter(s, line),
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
            }
            Agent::Codex => {
                line.contains("\"type\":\"compacted\"")
                    || codex_call_line(line)
                    || self.codex.wants(line)
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
    }

    fn branch(&mut self, _s: &Session) {
        // The session stays compacted if its left path was.
        self.before.clear();
        self.current.clear();
        self.claude.clear();
        self.unread.clear();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
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
                for b in tool_uses(r) {
                    let id = b.get("id").and_then(Value::as_str);
                    let fresh = self.claude.note(id);
                    let mut reread = false;
                    for path in claude_read_paths(b) {
                        reread |= self.read(s, &path, r.in_window);
                    }
                    if reread
                        && fresh
                        && let Some(id) = id
                    {
                        self.claude.wait(id, ());
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if self.claude.take(id).is_some() {
                        self.returned(s, result_chars(b.get("content").unwrap_or(&Value::Null)));
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
        Section {
            name: "rereads",
            title: "files read again after a context compaction",
            summary: vec![(
                "codex_outputs_not_paired".to_owned(),
                json!(self.codex.unpaired),
            )],
            tables,
            notes: vec![
                "A re-read is a file read after a compaction that was read before it and not yet since.".to_owned(),
                "Reread share divides re-reads by every read in the window, in compacted sessions or not.".to_owned(),
                "By compaction trigger: the trigger is the compactMetadata trigger Claude Code records on each compaction, such as auto or manual; Codex, pi, OpenCode and Qwen record none. Pre-compaction tokens are the preTokens it records, measured, not estimated; a row none of whose compactions records them shows null. A re-read counts toward the trigger of the latest compaction before it in its session.".to_owned(),
                "Reread chars are the characters of text in the results of calls that re-read, each call counted once; a result holds what the tool returned, which can be more or less than the file. A Codex call's result is all the text its exec cell printed; Codex outputs not paired counts the calls in the window that re-read but were a function_call, whose output is not read, or an exec cell that had no call id, shared it with another exec cell in the session or whose output never came.".to_owned(),
                EST_NOTE.to_owned(),
                prices::NOTE.to_owned(),
            ],
        }
    }
}

impl Rereads {
    /// Reads a Codex call's reads, or the output of a call that re-read.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
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
            self.returned(s, out.chars);
        }
    }

    fn boundary(&mut self, s: &Session, r: &Record<'_>) {
        let meta = r.value.get("compactMetadata");
        meta.and_then(|m| m.get("trigger"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .unwrap_or(NOT_RECORDED)
            .clone_into(&mut self.trigger);
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

    /// Adds the size of the result of a call that re-read.
    fn returned(&mut self, s: &Session, size: u64) {
        self.totals
            .entry((s.agent, s.scope()))
            .or_default()
            .reread_chars += size;
        self.trigger_counts(s).reread_chars += size;
        let trigger = self.trigger.clone();
        self.unread.push((s.agent, s.scope(), trigger, size));
    }

    /// Notes one read and says whether it is a re-read counted in the window.
    fn read(&mut self, s: &Session, raw: &str, in_window: bool) -> bool {
        let path = normalise_path(raw);
        if path.is_empty() {
            return false;
        }
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
mod tests {
    use serde_json::json;

    use super::*;
    use crate::detectors::{claude_tool_result, claude_tool_use, replay};

    #[test]
    fn a_reused_claude_tool_use_id_pairs_with_neither_read() {
        let read = |path: &str| json!({"file_path": path});
        let boundary = json!({"type": "system", "subtype": "compact_boundary"});
        let lines: Vec<(bool, String)> = [
            claude_tool_use("a", "Read", &read("/work/y.md")),
            boundary.to_string(),
            // The re-read and a block reusing its id: the result may be either's.
            claude_tool_use("b", "Read", &read("/work/y.md")),
            claude_tool_use("b", "Bash", &json!({"command": "ls"})),
            claude_tool_result("b", "12345"),
        ]
        .into_iter()
        .map(|l| (true, l))
        .collect();
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Rereads::default();
        replay(&mut d, &s, &lines);
        let c = d.totals[&(Agent::Claude, "main")];
        assert_eq!((c.rereads, c.reread_chars), (1, 0));
    }

    /// The rows of `rereads`' table titled `title`, each with the values of `keys`.
    fn table_cells(d: &Rereads, title: &str, keys: &[&str]) -> Value {
        let opts = Opts {
            limit: 100,
            paths: false,
            home: None,
            usd_per_mtok: None,
            prices_file: None,
        };
        let section = d.section(&opts);
        let table = section
            .tables
            .iter()
            .find(|t| t.title == title)
            .unwrap_or_else(|| panic!("no table {title}"));
        let at = |k: &&str| table.columns.iter().position(|c| c == k).expect("a column");
        table
            .rows
            .iter()
            .map(|row| keys.iter().map(|k| row[at(k)].clone()).collect::<Value>())
            .collect()
    }

    /// A Claude session that compacts by hand twice, automatically once and once
    /// without metadata, reading the same file again after each.
    fn trigger_session() -> Vec<(bool, String)> {
        let read = |id| claude_tool_use(id, "Read", &json!({"file_path": "/work/a.md"}));
        let boundary = |trigger: &str, tokens: u64| {
            json!({"type": "system", "subtype": "compact_boundary",
                   "compactMetadata": {"trigger": trigger, "preTokens": tokens}})
            .to_string()
        };
        vec![
            (true, read("a")),
            // Out of the window: sets the trigger but counts no compaction.
            (false, boundary("manual", 1)),
            (true, boundary("manual", 5000)),
            (true, read("b")),
            (true, claude_tool_result("b", "12345678")),
            (true, boundary("auto", 9000)),
            (true, read("c")),
            (true, boundary("manual", 7000)),
            (true, read("d")),
            // No metadata, as pi's, OpenCode's and Gemini's boundaries have.
            (
                true,
                json!({"type": "system", "subtype": "compact_boundary"}).to_string(),
            ),
            (true, read("e")),
        ]
    }

    #[test]
    fn rereads_count_toward_the_trigger_of_the_latest_compaction() {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Rereads::default();
        replay(&mut d, &s, &trigger_session());
        let keys = [
            "trigger",
            "compactions",
            "median_pre_compaction_tokens",
            "max_pre_compaction_tokens",
            "rereads",
            "reread_chars",
            "rereads_per_compaction",
        ];
        // The median of 2 values is the lower, by nearest rank.
        assert_eq!(
            table_cells(&d, "By compaction trigger", &keys),
            json!([
                ["manual", 2, 5000, 7000, 2, 8, 1.0],
                ["auto", 1, 9000, 9000, 1, 0, 1.0],
                ["not recorded", 1, null, null, 1, 0, 1.0]
            ])
        );
    }

    #[test]
    fn reads_with_no_path_are_not_reads() {
        let read = |id| claude_tool_use(id, "Read", &json!({"file_path": ""}));
        let boundary = json!({"type": "system", "subtype": "compact_boundary"});
        let lines: Vec<(bool, String)> = [read("a"), boundary.to_string(), read("b")]
            .into_iter()
            .map(|l| (true, l))
            .collect();
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Rereads::default();
        replay(&mut d, &s, &lines);
        let counts = d.totals.get(&(Agent::Claude, "main"));
        assert_eq!(counts.map_or((0, 0), |c| (c.reads, c.rereads)), (0, 0));
    }
}
