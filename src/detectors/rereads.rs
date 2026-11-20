//! Files read again after a context compaction that were read before it.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    ClaudeCalls, CodexCalls, EST_NOTE, codex_call, codex_call_line, codex_output, est_tokens,
    input_str, name_of, result_chars, tool_results, tool_uses,
};
use crate::prices::{self, Priced};
use crate::report::{Opts, Section, Table, display_path, ratio};
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
    unread: Vec<(Agent, &'static str, u64)>,
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

    fn begin(&mut self, _s: &Session) {
        self.before.clear();
        self.current.clear();
        self.compacted_in_window = false;
        self.claude.clear();
        self.unread.clear();
    }

    fn branch(&mut self, _s: &Session) {
        // The session stays compacted if its left path was.
        self.before.clear();
        self.current.clear();
        self.claude.clear();
        self.unread.clear();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
        for (agent, scope, n) in self.unread.drain(..) {
            let c = self.totals.entry((agent, scope)).or_default();
            c.reread_priced.add(n, usd_per_mtok);
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
            self.boundary(s, r.in_window);
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

    fn boundary(&mut self, s: &Session, in_window: bool) {
        if in_window {
            self.compacted_in_window = true;
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .compactions += 1;
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
        self.unread.push((s.agent, s.scope(), size));
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
        self.current.insert(path);
        reread
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
