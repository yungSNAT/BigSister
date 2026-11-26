//! Reading Codex tool calls, their outputs and turn boundaries from records.

use serde_json::Value;

use super::{chars, name_of};
use crate::scan::Record;
use crate::shell::{self, Step};

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

/// Stateless superset of `CodexCalls::wants` for parallel pre-parsing.
pub(super) fn codex_result_line(line: &str) -> bool {
    line.contains("\"custom_tool_call_output\"")
        || line.contains("\"function_call_output\"")
        || line.contains("\"CommandExecution\"")
}

/// True for lines that may hold a Codex turn or model-response boundary.
pub(super) fn codex_turn_line(line: &str) -> bool {
    line.contains("\"type\":\"turn_context\"")
        || line.contains("\"type\":\"token_usage_record\"")
        || line.contains("\"task_started\"")
        || line.contains("\"turn_started\"")
        || line.contains("\"agent_message\"")
        || (line.contains("\"type\":\"response_item\"") && line.contains("\"assistant\""))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MsgSeen {
    Event,
    Item,
    Both,
}

/// Tracks distinct Codex turn and model-response boundaries so multi-call turns
/// count once and intervening text-only turns advance the turn counter.
#[derive(Default)]
pub(super) struct CodexTurns {
    last_turn_id: Option<String>,
    last_msg_id: Option<String>,
    last_resp_id: Option<String>,
    msg_seen: Option<MsgSeen>,
    awaiting_msg: bool,
    awaiting_usage: bool,
}

impl CodexTurns {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns `true` when `r` marks a new Codex turn or model response.
    pub(super) fn note(&mut self, r: &Record<'_>) -> bool {
        let p = r.value.get("payload");
        match r.kind() {
            "turn_context" => self.note_turn_start(codex_str(p, "turn_id")),
            "event_msg" => match codex_str(p, "type") {
                Some("task_started" | "turn_started") => {
                    self.note_turn_start(codex_str(p, "turn_id"))
                }
                Some("agent_message") => self.note_msg(MsgSeen::Event, codex_str(p, "id")),
                _ => false,
            },
            "response_item"
                if codex_str(p, "type") == Some("message")
                    && codex_str(p, "role") == Some("assistant") =>
            {
                self.note_msg(MsgSeen::Item, codex_str(p, "id"))
            }
            "token_usage_record" => self.note_usage(codex_str(p, "response_id")),
            _ => false,
        }
    }

    fn note_turn_start(&mut self, id: Option<&str>) -> bool {
        if id.is_some() && self.last_turn_id.as_deref() == id {
            return false;
        }
        self.last_turn_id = id.map(str::to_owned);
        self.last_msg_id = None;
        self.last_resp_id = None;
        self.msg_seen = None;
        self.awaiting_msg = true;
        self.awaiting_usage = true;
        true
    }

    fn note_msg(&mut self, family: MsgSeen, id: Option<&str>) -> bool {
        let opposite = matches!(
            (self.msg_seen, family),
            (Some(MsgSeen::Event), MsgSeen::Item) | (Some(MsgSeen::Item), MsgSeen::Event)
        );
        if id.is_some() && self.last_msg_id.as_deref() == id {
            self.msg_seen = if opposite {
                Some(MsgSeen::Both)
            } else {
                self.msg_seen
            };
            return false;
        }
        if opposite && (id.is_none() || self.last_msg_id.is_none()) {
            self.last_msg_id = id.map(str::to_owned).or_else(|| self.last_msg_id.take());
            self.msg_seen = Some(MsgSeen::Both);
            return false;
        }
        self.last_msg_id = id.map(str::to_owned);
        self.msg_seen = Some(family);
        self.awaiting_usage = true;
        if self.awaiting_msg {
            self.awaiting_msg = false;
            return false;
        }
        true
    }

    fn note_usage(&mut self, id: Option<&str>) -> bool {
        if id.is_some() && self.last_resp_id.as_deref() == id {
            return false;
        }
        self.last_resp_id = id.map(str::to_owned);
        self.msg_seen = None;
        self.awaiting_msg = false;
        if self.awaiting_usage {
            self.awaiting_usage = false;
            return false;
        }
        true
    }
}

fn codex_str<'a>(p: Option<&'a Value>, key: &str) -> Option<&'a str> {
    p?.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// Stateless superset of `wants` for detectors reading Claude and Codex tool calls.
pub(super) fn tool_prefilter(s: &crate::scan::Session, line: &str) -> bool {
    match s.agent {
        crate::scan::Agent::Codex => codex_call_line(line) || codex_result_line(line),
        _ => line.contains("\"tool_use\"") || line.contains("\"tool_result\""),
    }
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
    /// Whether any raw text the cell printed carried a truncation or stash marker.
    pub visible_truncated: bool,
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
    let visible_truncated = texts.iter().any(|t| truncation::text_looks_truncated(t));
    Some(CodexOutput {
        call_id: p.get("call_id")?.as_str()?.to_owned(),
        chars: texts.iter().map(|t| chars(t)).sum(),
        results,
        visible_truncated,
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

/// A `cmd` argument as one command line; `[shell, flag, script]` gives the script.
pub(super) fn function_command(cmd: &Value) -> Option<String> {
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
