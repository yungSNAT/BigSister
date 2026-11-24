//! Commands repeated within a session, with or without a file edit in between, and
//! what each verb class's results added to context.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use serde_json::{Value, json};

use super::truncation;
use super::{
    ClaudeCalls, CodexCalls, EST_NOTE, codex_call, codex_call_line, codex_output, est_tokens,
    input_str, name_of, result_chars, tool_results, tool_uses,
};
use crate::prices::{self, Priced};
use crate::programs::Programs;
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};
use crate::shell::{Step, verb_class};

const CLAUDE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

#[derive(Default, Clone, Copy)]
struct Counts {
    commands: u64,
    polls: u64,
    polls_after_failure: u64,
    poll_chars: u64,
    /// The part of `poll_chars` a priced model read.
    poll_priced: Priced,
    reruns_after_edit: u64,
}

impl Counts {
    /// The columns every row shares, after those that key it.
    const COLUMNS: [&'static str; 7] = [
        "polls",
        "polls_after_failure",
        "poll_chars",
        "poll_tokens_est",
        "poll_usd_est",
        "unpriced_tokens_est",
        "reruns_after_edit",
    ];

    /// The cells under `COLUMNS`.
    fn cells(&self) -> [Value; 7] {
        [
            json!(self.polls),
            json!(self.polls_after_failure),
            json!(self.poll_chars),
            json!(est_tokens(self.poll_chars)),
            json!(self.poll_priced.usd()),
            json!(self.poll_priced.unpriced_tokens(self.poll_chars)),
            json!(self.reruns_after_edit),
        ]
    }
}

/// Commands of one verb class and their results.
#[derive(Default)]
struct Results {
    commands: u64,
    results: u64,
    chars: u64,
    /// The part of `chars` a priced model read.
    priced: Priced,
}

/// A run whose result has not been read yet.
struct Awaited {
    command: String,
    /// The verb class, when the run is in the window.
    class: Option<String>,
    /// Whether the run is a poll counted in the window.
    poll: bool,
    /// Whether the run repeats a command within 2 turns of a truncated output.
    truncation_rerun: bool,
    /// The run's number in the scan.
    run: u64,
}

/// What is known of a command's latest run.
#[derive(PartialEq, Eq)]
enum Latest {
    /// Its result, by run number, is awaited.
    Awaited(u64),
    /// Its result was read and reported an error.
    Failed,
}

/// Exact repeats of a command, split by whether a file edit came between.
#[derive(Default)]
pub struct Polls {
    last_seen: HashMap<String, u64>,
    edits: u64,
    /// The runs awaited so far, which number each one.
    runs: u64,
    /// Claude runs whose result has not been read yet, by `tool_use` id.
    claude: ClaudeCalls<Awaited>,
    /// Codex `exec` calls whose output has not been read yet, with each command's run.
    codex: CodexCalls<Option<Awaited>>,
    /// By command, its latest run's awaited result or failure. A command whose latest
    /// run succeeded, or whose result will never be read, has no entry.
    latest: HashMap<String, Latest>,
    /// Poll characters of this file that no model has read yet, with their rows.
    unread: Vec<(Agent, &'static str, String, u64)>,
    totals: BTreeMap<(Agent, &'static str), Counts>,
    verbs: BTreeMap<(Agent, String), Counts>,
    /// By agent and verb class, the commands in the window and their results.
    results: BTreeMap<(Agent, String), Results>,
    /// Result characters of this file that no model has read yet, with their rows.
    unread_results: Vec<(Agent, String, u64)>,
    truncation: truncation::Tracker,
    programs: Rc<Programs>,
}

impl Detector for Polls {
    fn agents(&self) -> &'static [Agent] {
        &[
            Agent::Claude,
            Agent::Codex,
            Agent::Pi,
            Agent::OpenCode,
            Agent::Gemini,
            Agent::Qwen,
        ]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen => {
                line.contains("\"tool_use\"") || self.claude.wants(line)
            }
            Agent::Codex => codex_call_line(line) || self.codex.wants(line),
        }
    }

    fn scans_whole_file(&self) -> bool {
        true
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Superset(super::tool_prefilter))
    }

    fn begin(&mut self, _s: &Session) {
        self.last_seen.clear();
        self.edits = 0;
        self.claude.clear();
        self.latest.clear();
        self.unread.clear();
        self.unread_results.clear();
        self.truncation.begin();
    }

    fn end(&mut self, _s: &Session) {
        self.codex.end();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
        for (agent, scope, class, n) in self.unread.drain(..) {
            let add = |c: &mut Counts| c.poll_priced.add(n, usd_per_mtok);
            add(self.totals.entry((agent, scope)).or_default());
            add(self.verbs.entry((agent, class)).or_default());
        }
        for (agent, class, n) in self.unread_results.drain(..) {
            let r = self.results.entry((agent, class)).or_default();
            r.priced.add(n, usd_per_mtok);
        }
        self.truncation.read_by(usd_per_mtok);
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen => {
                self.truncation.note_claude_turn(r);
                for b in tool_uses(r) {
                    let id = b.get("id").and_then(Value::as_str);
                    let fresh = self.claude.note(id);
                    let name = name_of(b);
                    if name == "Bash" {
                        let cmd = input_str(b, "command").unwrap_or("");
                        if let Some(run) = self.observe(s, cmd, r.in_window, fresh)
                            && let Some(id) = id
                        {
                            self.claude.wait(id, run);
                        }
                    } else if CLAUDE_EDIT_TOOLS.contains(&name) {
                        self.edits += 1;
                    }
                }
                for b in tool_results(r) {
                    let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                    if let Some(run) = self.claude.take(id) {
                        let failed = b.get("is_error").and_then(Value::as_bool) == Some(true);
                        let truncated = truncation::is_claude_truncated(b);
                        self.result(s, run, failed, truncated, || {
                            result_chars(b.get("content").unwrap_or(&Value::Null))
                        });
                    }
                }
            }
            Agent::Codex => self.codex(s, r),
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let summary = [
            (
                "codex_outputs_not_paired".to_owned(),
                json!(self.codex.unpaired),
            ),
            (
                "codex_commands_not_paired".to_owned(),
                json!(self.codex.unpaired_commands),
            ),
        ]
        .into_iter()
        .chain(self.truncation.summary())
        .collect();
        Section {
            name: "polls",
            title: "commands repeated exactly within a session",
            summary,
            tables: vec![
                self.totals_table(opts),
                self.verbs_table(opts),
                self.results_table(opts),
                self.truncation.table(opts),
            ],
            notes: notes(),
        }
    }
}

/// The section's notes.
fn notes() -> Vec<String> {
    vec![
        "A poll repeats a command with no file edit since its last run; a rerun follows an edit.".to_owned(),
        "Edits made through shell commands are not seen, so some polls may be reruns.".to_owned(),
        "Command results count every non-blank command in the window by verb class, polled or not: results are those read, and result chars their characters of text, for Codex only what the exec cell printed of them, which is what reached the model.".to_owned(),
        "Polls after failure repeat a command whose previous run's result was read before the repeat and marked as an error, which a hook's denial also is; poll chars are the characters of text in polls' results, for Codex only of what the exec cell printed of them. A Codex command's result is the CommandExecution record Codex wrote as it ended, tied to the command by its text; a command no record ties takes the result its exec cell printed in its place only when the cell printed one result per command and has one command, Codex wrote no such record whilst the cell ran, or it is the one command left untied and each tied command's result is printed whole in its own place; Codex marks a failure by a non-zero exit code. Codex outputs not paired counts the calls in the window whose results could not all be paired: a function_call, whose output is not read, or an exec cell that had no call id, whose call id another exec cell in the session had, whose output never came or with a command neither tied nor paired. Codex commands not paired counts those commands, whose results were not judged.".to_owned(),
        truncation::TRUNCATION_NOTE.to_owned(),
        EST_NOTE.to_owned(),
        prices::NOTE.to_owned(),
    ]
}

impl Polls {
    fn totals_table(&self, opts: &Opts) -> Table {
        let cols = [&["agent", "scope", "commands"][..], &Counts::COLUMNS[..]].concat();
        let mut totals = Table::new("By agent and scope", cols);
        for ((agent, scope), c) in &self.totals {
            let mut row = vec![json!(agent.name()), json!(scope), json!(c.commands)];
            row.extend(c.cells());
            totals.rows.push(row);
        }
        totals.truncate(opts.limit);
        totals
    }

    fn verbs_table(&self, opts: &Opts) -> Table {
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .verbs
            .iter()
            .filter(|(_, c)| c.polls + c.reruns_after_edit > 0)
            .map(|((agent, verb), c)| {
                let mut row = vec![json!(agent.name()), json!(verb)];
                row.extend(c.cells());
                (c.polls, row)
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let cols = [&["agent", "verb"][..], &Counts::COLUMNS[..]].concat();
        let mut table = Table::new("By verb class, most polls first", cols);
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        table
    }

    fn count_repeat(&mut self, s: &Session, key: &str, verb: &str, previous: Option<u64>) -> bool {
        self.results
            .entry((s.agent, verb.to_owned()))
            .or_default()
            .commands += 1;
        let Some(prev_edits) = previous else {
            return false;
        };
        let total = self.totals.entry((s.agent, s.scope())).or_default();
        let row = self.verbs.entry((s.agent, verb.to_owned())).or_default();
        if prev_edits == self.edits {
            total.polls += 1;
            row.polls += 1;
            if self.latest.get(key) == Some(&Latest::Failed) {
                total.polls_after_failure += 1;
                row.polls_after_failure += 1;
            }
            return true;
        }
        total.reruns_after_edit += 1;
        row.reruns_after_edit += 1;
        false
    }

    /// The table of command results by agent and verb class, largest first.
    fn results_table(&self, opts: &Opts) -> Table {
        let mut results: Vec<(u64, Vec<Value>)> = self
            .results
            .iter()
            .map(|((agent, verb), r)| {
                let row = vec![
                    json!(agent.name()),
                    json!(verb),
                    json!(r.commands),
                    json!(r.results),
                    json!(r.chars),
                    json!(est_tokens(r.chars)),
                    json!(r.priced.usd()),
                    json!(r.priced.unpriced_tokens(r.chars)),
                ];
                (r.chars, row)
            })
            .collect();
        results.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut by_result = Table::new(
            "Command results by verb class, largest first",
            vec![
                "agent",
                "verb",
                "commands",
                "results",
                "result_chars",
                "result_tokens_est",
                "result_usd_est",
                "unpriced_tokens_est",
            ],
        );
        by_result.rows = results.into_iter().map(|(_, r)| r).collect();
        by_result.truncate(opts.limit);
        by_result
    }

    pub(crate) fn new(programs: Rc<Programs>) -> Self {
        Self {
            programs,
            ..Self::default()
        }
    }

    /// Counts one run of a command and gives the run whose result to await, if
    /// `awaits`. The run becomes the command's latest, so until its result is read
    /// the next run does not follow a failure; a run whose result is not awaited
    /// leaves that unknown. A blank command counts as a command, as
    /// `codex_commands_not_paired` counts it too, but is never a poll or rerun.
    fn observe(
        &mut self,
        s: &Session,
        cmd: &str,
        in_window: bool,
        awaits: bool,
    ) -> Option<Awaited> {
        let key = cmd.trim();
        if in_window {
            self.totals
                .entry((s.agent, s.scope()))
                .or_default()
                .commands += 1;
        }
        if key.is_empty() {
            return None;
        }
        let previous = self.last_seen.insert(key.to_owned(), self.edits);
        let class = in_window.then(|| verb_class(key, &mut |w| self.programs.has(w)));
        let truncation_rerun = self
            .truncation
            .observe_command(s.agent, key, class.as_deref());
        let poll = class
            .as_deref()
            .is_some_and(|verb| self.count_repeat(s, key, verb, previous));
        if !awaits {
            self.latest.remove(key);
            return None;
        }
        self.runs += 1;
        self.latest
            .insert(key.to_owned(), Latest::Awaited(self.runs));
        Some(Awaited {
            command: key.to_owned(),
            class,
            poll,
            truncation_rerun,
            run: self.runs,
        })
    }

    /// Reads a Codex call's commands, or its output.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
            self.truncation.next_turn();
            let awaits = call.exec && !call.call_id.is_empty();
            let mut runs = Vec::new();
            for step in &call.steps {
                match step {
                    Step::Command(c) => runs.push(self.observe(s, c, r.in_window, awaits)),
                    Step::Edit => self.edits += 1,
                }
            }
            self.codex
                .call(r, &call, (!runs.is_empty()).then_some(runs));
        } else if !self.codex.record(r)
            && let Some(out) = codex_output(r)
        {
            for (run, res) in self.codex.pair(&out) {
                if let Some(run) = run {
                    let truncated = truncation::text_looks_truncated(&res.output);
                    self.result(s, run, res.exit_code != 0, truncated, || res.printed_chars);
                }
            }
        }
    }

    /// Reads the result of an awaited run: its size when the run was in the window, also
    /// as poll characters when it was a counted poll, and, when it is still its command's
    /// latest run, whether it failed.
    fn result(
        &mut self,
        s: &Session,
        run: Awaited,
        failed: bool,
        truncated: bool,
        size: impl FnOnce() -> u64,
    ) {
        let mut n = 0;
        if let Some(class) = run.class.as_ref() {
            n = size();
            let r = self.results.entry((s.agent, class.clone())).or_default();
            r.results += 1;
            r.chars += n;
            self.unread_results.push((s.agent, class.clone(), n));
            if run.poll {
                self.totals
                    .entry((s.agent, s.scope()))
                    .or_default()
                    .poll_chars += n;
                self.verbs
                    .entry((s.agent, class.clone()))
                    .or_default()
                    .poll_chars += n;
                self.unread.push((s.agent, s.scope(), class.clone(), n));
            }
        }
        self.truncation.observe_result(
            s.agent,
            &run.command,
            run.class.as_deref(),
            run.truncation_rerun,
            truncated,
            n,
        );
        if self.latest.get(&run.command) != Some(&Latest::Awaited(run.run)) {
            return;
        }
        if failed {
            self.latest.insert(run.command, Latest::Failed);
        } else {
            self.latest.remove(&run.command);
        }
    }
}

#[cfg(test)]
#[path = "polls_tests.rs"]
mod tests;
