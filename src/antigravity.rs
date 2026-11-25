//! Antigravity's conversation logs, rewritten into the Claude Code records the detectors
//! read.
//!
//! Antigravity writes each conversation's steps to
//! `~/.gemini/antigravity/brain/<conversation id>/.system_generated/logs/transcript_full.jsonl`,
//! and the same steps, with long fields cut short, to `transcript.jsonl` beside it. A line
//! is one step: `step_index`, `type`, `status` and `created_at`. A `PLANNER_RESPONSE`
//! step is a model reply, with its `model`, its `input_tokens` (uncached),
//! `cache_read_tokens` and `output_tokens`, and its `tool_calls`, each a `name` and `args`
//! with no id. The `GENERIC` steps that follow it hold each call's output in `content`,
//! in the calls' order, with `status` `ERROR` for a failed call.
//!
//! A reply becomes a Claude `assistant` record holding a `tool_use` block for each call,
//! under an id made from its line and place, and each `GENERIC` step a `user` record
//! holding one `tool_result` block. A reply's calls pair with the steps after it only when
//! the steps number the same as the calls; otherwise its calls get no result, and each of
//! those steps counts as a skipped line, as which step answers which call cannot be told.
//! Other steps, `CHECKPOINT` among them, hold only their time. A line that is not a JSON
//! object counts as skipped.
//!
//! The rules were taken from the files Antigravity's desktop app wrote up to 26 November
//! 2026; the format is undocumented, and later versions are unverified. Antigravity's
//! command-line tool, its SQLite conversation files and its models' prices are not read.
//! Every session counts as a main session.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

/// An Antigravity file read twice: first how many `GENERIC` steps follow each reply,
/// then the lines, rewritten.
#[derive(Default)]
pub struct Antigravity {
    /// By the line of each reply with calls: its calls, and the `GENERIC` steps after it.
    runs: HashMap<u64, (usize, usize)>,
    /// The reply whose following steps are being counted in the first pass.
    open: Option<u64>,
    indexed: u64,
    /// Lines read in the second pass.
    read: u64,
    /// The ids awaiting the `GENERIC` steps after the latest reply, in order; `None`
    /// when those steps pair with nothing.
    pending: Option<Vec<String>>,
}

impl Antigravity {
    /// Notes the file's next line in the first pass.
    pub fn index(&mut self, line: &str) {
        let n = self.indexed;
        self.indexed += 1;
        // A blank line is skipped in both passes, so it leaves a reply's run open.
        if line.trim().is_empty() {
            return;
        }
        let Some(v) = object(line) else {
            self.open = None;
            return;
        };
        match kind(&v) {
            "GENERIC" => {
                if let Some(run) = self.open.and_then(|r| self.runs.get_mut(&r)) {
                    run.1 += 1;
                }
            }
            "PLANNER_RESPONSE" => {
                let calls = v
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                self.runs.insert(n, (calls, 0));
                self.open = Some(n);
            }
            _ => self.open = None,
        }
    }

    /// Appends the Claude Code records for the file's next line to `out`. Returns how many
    /// of its parts are left out: 1 for a line that is not a JSON object, or for a
    /// `GENERIC` step that pairs with no call.
    pub fn read(&mut self, line: &str, out: &mut Vec<String>) -> u64 {
        let n = self.read;
        self.read += 1;
        if n >= self.indexed || line.trim().is_empty() {
            return 0;
        }
        let Some(v) = object(line) else {
            self.pending = None;
            return 1;
        };
        let ts = v.get("created_at").cloned().unwrap_or(Value::Null);
        match kind(&v) {
            "PLANNER_RESPONSE" => {
                out.push(self.reply(n, &v, &ts));
                0
            }
            "GENERIC" => {
                let Some(id) = self.pending.as_mut().and_then(Vec::pop) else {
                    out.push(json!({"type": "antigravity", "timestamp": ts}).to_string());
                    return 1;
                };
                out.push(result(n, &id, &v, &ts));
                0
            }
            _ => {
                self.pending = None;
                out.push(json!({"type": "antigravity", "timestamp": ts}).to_string());
                0
            }
        }
    }

    /// Reply `v` on line `n` as a Claude `assistant` record, awaiting the steps after it
    /// for its calls when they number the same.
    fn reply(&mut self, n: u64, v: &Value, ts: &Value) -> String {
        let calls = v.get("tool_calls").and_then(Value::as_array);
        let uses: Vec<Value> = calls
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, c)| tool_use(&format!("ag-{n}-{i}"), c))
            .collect();
        let paired = self.runs.get(&n).is_some_and(|(c, g)| c == g);
        self.pending = paired.then(|| {
            uses.iter()
                .rev()
                .filter_map(|u| u["id"].as_str().map(str::to_owned))
                .collect()
        });
        let mut usage = Map::new();
        for (from, to) in [
            ("input_tokens", "input_tokens"),
            ("cache_read_tokens", "cache_read_input_tokens"),
            ("output_tokens", "output_tokens"),
        ] {
            if let Some(x) = v.get(from).and_then(Value::as_u64) {
                usage.insert(to.to_owned(), x.into());
            }
        }
        assistant(n, v.get("model"), uses, &Value::Object(usage), ts)
    }
}

/// `GENERIC` step `v` on line `n` as a Claude `user` record holding the result of call
/// `id`: its `content`, or its `error` when it has no content, an error when its status is
/// `ERROR`.
fn result(n: u64, id: &str, v: &Value, ts: &Value) -> String {
    let content = ["content", "error"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
        .unwrap_or("");
    let error = v.get("status").and_then(Value::as_str) == Some("ERROR");
    json!({"type": "user", "uuid": format!("ag-{n}"),
           "message": {"role": "user", "content": [
               {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": error}]},
           "timestamp": ts})
    .to_string()
}

fn object(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}'))
        .ok()
        .filter(Value::is_object)
}

fn kind(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or("")
}

/// A Claude `assistant` record. Written as text: `model` must come first in the message,
/// where the scan looks for a reply's model, and a JSON object would sort its keys.
fn assistant(
    n: u64,
    model: Option<&Value>,
    content: Vec<Value>,
    usage: &Value,
    ts: &Value,
) -> String {
    let model = model.and_then(Value::as_str).unwrap_or("");
    format!(
        "{{\"type\":\"assistant\",\"uuid\":\"ag-{n}\",\"message\":{{\"model\":{},\"role\":\"assistant\",\"content\":{},\"usage\":{usage}}},\"timestamp\":{ts}}}",
        Value::from(model),
        Value::from(content),
    )
}

/// Call `c` as a Claude `tool_use` block under `id`. Antigravity's file and command tools
/// are named as Claude Code's: `run_command` runs `CommandLine` as `Bash` runs `command`,
/// `read_file` reads `Path` as `Read` reads `file_path`, and `edit_file` and `write_file`
/// change the file at `Path`.
fn tool_use(id: &str, c: &Value) -> Value {
    let name = c.get("name").and_then(Value::as_str).unwrap_or("");
    let args = c.get("args").cloned().unwrap_or(Value::Null);
    let renamed = |from: &str, to: &str| {
        let mut fields = args.as_object().cloned().unwrap_or_default();
        if let Some(v) = fields.remove(from) {
            fields.insert(to.to_owned(), v);
        }
        Value::Object(fields)
    };
    let (name, input) = match name {
        "run_command" => ("Bash", renamed("CommandLine", "command")),
        "read_file" => ("Read", renamed("Path", "file_path")),
        "edit_file" => ("Edit", renamed("Path", "file_path")),
        "write_file" => ("Write", renamed("Path", "file_path")),
        other => (other, args.clone()),
    };
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(text: &str) -> (Vec<Value>, u64) {
        let mut a = Antigravity::default();
        text.lines().for_each(|l| a.index(l));
        let mut out = Vec::new();
        let bad = text.lines().map(|l| a.read(l, &mut out)).sum();
        let parsed = out
            .iter()
            .map(|l| serde_json::from_str(l).expect("JSON"))
            .collect();
        (parsed, bad)
    }

    fn step(kind: &str, extra: &Value) -> String {
        let mut v = json!({"step_index": 0, "type": kind, "status": "DONE",
                           "created_at": "2026-11-09T06:00:00Z"});
        for (k, x) in extra.as_object().expect("object") {
            v[k] = x.clone();
        }
        v.to_string()
    }

    #[test]
    fn calls_pair_in_order_with_the_steps_after_them() {
        let text = [
            step(
                "PLANNER_RESPONSE",
                &json!({"model": "m", "input_tokens": 5,
                "cache_read_tokens": 90, "output_tokens": 2, "tool_calls": [
                {"name": "run_command", "args": {"CommandLine": "ls", "Cwd": "/w"}},
                {"name": "read_file", "args": {"Path": "/w/a.rs", "StartLine": 1}}]}),
            ),
            String::new(),
            step("GENERIC", &json!({"content": "one"})),
            step(
                "GENERIC",
                &json!({"content": "", "status": "ERROR", "error": "two"}),
            ),
        ]
        .join("\n");
        let (out, bad) = read_all(&text);
        assert_eq!(bad, 0);
        assert_eq!(
            out[0]["message"]["usage"],
            json!({"input_tokens": 5, "cache_read_input_tokens": 90, "output_tokens": 2})
        );
        let uses = &out[0]["message"]["content"];
        assert_eq!(uses[0]["name"], json!("Bash"));
        assert_eq!(uses[0]["input"], json!({"command": "ls", "Cwd": "/w"}));
        assert_eq!(uses[1]["name"], json!("Read"));
        assert_eq!(uses[1]["input"]["file_path"], json!("/w/a.rs"));
        let first = &out[1]["message"]["content"][0];
        assert_eq!(first["tool_use_id"], uses[0]["id"]);
        assert_eq!(
            (first["content"].clone(), first["is_error"].clone()),
            (json!("one"), json!(false))
        );
        let second = &out[2]["message"]["content"][0];
        assert_eq!(second["tool_use_id"], uses[1]["id"]);
        assert_eq!(second["is_error"], json!(true));
        assert_eq!(second["content"], json!("two"));
        assert_eq!(out[2]["timestamp"], json!("2026-11-09T06:00:00Z"));
    }

    #[test]
    fn steps_that_do_not_match_their_calls_in_number_pair_with_none() {
        let text = [
            step(
                "PLANNER_RESPONSE",
                &json!({"tool_calls": [
                {"name": "run_command", "args": {"CommandLine": "a"}},
                {"name": "run_command", "args": {"CommandLine": "b"}}]}),
            ),
            step("GENERIC", &json!({"content": "x"})),
            step("SYSTEM_MESSAGE", &json!({})),
            step("GENERIC", &json!({"content": "orphan"})),
            "not JSON".to_owned(),
        ]
        .join("\n");
        let (out, bad) = read_all(&text);
        assert_eq!(bad, 3);
        assert_eq!(
            out[0]["message"]["content"].as_array().map(Vec::len),
            Some(2)
        );
        assert!(out.iter().all(|r| r["type"] != json!("user")), "{out:?}");
    }

    #[test]
    fn file_tools_are_named_as_claude_code_s() {
        for (tool, name) in [("edit_file", "Edit"), ("write_file", "Write")] {
            let c = json!({"name": tool, "args": {"Path": "/w/a.rs"}});
            let u = tool_use("id", &c);
            assert_eq!(
                (u["name"].clone(), u["input"]["file_path"].clone()),
                (json!(name), json!("/w/a.rs"))
            );
        }
        // Any other tool keeps its name and arguments.
        let c = json!({"name": "grep_search", "args": {"Query": "x"}});
        let u = tool_use("id", &c);
        assert_eq!(
            (u["name"].clone(), u["input"].clone()),
            (json!("grep_search"), json!({"Query": "x"}))
        );
    }

    #[test]
    fn a_line_written_after_the_first_pass_is_not_read() {
        let mut a = Antigravity::default();
        a.index(&step("USER_INPUT", &json!({})));
        let mut out = Vec::new();
        assert_eq!(a.read(&step("USER_INPUT", &json!({})), &mut out), 0);
        assert_eq!(a.read(&step("GENERIC", &json!({})), &mut out), 0);
        assert_eq!(out.len(), 1);
    }
}
