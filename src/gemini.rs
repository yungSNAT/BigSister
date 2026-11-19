//! Gemini CLI's and Qwen Code's session files, rewritten into the Claude Code records the
//! detectors read.
//!
//! Gemini CLI (<https://github.com/google-gemini/gemini-cli>) writes each session to
//! `~/.gemini/tmp/<project>/chats/session-<time>-<id>.jsonl`: a header, then records with
//! an `id` and a `type` of `user` or `gemini`, and `$set` records. It writes a reply again,
//! under the same id, as its tool calls are scheduled and finish, so the last record with
//! an id stands for it, in the place of the first. A reply's `toolCalls` give each call's
//! `name`, `args`, `status` and `result`, a list of `functionResponse` parts.
//!
//! Qwen Code (<https://github.com/QwenLM/qwen-code>), a fork of Gemini CLI, writes
//! `~/.qwen/projects/<folder>/chats/<session id>.jsonl`: records with a `uuid` and a `type`
//! of `user`, `assistant`, `tool_result` or `system`, whose `message` holds Gemini API
//! `parts`; a `tool_result` record's `toolCallResult` gives the call's `status`.
//!
//! A reply becomes a Claude `assistant` record holding a `tool_use` block for each tool
//! call, and each result a Claude `user` record holding one `tool_result` block, so that
//! the Claude code paths count both agents' tool calls by the same rules. Every other
//! record becomes a record holding only its time, which the detectors ignore. Gemini CLI
//! 0.69.0 and Qwen Code 0.28.0 wrote the files these rules were taken from, on
//! 19 November 2026; no context compaction was seen in them, so `rereads` reads neither.

use std::collections::HashMap;

use serde_json::{Value, json};

/// The Claude Code records for a Gemini CLI session file's `text`, one a line, and the
/// number of its lines that are not JSON objects.
///
/// shortcut: the whole file is held in memory, as a reply's last version can come any
/// number of lines after its first; Gemini CLI's files hold one session each.
#[must_use]
pub fn gemini(text: &str) -> (Vec<String>, u64) {
    let mut order = Vec::new();
    let mut last: HashMap<String, Value> = HashMap::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Some(v) = object(line) else {
            bad += 1;
            continue;
        };
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if !last.contains_key(id) {
                order.push(id.to_owned());
            }
            last.insert(id.to_owned(), v);
        }
    }
    let mut out = Vec::new();
    for id in &order {
        let v = &last[id];
        let ts = v.get("timestamp").cloned().unwrap_or(Value::Null);
        if v.get("type").and_then(Value::as_str) != Some("gemini") {
            out.push(json!({"type": "gemini", "timestamp": ts}).to_string());
            continue;
        }
        let calls = v.get("toolCalls").and_then(Value::as_array);
        let uses = calls.into_iter().flatten().map(|c| {
            tool_use(
                c.get("id"),
                c.get("name").and_then(Value::as_str),
                c.get("args"),
            )
        });
        let tokens = v.get("tokens");
        let count = |k: &str| tokens.and_then(|t| t.get(k)).and_then(Value::as_u64);
        let usage = usage(count("input"), count("cached"), count("output"));
        out.push(assistant(
            &Value::from(id.as_str()),
            v.get("model"),
            uses.collect(),
            &usage,
            &ts,
        ));
        for (n, c) in calls.into_iter().flatten().enumerate() {
            let Some(result) = c.get("result").and_then(Value::as_array) else {
                continue;
            };
            let error = c.get("status").and_then(Value::as_str) == Some("error");
            let text = result
                .iter()
                .filter_map(|p| p.get("functionResponse"))
                .map(response_text)
                .collect::<Vec<_>>()
                .join("\n");
            let block = tool_result(c.get("id"), &Value::from(text), error);
            out.push(user(&Value::from(format!("{id}#{n}")), vec![block], &ts));
        }
    }
    (out, bad)
}

/// The Claude Code records for a Qwen Code session file's `text`, one a line, and the
/// number of its lines that are not JSON objects.
#[must_use]
pub fn qwen(text: &str) -> (Vec<String>, u64) {
    let mut out = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Some(v) = object(line) else {
            bad += 1;
            continue;
        };
        let ts = v.get("timestamp").cloned().unwrap_or(Value::Null);
        let uuid = v.get("uuid").cloned().unwrap_or(Value::Null);
        let parts = v.pointer("/message/parts").and_then(Value::as_array);
        let parts = parts.into_iter().flatten();
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let uses = parts.filter_map(|p| p.get("functionCall")).map(|c| {
                    tool_use(
                        c.get("id"),
                        c.get("name").and_then(Value::as_str),
                        c.get("args"),
                    )
                });
                let u = v.get("usageMetadata");
                let count = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64);
                let usage = usage(
                    count("promptTokenCount"),
                    count("cachedContentTokenCount"),
                    count("candidatesTokenCount"),
                );
                out.push(assistant(
                    &uuid,
                    v.get("model"),
                    uses.collect(),
                    &usage,
                    &ts,
                ));
            }
            Some("tool_result") => {
                let error =
                    v.pointer("/toolCallResult/status").and_then(Value::as_str) == Some("error");
                let blocks = parts
                    .filter_map(|p| p.get("functionResponse"))
                    .map(|r| tool_result(r.get("id"), &Value::from(response_text(r)), error))
                    .collect();
                out.push(user(&uuid, blocks, &ts));
            }
            _ => out.push(json!({"type": "qwen", "timestamp": ts}).to_string()),
        }
    }
    (out, bad)
}

fn object(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line)
        .ok()
        .filter(Value::is_object)
}

/// A Claude `assistant` record. Written as text: `model` must come first in the message,
/// where the scan looks for a reply's model, and a JSON object would sort its keys.
fn assistant(
    uuid: &Value,
    model: Option<&Value>,
    content: Vec<Value>,
    usage: &Value,
    ts: &Value,
) -> String {
    let model = model.and_then(Value::as_str).unwrap_or("");
    format!(
        "{{\"type\":\"assistant\",\"uuid\":{uuid},\"message\":{{\"model\":{},\"role\":\"assistant\",\"content\":{},\"usage\":{usage}}},\"timestamp\":{ts}}}",
        Value::from(model),
        Value::from(content),
    )
}

fn user(uuid: &Value, content: Vec<Value>, ts: &Value) -> String {
    json!({"type": "user", "uuid": uuid,
           "message": {"role": "user", "content": content}, "timestamp": ts})
    .to_string()
}

/// A tool call as a Claude `tool_use` block, with the 2 CLIs' built-in tools named as
/// Claude Code's: `run_shell_command` runs `command` as `Bash` does, `read_file` reads
/// `file_path` as `Read` does, and `replace` (Gemini CLI), `edit` (Qwen Code) and
/// `write_file` change a file. Their arguments already carry Claude Code's names.
fn tool_use(id: Option<&Value>, name: Option<&str>, args: Option<&Value>) -> Value {
    let name = match name.unwrap_or("") {
        "run_shell_command" => "Bash",
        "read_file" => "Read",
        "replace" | "edit" => "Edit",
        "write_file" => "Write",
        other => other,
    };
    json!({"type": "tool_use", "id": id.cloned().unwrap_or(Value::Null),
           "name": name, "input": args.cloned().unwrap_or(Value::Null)})
}

fn tool_result(id: Option<&Value>, content: &Value, error: bool) -> Value {
    json!({"type": "tool_result", "tool_use_id": id.cloned().unwrap_or(Value::Null),
           "content": content, "is_error": error})
}

/// A `functionResponse` part's text: its response's `output` or `error` when that is
/// text, else the response as JSON.
fn response_text(r: &Value) -> String {
    let response = r.get("response").unwrap_or(&Value::Null);
    ["output", "error"]
        .iter()
        .find_map(|k| response.get(k).and_then(Value::as_str))
        .map_or_else(|| response.to_string(), str::to_owned)
}

/// Usage under Claude's names. Both CLIs count a prompt's cached tokens inside its prompt
/// tokens, as the Gemini API does; Claude counts them apart.
fn usage(prompt: Option<u64>, cached: Option<u64>, output: Option<u64>) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(p) = prompt {
        let uncached = p.saturating_sub(cached.unwrap_or(0));
        out.insert("input_tokens".to_owned(), uncached.into());
    }
    if let Some(c) = cached {
        out.insert("cache_read_input_tokens".to_owned(), c.into());
    }
    if let Some(o) = output {
        out.insert("output_tokens".to_owned(), o.into());
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Value {
        serde_json::from_str(line).expect("JSON")
    }

    #[test]
    fn a_gemini_reply_written_again_counts_once_in_its_first_place() {
        let text = concat!(
            "{\"sessionId\":\"s\",\"kind\":\"main\"}\n",
            "{\"id\":\"g\",\"type\":\"gemini\",\"timestamp\":\"t1\",\"model\":\"m\"}\n",
            "{\"id\":\"u\",\"type\":\"user\",\"timestamp\":\"t2\"}\n",
            "not JSON\n",
            "{\"$set\":{}}\n",
            "{\"id\":\"g\",\"type\":\"gemini\",\"timestamp\":\"t1\",\"model\":\"m\",",
            "\"tokens\":{\"input\":10,\"cached\":4,\"output\":2},\"toolCalls\":[{\"id\":\"c\",",
            "\"name\":\"run_shell_command\",\"args\":{\"command\":\"ls\"},\"status\":\"error\",",
            "\"result\":[{\"functionResponse\":{\"response\":{\"error\":\"no\"}}}]}]}\n",
        );
        let (lines, bad) = gemini(text);
        assert_eq!(bad, 1);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("\"message\":{\"model\":\"m\""),
            "{}",
            lines[0]
        );
        let reply = parsed(&lines[0]);
        assert_eq!(
            reply["message"]["content"],
            json!([{"type": "tool_use", "id": "c", "name": "Bash", "input": {"command": "ls"}}])
        );
        assert_eq!(
            reply["message"]["usage"],
            json!({"input_tokens": 6, "cache_read_input_tokens": 4, "output_tokens": 2})
        );
        let result = parsed(&lines[1]);
        assert_eq!(result["uuid"], "g#0");
        assert_eq!(
            result["message"]["content"],
            json!([{"type": "tool_result", "tool_use_id": "c", "content": "no", "is_error": true}])
        );
        assert_eq!(
            parsed(&lines[2]),
            json!({"type": "gemini", "timestamp": "t2"})
        );
    }

    #[test]
    fn a_qwen_result_takes_its_status_from_the_call_result() {
        let text = concat!(
            "{\"uuid\":\"a\",\"type\":\"assistant\",\"model\":\"q\",\"message\":{\"parts\":[",
            "{\"functionCall\":{\"id\":\"c\",\"name\":\"edit\",\"args\":{\"file_path\":\"/f\"}}}]}}\n",
            "{\"uuid\":\"r\",\"type\":\"tool_result\",\"message\":{\"parts\":[{\"functionResponse\":",
            "{\"id\":\"c\",\"response\":{\"code\":1}}}]},\"toolCallResult\":{\"status\":\"error\"}}\n",
            "[]\n",
        );
        let (lines, bad) = qwen(text);
        assert_eq!(bad, 1);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            parsed(&lines[0])["message"]["content"][0]["name"],
            json!("Edit")
        );
        assert_eq!(
            parsed(&lines[1])["message"]["content"],
            json!([{"type": "tool_result", "tool_use_id": "c", "content": "{\"code\":1}", "is_error": true}])
        );
    }
}
