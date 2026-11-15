//! The detectors and the record helpers they share.

pub mod hooks;
pub mod pipes;
pub mod polls;
pub mod rereads;
pub mod schemas;
pub mod startup;

#[cfg(test)]
mod codex_tests;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;

use crate::cli::Command;
use crate::programs::Programs;
#[cfg(test)]
use crate::scan::Session;
use crate::scan::{Detector, Record};
use crate::shell::{self, Step};

/// The detectors a subcommand runs, in report order.
#[must_use]
pub fn build(cmd: Command, programs: &Rc<Programs>) -> Vec<Box<dyn Detector>> {
    let all = cmd == Command::All;
    let mut v: Vec<Box<dyn Detector>> = Vec::new();
    if all || cmd == Command::Startup {
        v.push(Box::new(startup::Startup::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Schemas {
        v.push(Box::<schemas::Schemas>::default());
    }
    if all || cmd == Command::Pipes {
        v.push(Box::<pipes::Pipes>::default());
    }
    if all || cmd == Command::Polls {
        v.push(Box::new(polls::Polls::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Rereads {
        v.push(Box::<rereads::Rereads>::default());
    }
    if all || cmd == Command::Hooks {
        v.push(Box::new(hooks::Hooks::new(Rc::clone(programs))));
    }
    v
}

fn blocks<'a>(
    r: &'a Record<'_>,
    kind: &str,
    block: &'static str,
) -> impl Iterator<Item = &'a Value> {
    let content = if r.kind() == kind {
        r.value
            .pointer("/message/content")
            .and_then(Value::as_array)
    } else {
        None
    };
    content
        .into_iter()
        .flatten()
        .filter(move |b| b.get("type").and_then(Value::as_str) == Some(block))
}

/// The `tool_use` blocks of a Claude `assistant` record.
pub fn tool_uses<'a>(r: &'a Record<'_>) -> impl Iterator<Item = &'a Value> {
    blocks(r, "assistant", "tool_use")
}

/// The `tool_result` blocks of a Claude `user` record.
pub fn tool_results<'a>(r: &'a Record<'_>) -> impl Iterator<Item = &'a Value> {
    blocks(r, "user", "tool_result")
}

/// A block's `name`, or "".
#[must_use]
pub fn name_of(b: &Value) -> &str {
    b.get("name").and_then(Value::as_str).unwrap_or("")
}

/// A string field of a `tool_use` block's input.
#[must_use]
pub fn input_str<'a>(b: &'a Value, key: &str) -> Option<&'a str> {
    b.get("input")
        .and_then(|i| i.get(key))
        .and_then(Value::as_str)
}

/// The text of a message or tool result: a string, or the `text` of its blocks.
#[must_use]
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Characters in a string.
#[must_use]
pub fn chars(s: &str) -> u64 {
    s.chars().count() as u64
}

/// About how many tokens `chars` characters make, for the `*_tokens_est` columns.
///
/// shortcut: 4 characters a token, a rough average for English text and code; the
/// model's tokenizer would give the true count, but it is not published.
#[must_use]
pub fn est_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

/// The note of each section with `*_tokens_est` columns.
pub const EST_NOTE: &str = "Token estimates (tokens est) divide each row's characters by 4, rounded up, so rows need not add up to a total; the model's own count differs.";

/// The size of a tool result: the characters of text in its content, which is a
/// string or a list of blocks. A block without text, such as an image, adds nothing.
#[must_use]
pub fn result_chars(content: &Value) -> u64 {
    match content {
        Value::String(s) => chars(s),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .map(chars)
            .sum(),
        _ => 0,
    }
}

/// Characters in a value's compact JSON, or in the string itself.
#[must_use]
pub fn json_chars(v: &Value) -> u64 {
    match v {
        Value::String(s) => chars(s),
        other => chars(&other.to_string()),
    }
}

/// What one Codex tool call runs and edits, in order, and what it reads.
#[derive(Debug, Default)]
pub struct CodexCall {
    /// The id its output will carry, or "" when it has none.
    pub call_id: String,
    /// True for an `exec` cell, whose output `codex_output` reads.
    pub exec: bool,
    pub steps: Vec<Step>,
    pub reads: Vec<String>,
}

impl CodexCall {
    /// The shell commands among the steps.
    pub fn commands(&self) -> impl Iterator<Item = &str> {
        self.steps.iter().filter_map(|s| match s {
            Step::Command(c) => Some(c.as_str()),
            Step::Edit => None,
        })
    }
}

/// True for lines that may hold a Codex tool call.
#[must_use]
pub fn codex_call_line(line: &str) -> bool {
    line.contains("\"custom_tool_call\"") || line.contains("\"function_call\"")
}

/// Reads a Codex `exec` cell or a `function_call` with a `cmd` argument.
#[must_use]
pub fn codex_call(r: &Record<'_>) -> Option<CodexCall> {
    if r.kind() != "response_item" {
        return None;
    }
    let p = r.value.get("payload")?;
    let call_id = p
        .get("call_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    match p.get("type")?.as_str()? {
        "custom_tool_call" if name_of(p) == "exec" => {
            let input = p.get("input")?.as_str()?;
            Some(CodexCall {
                call_id,
                exec: true,
                steps: shell::codex_steps(input),
                reads: shell::codex_read_calls(input),
            })
        }
        "function_call" if name_of(p) == "apply_patch" => Some(CodexCall {
            call_id,
            steps: vec![Step::Edit],
            ..CodexCall::default()
        }),
        "function_call" => {
            let args: Value = serde_json::from_str(p.get("arguments")?.as_str()?).ok()?;
            let cmd = function_command(args.get("cmd").or_else(|| args.get("command"))?)?;
            Some(CodexCall {
                call_id,
                steps: vec![Step::Command(cmd)],
                ..CodexCall::default()
            })
        }
        _ => None,
    }
}

/// What a Codex `exec` cell printed.
#[derive(Debug, Default)]
pub struct CodexOutput {
    pub call_id: String,
    /// Characters of text in the output.
    pub chars: u64,
    /// The `exec_command` results it printed, in order.
    pub results: Vec<CommandResult>,
}

/// One `exec_command` result as an `exec` cell prints it.
#[derive(Debug)]
pub struct CommandResult {
    pub exit_code: i64,
    pub output: String,
}

/// Reads a Codex `exec` cell's output: a list of text items or one string. In the
/// texts that are JSON, the objects with an `exit_code` and an `output` are
/// `exec_command` results, whether a text is one, as `text(await
/// tools.exec_command(...))` prints it, or holds them in lists and objects, as a
/// printed `Promise.allSettled` does; a null `output` is empty. Results keep the
/// order printed. An object holding results under more than one key gives the
/// output no results, as the parsed object does not keep its keys' order, and so do
/// entries numbered by an `i` that does not rise in the order printed and a result
/// still running, after which a result may be another session's. Any other `output`
/// holds no text.
///
/// shortcut: results are found only where the cell prints a result object whole; a
/// cell that prints parts of one, such as its `output` alone, or none, gives no
/// results, and one that prints each result as it finishes can print them out of
/// order, which is why `CodexCalls` pairs a command by its item first.
#[must_use]
pub fn codex_output(r: &Record<'_>) -> Option<CodexOutput> {
    if r.kind() != "response_item" {
        return None;
    }
    let p = r.value.get("payload")?;
    if p.get("type")?.as_str()? != "custom_tool_call_output" {
        return None;
    }
    let texts: Vec<&str> = match p.get("output") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect(),
        _ => Vec::new(),
    };
    let mut results = Vec::new();
    let mut last_i = None;
    let ordered = texts.iter().all(|t| {
        serde_json::from_str::<Value>(t)
            .map_or(true, |v| collect_results(&v, &mut results, &mut last_i))
    });
    if !ordered {
        results.clear();
    }
    Some(CodexOutput {
        call_id: p.get("call_id")?.as_str()?.to_owned(),
        chars: texts.iter().map(|t| chars(t)).sum(),
        results,
    })
}

/// Adds the results in `v` to `out` in order, and gives false when their order is
/// unknown: an object holds results under more than one key, an entry numbered `i`
/// holds results but is not numbered above the one before it (`last_i`), or a result
/// is still running, after which a result may be another session's.
fn collect_results(v: &Value, out: &mut Vec<CommandResult>, last_i: &mut Option<u64>) -> bool {
    let before = out.len();
    let ordered = if let Some(r) = command_result(v) {
        out.push(r);
        true
    } else if v.get("session_id").is_some() && v.get("output").is_some() {
        false
    } else {
        match v {
            Value::Array(items) => items.iter().all(|x| collect_results(x, out, last_i)),
            Value::Object(map) => {
                let mut keys_with_results = 0;
                map.values().all(|x| {
                    let len = out.len();
                    let ordered = collect_results(x, out, last_i);
                    keys_with_results += usize::from(out.len() > len);
                    ordered
                }) && keys_with_results <= 1
            }
            _ => true,
        }
    };
    match v.get("i").and_then(Value::as_u64) {
        Some(i) if ordered && out.len() > before => {
            let follows = last_i.is_none_or(|last| i > last);
            *last_i = Some(i);
            follows
        }
        _ => ordered,
    }
}

fn command_result(v: &Value) -> Option<CommandResult> {
    let output = match v.get("output")? {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => return None,
    };
    Some(CommandResult {
        exit_code: v.get("exit_code")?.as_i64()?,
        output,
    })
}

/// A command's result as a detector reads it.
#[derive(Debug)]
pub struct Paired {
    pub exit_code: i64,
    /// What the command wrote, as Codex recorded it or the cell printed it.
    pub output: String,
    /// Characters of the output the cell printed for this command, which reached the
    /// model; 0 when it printed none.
    pub printed_chars: u64,
}

/// A `CommandExecution` item kept for the one open call that could have run it.
struct Item {
    key: String,
    result: CommandResult,
}

/// A Codex call whose output has not come.
struct Open<T> {
    id: String,
    /// `id` as JSON writes it, to find its output's line.
    needle: String,
    /// When the call was recorded, in milliseconds since the epoch.
    start: Option<i64>,
    /// Its place amongst the session's calls.
    seq: u64,
    /// Each command's text as `command_key` gives it.
    keys: Vec<String>,
    /// The items whose text no other call's command could have.
    items: Vec<Item>,
    /// Texts that an item could have from this call or another, or that an item
    /// without an exit code had.
    ambiguous: Vec<String>,
    /// Whether Codex recorded any `CommandExecution` item whilst the call was open.
    saw_items: bool,
    /// While a detector awaits the output: what it keeps for each command, and whether
    /// the call was in the window.
    runs: Option<(Vec<T>, bool)>,
}

/// What a call left when its output came.
struct Closed<T> {
    runs: Option<(Vec<T>, bool)>,
    /// Each command's result from its items, if tied.
    ties: Vec<Option<CommandResult>>,
    saw_items: bool,
}

/// A command not tied to an item when its call's output came, which may still be
/// running and so end whilst a call made before that output is open.
struct Lingering {
    key: String,
    start: Option<i64>,
    /// The `seq` of the last call made before the output came.
    closed_at: u64,
}

/// Codex calls whose output a detector awaits, each with what the detector keeps for
/// each of its commands, and how many in the window, and how many of their commands,
/// could not be paired with their results: calls without an id, calls whose id
/// another call in the session also carries, `function_call` calls, whose output is
/// not read, calls whose output never came, and commands neither tied to an item nor
/// paired by position.
///
/// A command ties to a `CommandExecution` item Codex records as a command ends, which
/// carries no call id, when the item's command reads as the command's text, it ended
/// before the call's output, the call's record has a time no later than the item's
/// start, and no other command could have run it: no command of another open call,
/// including a call without an id or time, that could have started first has that
/// text, nor does an untied command of a call whose output came whilst one of those
/// was open; and the call has no other command with that text and no other such item.
/// A command not tied pairs with the result the cell printed at its position when the
/// cell printed one result per command and has one command, or Codex recorded no item
/// whilst the call was open, as older Codex versions record none, or it is the one
/// command left untied, every tied command's result is printed whole in its own place
/// and the result in its place is no tied command's item cut to its end (`in_order`).
pub struct CodexCalls<T> {
    /// The ids of the session's calls so far.
    seen: HashSet<String>,
    open: Vec<Open<T>>,
    lingering: Vec<Lingering>,
    /// Calls made so far in the session.
    seq: u64,
    pub unpaired: u64,
    /// The commands of the `unpaired` calls.
    pub unpaired_commands: u64,
}

impl<T> Default for CodexCalls<T> {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            open: Vec::new(),
            lingering: Vec::new(),
            seq: 0,
            unpaired: 0,
            unpaired_commands: 0,
        }
    }
}

impl<T> CodexCalls<T> {
    /// Notes a call, which every detector reading outputs must do for every call so
    /// that a reused id and every running command are seen. `runs` holds what the
    /// detector keeps for each command, by position, or is `None` when it awaits none
    /// of the output. An id carried by more than one call pairs with neither from then
    /// on, as their outputs cannot be told apart.
    pub fn call(&mut self, r: &Record<'_>, call: &CodexCall, runs: Option<Vec<T>>) {
        let id = call.call_id.as_str();
        let fresh = !id.is_empty() && self.seen.insert(id.to_owned());
        if !fresh {
            let earlier: Vec<(Vec<T>, bool)> = self
                .open
                .iter_mut()
                .filter(|o| o.id == id)
                .filter_map(|o| o.runs.take())
                .collect();
            for (earlier, w) in earlier {
                self.not_paired(earlier.len(), w);
            }
        }
        let awaited = call.exec && fresh;
        let runs = match runs {
            Some(runs) if !awaited => {
                self.not_paired(runs.len(), r.in_window);
                None
            }
            runs => runs.map(|runs| (runs, r.in_window)),
        };
        if runs.is_none() && call.commands().next().is_none() {
            return;
        }
        // A call without an id stays open for the rest of the session, as its output
        // cannot be found, so that no other call takes an item it could have run.
        self.seq += 1;
        self.open.push(Open {
            id: id.to_owned(),
            needle: json_escaped(id),
            start: record_ms(r),
            seq: self.seq,
            keys: call.commands().map(command_key).collect(),
            items: Vec::new(),
            ambiguous: Vec::new(),
            saw_items: false,
            runs,
        });
    }

    fn not_paired(&mut self, commands: usize, in_window: bool) {
        if in_window {
            self.unpaired += 1;
            self.unpaired_commands += commands as u64;
        }
    }

    /// True for lines that may hold an open call's output or a command's item.
    #[must_use]
    pub fn wants(&self, line: &str) -> bool {
        if line.contains("\"custom_tool_call_output\"") || line.contains("\"function_call_output\"")
        {
            self.open
                .iter()
                .any(|o| !o.id.is_empty() && line.contains(o.needle.as_str()))
        } else {
            // An item matters only to the commands of an awaited call.
            line.contains("\"CommandExecution\"")
                && self
                    .open
                    .iter()
                    .any(|o| o.runs.as_ref().is_some_and(|(runs, _)| !runs.is_empty()))
        }
    }

    /// Reads a `CommandExecution` item or a `function_call`'s output, and says whether
    /// the record was one.
    pub fn record(&mut self, r: &Record<'_>) -> bool {
        let Some(p) = r.value.get("payload") else {
            return false;
        };
        match (r.kind(), p.get("type").and_then(Value::as_str)) {
            ("response_item", Some("function_call_output")) => {
                if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    self.close(id);
                }
                true
            }
            ("event_msg", Some("item_completed"))
                if p.pointer("/item/type").and_then(Value::as_str) == Some("CommandExecution") =>
            {
                self.item(p);
                true
            }
            _ => false,
        }
    }

    fn item(&mut self, p: &Value) {
        for o in &mut self.open {
            o.saw_items = true;
        }
        let item = &p["item"];
        let Some(key) = item.get("command").and_then(function_command) else {
            return;
        };
        let key = command_key(&key);
        let started = p.get("started_at_ms").and_then(Value::as_i64);
        // A command could have run the item unless it is known to have started later;
        // only a call whose start is known can take it.
        let could = |start: Option<i64>| start.zip(started).is_none_or(|(s, t)| s <= t);
        let lingers = self
            .lingering
            .iter()
            .any(|l| l.key == key && could(l.start));
        let calls: Vec<usize> = (0..self.open.len())
            .filter(|&i| could(self.open[i].start) && self.open[i].keys.contains(&key))
            .collect();
        match (calls.as_slice(), lingers, started.zip(item_result(item))) {
            ([i], false, Some((_, result))) if self.open[*i].start.is_some() => {
                self.open[*i].items.push(Item { key, result });
            }
            _ => {
                for i in calls {
                    self.open[i].ambiguous.push(key.clone());
                }
            }
        }
    }

    /// Ends the calls with id `id`, and gives the last. Their untied commands linger
    /// whilst a call made before them is open.
    fn close(&mut self, id: &str) -> Option<Closed<T>> {
        if id.is_empty() {
            return None;
        }
        let mut last = None;
        while let Some(i) = self.open.iter().position(|o| o.id == id) {
            let Open {
                start,
                keys,
                items,
                ambiguous,
                saw_items,
                runs,
                ..
            } = self.open.remove(i);
            let ties = ties(&keys, items, &ambiguous);
            if !self.open.is_empty() {
                for (key, tie) in keys.into_iter().zip(&ties) {
                    if tie.is_none() {
                        self.lingering.push(Lingering {
                            key,
                            start,
                            closed_at: self.seq,
                        });
                    }
                }
            }
            last = Some(Closed {
                runs,
                ties,
                saw_items,
            });
        }
        let open = &self.open;
        self.lingering
            .retain(|l| open.iter().any(|o| o.seq <= l.closed_at));
        last
    }

    /// Whether the output's call was awaited; it is awaited no longer.
    pub fn take(&mut self, out: &CodexOutput) -> bool {
        self.close(&out.call_id).is_some_and(|c| c.runs.is_some())
    }

    /// For an awaited call's output: what the detector kept for each command that
    /// could be paired, with that command's result: from its item if tied, otherwise
    /// the result printed at its position where that is allowed (`CodexCalls`).
    pub fn pair(&mut self, out: &CodexOutput) -> Vec<(T, Paired)> {
        let Some(Closed {
            runs: Some((runs, in_window)),
            ties,
            saw_items,
        }) = self.close(&out.call_id)
        else {
            return Vec::new();
        };
        let n = runs.len();
        // With every other command tied to the result printed whole in its own place,
        // the one result left is the untied command's.
        let by_position =
            out.results.len() == n && (n == 1 || !saw_items || in_order(&ties, &out.results));
        let shown = printed_for(&out.results, &ties, by_position);
        let mut missing = 0;
        let paired = runs
            .into_iter()
            .zip(ties)
            .zip(shown)
            .enumerate()
            .filter_map(|(k, ((run, tie), shown))| {
                let p = match tie {
                    Some(item) => Some(Paired {
                        printed_chars: shown.map_or(0, |x| chars(&x.output)),
                        exit_code: item.exit_code,
                        output: item.output,
                    }),
                    None if by_position => out.results.get(k).map(|x| Paired {
                        exit_code: x.exit_code,
                        output: x.output.clone(),
                        printed_chars: chars(&x.output),
                    }),
                    None => None,
                };
                missing += usize::from(p.is_none());
                p.map(|p| (run, p))
            })
            .collect();
        if missing > 0 {
            self.not_paired(missing, in_window);
        }
        paired
    }

    /// Ends a session: the calls still awaiting output never get it.
    pub fn end(&mut self) {
        for o in std::mem::take(&mut self.open) {
            if let Some((runs, in_window)) = o.runs {
                self.not_paired(runs.len(), in_window);
            }
        }
        self.lingering.clear();
        self.seen.clear();
        self.seq = 0;
    }
}

/// Each command's result from a call's items: the one item with its text, when the
/// text is not `ambiguous` and no other command of the call has it, as code need not
/// run commands in the order it writes them.
fn ties(keys: &[String], items: Vec<Item>, ambiguous: &[String]) -> Vec<Option<CommandResult>> {
    let once = |key: &String| {
        !ambiguous.contains(key)
            && keys.iter().filter(|k| *k == key).count() == 1
            && items.iter().filter(|i| i.key == *key).count() == 1
    };
    let fits: Vec<bool> = keys.iter().map(once).collect();
    let mut items: Vec<Option<Item>> = items.into_iter().map(Some).collect();
    keys.iter()
        .zip(fits)
        .map(|(key, fits)| {
            let item = items
                .iter_mut()
                .find(|i| fits && i.as_ref().is_some_and(|i| i.key == *key))?;
            item.take().map(|i| i.result)
        })
        .collect()
}

/// Whether the one command `ties` leaves untied can take the result printed in its
/// place: every tied command's result is printed whole in its own place, and the one
/// left is no tied command's item cut to its end. Another pairing that gave the untied
/// command another result would be a chain of tied commands, each taking a result that
/// shows its item, the last taking the untied command's; a link that is not whole makes
/// that result a tied item cut to its end, and with every link whole the untied
/// command's result is the same either way.
fn in_order(ties: &[Option<CommandResult>], printed: &[CommandResult]) -> bool {
    let mut untied = (0..ties.len()).filter(|&k| ties[k].is_none());
    let (Some(u), None) = (untied.next(), untied.next()) else {
        return false;
    };
    let tied = || ties.iter().flatten();
    ties.iter().enumerate().all(|(k, tie)| {
        tie.as_ref().is_none_or(|item| {
            printed
                .get(k)
                .is_some_and(|x| x.exit_code == item.exit_code && x.output == item.output)
        })
    }) && printed
        .get(u)
        .is_some_and(|x| !tied().any(|item| shows(x, item) && x.output != item.output))
}

/// Whether printed result `x` shows the item's: the same exit code, and the item's
/// output or, as `exec_command` keeps the end of a long output, its end.
fn shows(x: &CommandResult, item: &CommandResult) -> bool {
    x.exit_code == item.exit_code
        && (x.output == item.output || (!x.output.is_empty() && item.output.ends_with(&x.output)))
}

/// For each tied command, the result the cell printed of it: one that shows its item
/// and no other tied command's, preferring its whole output. A printed result that
/// could be more than one command's counts for none. Where a command is untied, which
/// could own any printed result, a tied command has only the result in its own place,
/// and only when the cell's results pair `by_position`.
fn printed_for<'a>(
    printed: &'a [CommandResult],
    ties: &[Option<CommandResult>],
    by_position: bool,
) -> Vec<Option<&'a CommandResult>> {
    if ties.iter().any(Option::is_none) {
        return (0..ties.len())
            .map(|k| printed.get(k).filter(|_| by_position && ties[k].is_some()))
            .collect();
    }
    let owners: Vec<Option<usize>> = printed
        .iter()
        .map(|x| {
            let mut can =
                (0..ties.len()).filter(|&k| ties[k].as_ref().is_some_and(|item| shows(x, item)));
            match (can.next(), can.next()) {
                (Some(k), None) => Some(k),
                _ => None,
            }
        })
        .collect();
    ties.iter()
        .enumerate()
        .map(|(k, tie)| {
            let item = tie.as_ref()?;
            let mine = || {
                printed
                    .iter()
                    .zip(&owners)
                    .filter(move |(_, o)| **o == Some(k))
                    .map(|(x, _)| x)
            };
            mine()
                .find(|x| x.output == item.output)
                .or_else(|| mine().next())
        })
        .collect()
}

/// An item's exit code and output; a missing output is empty.
fn item_result(item: &Value) -> Option<CommandResult> {
    Some(CommandResult {
        exit_code: item.get("exit_code")?.as_i64()?,
        output: match item.get("aggregated_output") {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => String::new(),
            Some(_) => return None,
        },
    })
}

/// When a record was written, in milliseconds since the epoch.
fn record_ms(r: &Record<'_>) -> Option<i64> {
    let t = r.value.get("timestamp")?.as_str()?;
    Some(crate::time::parse_ts(t)?.timestamp_millis())
}

/// A command's text as it is matched with Codex's record of what ran: trimmed, and
/// without `rtk ` where a simple command starts.
///
/// shortcut: the RTK hook rewrites the commands Codex runs by putting `rtk ` before
/// each program it filters (38,599 of 104,470 items on one PC, 15 November 2026), so
/// both texts drop the word, and 2 commands of a call that then read alike tie to
/// neither.
/// Commands another hook rewrites stay untied; telling rewrites apart would need the
/// hooks' own records.
fn command_key(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd.trim();
    while let Some(c) = rest.chars().next() {
        let starts = out
            .chars()
            .next_back()
            .is_none_or(|p| p.is_whitespace() || ";&|(".contains(p));
        if starts && let Some(after) = rest.strip_prefix("rtk ") {
            rest = after;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Claude `tool_use` blocks whose result a detector awaits, by id, and the ids of the
/// session's blocks so far.
pub struct ClaudeCalls<T> {
    seen: HashSet<String>,
    /// By id: the id as JSON writes it, and what the detector keeps.
    awaited: HashMap<String, (String, T)>,
}

impl<T> Default for ClaudeCalls<T> {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            awaited: HashMap::new(),
        }
    }
}

impl<T> ClaudeCalls<T> {
    /// Notes a `tool_use` block, which a detector reading results must do for every
    /// block so that a reused id is seen, and says whether its result can be
    /// awaited: it has a non-empty id that no earlier block in the session carried. An id
    /// carried by more than one block pairs with neither from then on, as their
    /// results cannot be told apart.
    pub fn note(&mut self, id: Option<&str>) -> bool {
        let id = id.filter(|id| !id.is_empty());
        let fresh = id.is_some_and(|id| self.seen.insert(id.to_owned()));
        if !fresh && let Some(id) = id {
            self.awaited.remove(id);
        }
        fresh
    }

    /// Awaits the result of a block that `note` said can be awaited.
    pub fn wait(&mut self, id: &str, run: T) {
        self.awaited.insert(id.to_owned(), (json_escaped(id), run));
    }

    /// True for lines that may hold an awaited block's result.
    #[must_use]
    pub fn wants(&self, line: &str) -> bool {
        line.contains("\"tool_result\"")
            && self
                .awaited
                .values()
                .any(|(id, _)| line.contains(id.as_str()))
    }

    /// What the detector kept for the block whose result has id `id`, if awaited; it
    /// is awaited no longer.
    pub fn take(&mut self, id: &str) -> Option<T> {
        self.awaited.remove(id).map(|(_, run)| run)
    }

    /// Starts a session.
    pub fn clear(&mut self) {
        self.seen.clear();
        self.awaited.clear();
    }
}

/// `id` as it appears inside a JSON string, so that a raw line can be searched for
/// an id holding a quote, a backslash or a control character.
fn json_escaped(id: &str) -> String {
    let quoted = Value::from(id).to_string();
    quoted[1..quoted.len() - 1].to_owned()
}

/// Feeds one session to `d` as the scan does, parsing only the lines it wants; each
/// line comes with whether it is in the window.
#[cfg(test)]
fn replay(d: &mut dyn Detector, s: &Session, lines: &[(bool, String)]) {
    d.begin(s);
    for (in_window, line) in lines {
        if d.wants(s, line) {
            let value: Value = serde_json::from_str(line).expect("test record");
            d.record(
                s,
                &Record {
                    value: &value,
                    in_window: *in_window,
                },
            );
        }
    }
    d.end(s);
}

/// A Claude `tool_use` block of tool `name` with `input`, under id `id`.
#[cfg(test)]
fn claude_tool_use(id: &str, name: &str, input: &Value) -> String {
    serde_json::json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": id, "name": name, "input": input}
    ]}})
    .to_string()
}

/// A Claude `tool_result` block for id `id` holding `text`.
#[cfg(test)]
fn claude_tool_result(id: &str, text: &str) -> String {
    serde_json::json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": id, "content": text}
    ]}})
    .to_string()
}

/// A `cmd` argument as one command line; `[shell, flag, script]` gives the script.
fn function_command(cmd: &Value) -> Option<String> {
    match cmd {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            let shells = ["bash", "sh", "zsh", "pwsh", "powershell", "cmd"];
            // `bash -lc <script>`: the script is the element after the flag.
            let script = match parts.as_slice() {
                [first, rest @ ..] if shells.contains(&shell::program(first).as_str()) => rest
                    .iter()
                    .position(|w| {
                        matches!(w.to_lowercase().as_str(), "-c" | "-lc" | "-command" | "/c")
                    })
                    .and_then(|i| rest.get(i + 1)),
                _ => None,
            };
            Some(script.map_or_else(|| parts.join(" "), |s| (*s).to_owned()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_or_missing_claude_id_is_never_awaited() {
        let mut calls = ClaudeCalls::<()>::default();
        assert!(!calls.note(None));
        assert!(!calls.note(Some("")));
        assert!(calls.note(Some("toolu_a")));
        assert!(!calls.note(Some("toolu_a")));
    }

    #[test]
    fn an_id_that_json_escapes_is_still_found_in_its_line() {
        let id = "toolu_\"q\\b\u{1}";
        let mut claude = ClaudeCalls::default();
        assert!(claude.note(Some(id)));
        claude.wait(id, ());
        assert!(claude.wants(&claude_tool_result(id, "ok")));
        let mut codex = CodexCalls::<()>::default();
        let call = CodexCall {
            call_id: id.to_owned(),
            exec: true,
            steps: Vec::new(),
            reads: Vec::new(),
        };
        let value = json!({});
        let r = Record {
            value: &value,
            in_window: true,
        };
        codex.call(&r, &call, Some(Vec::new()));
        let out = json!({"type": "response_item", "payload": {
            "type": "custom_tool_call_output", "call_id": id, "output": "ok"
        }});
        assert!(codex.wants(&out.to_string()));
    }

    #[test]
    fn function_command_takes_the_script_after_the_shell_flag() {
        let cmd = |v: Value| function_command(&v);
        assert_eq!(
            cmd(json!(["bash", "-lc", "cargo test"])).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            cmd(json!([
                r"C:\x\pwsh.exe",
                "-NoProfile",
                "-Command",
                "git status"
            ]))
            .as_deref(),
            Some("git status")
        );
        assert_eq!(
            cmd(json!(["bash", "run.sh", "arg"])).as_deref(),
            Some("bash run.sh arg")
        );
        assert_eq!(cmd(json!(["git", "status"])).as_deref(), Some("git status"));
    }
}
