//! pi's session records, rewritten into the Claude Code records the detectors read.
//!
//! pi (<https://github.com/earendil-works/pi>) writes one JSONL entry a line, as its
//! `docs/session-format.md` documents: `message` entries whose message has role
//! `assistant` (content blocks, among them `toolCall` blocks), `toolResult` or another
//! role, and `compaction` entries. An assistant message becomes a Claude `assistant`
//! record, a tool result a Claude `user` record holding one `tool_result` block and a
//! compaction a Claude `compact_boundary`, so that the Claude code paths count pi's
//! tool calls by the same rules. Every other entry is passed on unchanged, which the
//! detectors ignore.

use serde_json::{Map, Value, json};

/// The Claude Code record for pi entry `line`, or `None` to pass the line on unchanged.
/// A line that is not JSON is passed on, to be counted as a skipped line.
#[must_use]
pub fn as_claude(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ts = v.get("timestamp").cloned().unwrap_or(Value::Null);
    // pi copies an entry with its id and timestamp into a fork, so the 2 together find a
    // copy. An id alone is 8 hexadecimal characters, which other sessions may repeat.
    let uuid = match (v.get("id").and_then(Value::as_str), ts.as_str()) {
        (Some(id), Some(t)) => Value::from(format!("{id}@{t}")),
        _ => Value::Null,
    };
    match v.get("type").and_then(Value::as_str)? {
        "compaction" => Some(
            json!({"type": "system", "subtype": "compact_boundary", "uuid": uuid, "timestamp": ts})
                .to_string(),
        ),
        "message" => message(v.get("message")?, &uuid, &ts),
        _ => None,
    }
}

fn message(m: &Value, uuid: &Value, ts: &Value) -> Option<String> {
    match m.get("role").and_then(Value::as_str)? {
        "assistant" => {
            let content: Vec<Value> = m
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(tool_use)
                .collect();
            let model = m.get("model").and_then(Value::as_str).unwrap_or("");
            // Written as text: `model` must come first in the message, where the scan
            // looks for a reply's model, and a JSON object would sort its keys.
            Some(format!(
                "{{\"type\":\"assistant\",\"uuid\":{uuid},\"message\":{{\"model\":{},\"role\":\"assistant\",\"content\":{},\"usage\":{}}},\"timestamp\":{ts}}}",
                Value::from(model),
                Value::from(content),
                usage(m.get("usage")),
            ))
        }
        "toolResult" => {
            let block = json!({
                "type": "tool_result",
                "tool_use_id": m.get("toolCallId").cloned().unwrap_or(Value::Null),
                "content": m.get("content").cloned().unwrap_or(Value::Null),
                "is_error": m.get("isError").and_then(Value::as_bool).unwrap_or(false),
            });
            Some(
                json!({"type": "user", "uuid": uuid,
                       "message": {"role": "user", "content": [block]}, "timestamp": ts})
                .to_string(),
            )
        }
        _ => None,
    }
}

/// A `toolCall` block as a Claude `tool_use` block, with pi's built-in tools named as
/// Claude Code's: `bash` and `powershell` run commands as `Bash` does, `read` reads a
/// path as `Read` reads `file_path`, and `edit` and `write` change files.
fn tool_use(b: &Value) -> Option<Value> {
    if b.get("type").and_then(Value::as_str) != Some("toolCall") {
        return None;
    }
    let args = b.get("arguments").cloned().unwrap_or(Value::Null);
    let (name, input) = match b.get("name").and_then(Value::as_str).unwrap_or("") {
        "bash" | "powershell" => ("Bash", args),
        "read" => {
            let path = args.get("path").cloned().unwrap_or(Value::Null);
            ("Read", json!({"file_path": path}))
        }
        "edit" => ("Edit", args),
        "write" => ("Write", args),
        other => (other, args),
    };
    Some(
        json!({"type": "tool_use", "id": b.get("id").cloned().unwrap_or(Value::Null),
                "name": name, "input": input}),
    )
}

/// pi's usage under Claude's names, as the scan reads a prompt's size from them.
fn usage(u: Option<&Value>) -> Value {
    let mut out = Map::new();
    for (pi, claude) in [
        ("input", "input_tokens"),
        ("cacheWrite", "cache_creation_input_tokens"),
        ("cacheRead", "cache_read_input_tokens"),
        ("output", "output_tokens"),
    ] {
        if let Some(n) = u.and_then(|u| u.get(pi)) {
            out.insert(claude.to_owned(), n.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Value {
        serde_json::from_str(&as_claude(line).expect("rewritten")).expect("JSON")
    }

    #[test]
    fn a_reply_keeps_its_model_first_and_its_tool_calls_under_claude_names() {
        let line = r#"{"type":"message","id":"a1","parentId":null,"timestamp":"2026-11-09T06:00:00.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"t"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"/w/a.rs","offset":3}},{"type":"toolCall","id":"c2","name":"powershell","arguments":{"command":"ls"}}],"model":"claude-haiku-5-5","usage":{"input":5,"output":1,"cacheRead":7,"cacheWrite":2}}}"#;
        let out = as_claude(line).expect("rewritten");
        assert!(
            out.contains("\"message\":{\"model\":\"claude-haiku-5-5\""),
            "{out}"
        );
        let v = parsed(line);
        assert_eq!(v["uuid"], "a1@2026-11-09T06:00:00.000Z");
        assert_eq!(
            v["message"]["content"],
            json!([
                {"type": "tool_use", "id": "c1", "name": "Read", "input": {"file_path": "/w/a.rs"}},
                {"type": "tool_use", "id": "c2", "name": "Bash", "input": {"command": "ls"}},
            ])
        );
        assert_eq!(
            v["message"]["usage"],
            json!({"input_tokens": 5, "cache_creation_input_tokens": 2,
                   "cache_read_input_tokens": 7, "output_tokens": 1})
        );
    }

    #[test]
    fn a_tool_result_and_a_compaction_take_claude_shapes() {
        let result = r#"{"type":"message","id":"b2","timestamp":"t","message":{"role":"toolResult","toolCallId":"c1","toolName":"bash","content":[{"type":"text","text":"out"}],"isError":true}}"#;
        assert_eq!(
            parsed(result)["message"]["content"],
            json!([{"type": "tool_result", "tool_use_id": "c1",
                    "content": [{"type": "text", "text": "out"}], "is_error": true}])
        );
        let compaction = r#"{"type":"compaction","id":"c3","timestamp":"t","summary":"s","firstKeptEntryId":"b2"}"#;
        let v = parsed(compaction);
        assert_eq!(
            (v["type"].as_str(), v["subtype"].as_str()),
            (Some("system"), Some("compact_boundary"))
        );
    }

    #[test]
    fn other_entries_and_bad_lines_pass_unchanged() {
        for line in [
            r#"{"type":"session","version":3,"id":"u","timestamp":"t","cwd":"/w"}"#,
            r#"{"type":"message","id":"d4","timestamp":"t","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"model_change","id":"e5","timestamp":"t","provider":"p","modelId":"m"}"#,
            "{not json}",
        ] {
            assert_eq!(as_claude(line), None, "{line}");
        }
    }
}
