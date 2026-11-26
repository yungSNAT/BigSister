//! The detectors and the record helpers they share.

pub mod cache;
mod cache_bust;
mod compaction;
pub mod context;
pub mod hooks;
pub mod models;
pub mod permissions;
pub mod pipes;
pub mod polls;
pub mod rereads;
pub mod schemas;
pub mod startup;
pub mod tools;
mod truncation;

mod codex;
mod codex_calls;
#[cfg(test)]
mod codex_item_tests;
#[cfg(test)]
mod codex_tests;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;

use crate::cli::Command;
use crate::prices::Prices;
use crate::programs::Programs;
#[cfg(test)]
use crate::scan::Session;
use crate::scan::{Detector, Record};

pub use codex::{CodexCall, CodexOutput, CommandResult, codex_call, codex_call_line, codex_output};
use codex::{CodexTurns, codex_turn_line, tool_prefilter};
pub use codex_calls::{CodexCalls, Paired};

/// The detectors a subcommand runs, in report order; `prices` are those the scan
/// prices with, which `models` also ranks models by.
#[must_use]
pub fn build(cmd: Command, programs: &Rc<Programs>, prices: &Rc<Prices>) -> Vec<Box<dyn Detector>> {
    let all = cmd == Command::All;
    let mut v: Vec<Box<dyn Detector>> = Vec::new();
    if all || cmd == Command::Startup {
        v.push(Box::new(startup::Startup::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Context {
        v.push(Box::<context::Context>::default());
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
    if all || cmd == Command::Tools {
        v.push(Box::<tools::Tools>::default());
    }
    if all || cmd == Command::Cache {
        v.push(Box::<cache::Cache>::default());
    }
    if all || cmd == Command::Models {
        v.push(Box::new(models::Models::new(Rc::clone(prices))));
    }
    if all || cmd == Command::Hooks {
        v.push(Box::new(hooks::Hooks::new(Rc::clone(programs))));
    }
    if all || cmd == Command::Permissions {
        v.push(Box::<permissions::Permissions>::default());
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

#[cfg(test)]
mod tests {
    use super::codex::function_command;
    use super::codex_calls::item_result;
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
        assert_eq!(cmd(json!("cargo build")).as_deref(), Some("cargo build"));
        assert_eq!(cmd(json!({"cmd": "ls"})), None);
    }

    fn at(value: &Value) -> Record<'_> {
        Record {
            value,
            in_window: true,
        }
    }

    #[test]
    fn a_function_call_output_closes_its_call_and_is_not_read_as_a_cell_output() {
        let mut codex = CodexCalls::<()>::default();
        let call = CodexCall {
            call_id: "c1".to_owned(),
            exec: true,
            ..CodexCall::default()
        };
        let empty = json!({});
        codex.call(&at(&empty), &call, Some(Vec::new()));
        let out = json!({"type": "response_item", "payload": {
            "type": "function_call_output", "call_id": "c1", "output": "done"
        }});
        assert!(codex.wants(&out.to_string()));
        assert!(codex.record(&at(&out)));
        assert!(!codex.wants(&out.to_string()), "the call stays open");
        assert!(codex_output(&at(&out)).is_none());
        // A record with no payload is neither kind.
        assert!(!codex.record(&at(&json!({"type": "response_item"}))));
    }

    #[test]
    fn outputs_of_other_shapes_hold_no_text_or_results() {
        let r = |value: &Value| codex_output(&at(value));
        let output = |output: Value| {
            json!({"type": "response_item", "payload": {
                "type": "custom_tool_call_output", "call_id": "c1", "output": output
            }})
        };
        let got = r(&output(json!({"exit_code": 0, "output": "x"}))).expect("an output");
        assert_eq!((got.chars, got.results.len()), (0, 0));
        // A result whose output is not text is not a result.
        let text = json!({"exit_code": 0, "output": 5}).to_string();
        let got = r(&output(json!(text))).expect("an output");
        assert_eq!((got.chars > 0, got.results.len()), (true, 0));
        let event = json!({"type": "event_msg", "payload": {
            "type": "custom_tool_call_output", "call_id": "c1", "output": "x"
        }});
        assert!(r(&event).is_none());
        assert_eq!(text_of(&json!(5)), "");
    }

    #[test]
    fn an_item_s_output_is_text_or_missing() {
        let result = |item: Value| item_result(&item).map(|r| (r.exit_code, r.output));
        assert_eq!(
            result(json!({"exit_code": 2, "aggregated_output": "e"})),
            Some((2, "e".to_owned()))
        );
        assert_eq!(
            result(json!({"exit_code": 1, "aggregated_output": null})),
            Some((1, String::new()))
        );
        assert_eq!(result(json!({"exit_code": 1})), Some((1, String::new())));
        assert_eq!(
            result(json!({"exit_code": 1, "aggregated_output": 5})),
            None
        );
    }
}
