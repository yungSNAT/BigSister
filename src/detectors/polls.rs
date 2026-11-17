//! Commands repeated within a session, with or without a file edit in between, and
//! what each verb class's results added to context.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use serde_json::{Value, json};

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
    programs: Rc<Programs>,
}

impl Detector for Polls {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex, Agent::Pi, Agent::OpenCode]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode => {
                line.contains("\"tool_use\"") || self.claude.wants(line)
            }
            Agent::Codex => codex_call_line(line) || self.codex.wants(line),
        }
    }

    fn begin(&mut self, _s: &Session) {
        self.last_seen.clear();
        self.edits = 0;
        self.claude.clear();
        self.latest.clear();
        self.unread.clear();
        self.unread_results.clear();
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
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        match s.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode => {
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
                        self.result(s, run, failed, || {
                            result_chars(b.get("content").unwrap_or(&Value::Null))
                        });
                    }
                }
            }
            Agent::Codex => self.codex(s, r),
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let columns = |keys: &[&'static str]| [keys, &Counts::COLUMNS[..]].concat();
        let mut totals = Table::new(
            "By agent and scope",
            columns(&["agent", "scope", "commands"]),
        );
        for ((agent, scope), c) in &self.totals {
            let mut row = vec![json!(agent.name()), json!(scope), json!(c.commands)];
            row.extend(c.cells());
            totals.rows.push(row);
        }
        totals.truncate(opts.limit);
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
        let mut table = Table::new(
            "By verb class, most polls first",
            columns(&["agent", "verb"]),
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "polls",
            title: "commands repeated exactly within a session",
            summary: vec![
                (
                    "codex_outputs_not_paired".to_owned(),
                    json!(self.codex.unpaired),
                ),
                (
                    "codex_commands_not_paired".to_owned(),
                    json!(self.codex.unpaired_commands),
                ),
            ],
            tables: vec![totals, table, self.results_table(opts)],
            notes: notes(),
        }
    }
}

/// The section's notes.
fn notes() -> Vec<String> {
    vec![
        "A poll repeats a command with no file edit since its last run; a rerun follows an edit.".to_owned(),
        "Edits made through shell commands are not seen, so some polls may be reruns.".to_owned(),
        "Command results count every command in the window by verb class, polled or not: results are those read, and result chars their characters of text, for Codex only what the exec cell printed of them, which is what reached the model.".to_owned(),
        "Polls after failure repeat a command whose previous run's result was read before the repeat and marked as an error, which a hook's denial also is; poll chars are the characters of text in polls' results, for Codex only of what the exec cell printed of them. A Codex command's result is the CommandExecution record Codex wrote as it ended, tied to the command by its text; a command no record ties takes the result its exec cell printed in its place only when the cell printed one result per command and has one command, Codex wrote no such record whilst the cell ran, or it is the one command left untied and each tied command's result is printed whole in its own place; Codex marks a failure by a non-zero exit code. Codex outputs not paired counts the calls in the window whose results could not all be paired: a function_call, whose output is not read, or an exec cell that had no call id, whose call id another exec cell in the session had, whose output never came or with a command neither tied nor paired. Codex commands not paired counts those commands, whose results were not judged.".to_owned(),
        EST_NOTE.to_owned(),
        prices::NOTE.to_owned(),
    ]
}

impl Polls {
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

    pub(super) fn new(programs: Rc<Programs>) -> Self {
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
        let mut poll = false;
        let class = in_window.then(|| verb_class(key, &mut |w| self.programs.has(w)));
        if let Some(verb) = &class {
            self.results
                .entry((s.agent, verb.clone()))
                .or_default()
                .commands += 1;
            let total = self.totals.entry((s.agent, s.scope())).or_default();
            if let Some(prev_edits) = previous {
                let verb = self.verbs.entry((s.agent, verb.clone())).or_default();
                if prev_edits == self.edits {
                    total.polls += 1;
                    verb.polls += 1;
                    if self.latest.get(key) == Some(&Latest::Failed) {
                        total.polls_after_failure += 1;
                        verb.polls_after_failure += 1;
                    }
                    poll = true;
                } else {
                    total.reruns_after_edit += 1;
                    verb.reruns_after_edit += 1;
                }
            }
        }
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
            run: self.runs,
        })
    }

    /// Reads a Codex call's commands, or its output.
    fn codex(&mut self, s: &Session, r: &Record<'_>) {
        if let Some(call) = codex_call(r) {
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
                    self.result(s, run, res.exit_code != 0, || res.printed_chars);
                }
            }
        }
    }

    /// Reads the result of an awaited run: its size when the run was in the window, also
    /// as poll characters when it was a counted poll, and, when it is still its command's
    /// latest run, whether it failed.
    fn result(&mut self, s: &Session, run: Awaited, failed: bool, size: impl FnOnce() -> u64) {
        if let Some(class) = run.class {
            let n = size();
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
                self.unread.push((s.agent, s.scope(), class, n));
            }
        }
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
mod tests {
    use super::*;

    #[test]
    fn only_results_of_awaited_runs_are_parsed() {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Polls::default();
        let run = d.observe(&s, "git status", true, true).expect("a run");
        d.claude.wait("toolu_a", run);
        let result = |id: &str| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}"}}]}}}}"#
            )
        };
        assert!(d.wants(&s, &result("toolu_a")));
        assert!(!d.wants(&s, &result("toolu_b")));
    }

    const CLAUDE: Session = Session {
        agent: Agent::Claude,
        subagent: false,
    };

    /// A Claude `tool_use` block running `git status` with id `id`.
    fn call(id: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"git status"}}}}]}}}}"#
        )
    }

    /// The main-scope totals after one Claude session of `lines`, all in the window.
    fn claude_totals(lines: &[String]) -> Counts {
        let mut d = Polls::default();
        d.begin(&CLAUDE);
        for line in lines {
            if d.wants(&CLAUDE, line) {
                let value: Value = serde_json::from_str(line).expect("valid JSON");
                d.record(
                    &CLAUDE,
                    &Record {
                        value: &value,
                        in_window: true,
                    },
                );
            }
        }
        d.end(&CLAUDE);
        d.totals[&(Agent::Claude, "main")]
    }

    #[test]
    fn a_claude_run_whose_result_never_comes_hides_an_earlier_failure() {
        let failure = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","is_error":true}]}}"#;
        let c = claude_totals(&[
            call("toolu_a"),
            failure.to_owned(),
            call("toolu_b"),
            call("toolu_c"),
        ]);
        // b follows the failure; c follows b, whose result never came.
        assert_eq!((c.polls, c.polls_after_failure), (2, 1));
    }

    #[test]
    fn a_reused_claude_tool_use_id_pairs_with_neither_run() {
        let result = |is_error: bool| {
            format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_a","is_error":{is_error},"content":"clean"}}]}}}}"#
            )
        };
        // The repeat is a poll, but the one result may be the first run's, so its
        // characters are not the poll's.
        let c = claude_totals(&[call("toolu_a"), call("toolu_a"), result(false)]);
        assert_eq!((c.polls, c.poll_chars), (1, 0));
        // Nor does a failure read for the reused id mark the next run.
        let c = claude_totals(&[
            call("toolu_a"),
            call("toolu_a"),
            result(true),
            call("toolu_b"),
        ]);
        assert_eq!((c.polls, c.polls_after_failure), (2, 0));
    }

    #[test]
    fn command_results_count_every_run_in_the_window_by_verb_class() {
        let bash = |id: &str, cmd: &str| {
            crate::detectors::claude_tool_use(id, "Bash", &json!({"command": cmd}))
        };
        let result = crate::detectors::claude_tool_result;
        let lines = [
            (false, bash("toolu_c", "cargo build")),
            (false, result("toolu_c", "1234567")),
            (true, bash("toolu_a", "git status")),
            (true, result("toolu_a", "0123456789")),
            (true, bash("toolu_b", "git status")),
            (true, result("toolu_b", "abcde")),
            (true, bash("toolu_d", "ls")),
        ];
        let mut d = Polls::default();
        crate::detectors::replay(&mut d, &CLAUDE, &lines);
        let r = |verb: &str| {
            d.results
                .get(&(Agent::Claude, verb.to_owned()))
                .map(|r| (r.commands, r.results, r.chars))
        };
        // A run before the window counts nowhere; a run whose result never comes counts
        // as a command only.
        assert_eq!(r("git"), Some((2, 2, 15)));
        assert_eq!(r("ls"), Some((1, 0, 0)));
        assert_eq!(r("cargo"), None);
    }
}
