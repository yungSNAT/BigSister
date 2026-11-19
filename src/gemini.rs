//! Gemini CLI's and Qwen Code's session files, rewritten into the Claude Code records the
//! detectors read.
//!
//! Gemini CLI (<https://github.com/google-gemini/gemini-cli>) writes each session to
//! `~/.gemini/tmp/<project>/chats/session-<time>-<id>.jsonl`: a header, then records with
//! an `id` and a `type` of `user` or `gemini`, and `$set`, `$patch` and `$rewindTo`
//! records. It writes a reply again, under the same id, as its tool calls are scheduled and
//! finish, so the last record of a type with an id stands for it, in its own place. A
//! reply's `toolCalls` give each call's `name`, `args`, `status` and `result`, a list of
//! `functionResponse` parts.
//!
//! Qwen Code (<https://github.com/QwenLM/qwen-code>), a fork of Gemini CLI, writes
//! `~/.qwen/projects/<folder>/chats/<session id>.jsonl`: records with a `uuid` and a `type`
//! of `user`, `assistant`, `tool_result` or `system`, whose `message` holds Gemini API
//! `parts`; a `tool_result` record's `toolCallResult` gives the call's `status`.
//!
//! A reply becomes a Claude `assistant` record holding a `tool_use` block for each tool
//! call, and each result a Claude `user` record holding one `tool_result` block, so that
//! the Claude code paths count both agents' tool calls by the same rules. Every other
//! record becomes a record holding only its time, which the detectors ignore. A call whose
//! status is `error` or `cancelled` is an error. A Gemini CLI line with no `id` that is not
//! the header or a `$set`, `$patch` or `$rewindTo` record, a call or result without a
//! non-empty string id (or, for a call, name), and a list field that is not a list count
//! as skipped lines.
//!
//! Gemini CLI 0.69.0 and Qwen Code 0.28.0 wrote the files these rules were taken from, on
//! 19 November 2026; other versions are unverified, and Gemini CLI's older `.json` session
//! files are not read. Neither reader reads compaction (Gemini CLI's `$patch` removals and
//! `$rewindTo` rollbacks, Qwen Code's `chat_compression` system records), so `rereads`
//! reads neither agent, nor a subagent's header field or parent record, so every session
//! counts as a main session.

use std::collections::HashMap;

use serde_json::{Value, json};

/// A Gemini CLI or Qwen Code session file, rewritten a line at a time.
pub enum Rewrite {
    /// A Gemini CLI file, indexed in a first pass.
    Gemini(Gemini),
    Qwen,
}

impl Rewrite {
    /// Appends the Claude Code records for the file's next line to `out`. Returns 1 for a
    /// line that is not a JSON object or is of a shape not known, and the number of its
    /// tool calls and results that are left out.
    pub fn read(&mut self, line: &str, out: &mut Vec<String>) -> u64 {
        match self {
            Self::Gemini(g) => g.read(line, out),
            Self::Qwen => qwen(line, out),
        }
    }
}

/// A Gemini CLI file read twice: first each record's last line, then the lines, each
/// record rewritten at its last version's line. A record is held one line at a time, so
/// the scan's line limit bounds the memory, plus a type and id for each record.
#[derive(Default)]
pub struct Gemini {
    /// By type and id: only a record of the same type stands for an earlier one.
    last: HashMap<(String, String), u64>,
    /// Lines indexed. Lines after them, written since the first pass, are not read.
    indexed: u64,
    /// Lines read in the second pass.
    read: u64,
}

impl Gemini {
    /// Notes the file's next line in the first pass.
    pub fn index(&mut self, line: &str) {
        if let Some(key) = object(line).as_ref().and_then(key) {
            self.last.insert(key, self.indexed);
        }
        self.indexed += 1;
    }

    fn read(&mut self, line: &str, out: &mut Vec<String>) -> u64 {
        let n = self.read;
        self.read += 1;
        if n >= self.indexed || line.trim().is_empty() {
            return 0;
        }
        let Some(v) = object(line) else {
            return 1;
        };
        let Some(key) = key(&v) else {
            let known = ["sessionId", "$set", "$patch", "$rewindTo"];
            return u64::from(!known.iter().any(|k| v.get(k).is_some()));
        };
        if self.last.get(&key).is_some_and(|&last| last != n) {
            return 0;
        }
        gemini_record(&key.1, &v, out)
    }
}

/// A Gemini CLI record's type and id, if it has an id.
fn key(v: &Value) -> Option<(String, String)> {
    let id = v.get("id").and_then(Value::as_str)?;
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    Some((kind.to_owned(), id.to_owned()))
}

/// Appends the Claude Code records for Gemini CLI record `v`, the last of its type with id
/// `id`: a reply and a result for each of its tool calls that has one, or a record holding
/// only its time. Returns how many of its tool calls have no non-empty string id or name,
/// which are left out.
fn gemini_record(id: &str, v: &Value, out: &mut Vec<String>) -> u64 {
    let ts = v.get("timestamp").cloned().unwrap_or(Value::Null);
    if v.get("type").and_then(Value::as_str) != Some("gemini") {
        out.push(json!({"type": "gemini", "timestamp": ts}).to_string());
        return 0;
    }
    let all = v.get("toolCalls").and_then(Value::as_array);
    let calls: Vec<(&Value, Value)> = all
        .into_iter()
        .flatten()
        .filter_map(|c| tool_use(c).map(|u| (c, u)))
        .collect();
    let mut skipped = (all.map_or(0, Vec::len) - calls.len()) as u64 + not_list(v.get("toolCalls"));
    let tokens = v.get("tokens");
    let count = |k: &str| tokens.and_then(|t| t.get(k)).and_then(Value::as_u64);
    let usage = usage(count("input"), count("cached"), count("output"));
    let uses = calls.iter().map(|(_, u)| u.clone()).collect();
    out.push(assistant(
        &Value::from(id),
        v.get("model"),
        uses,
        &usage,
        &ts,
    ));
    for (n, (c, u)) in calls.iter().enumerate() {
        skipped += not_list(c.get("result"));
        let Some(result) = c.get("result").and_then(Value::as_array) else {
            continue;
        };
        let error = failed(c.get("status"));
        let text = result
            .iter()
            .filter_map(|p| p.get("functionResponse"))
            .map(response_text)
            .collect::<Vec<_>>()
            .join("\n");
        let block = tool_result(&u["id"], &Value::from(text), error);
        out.push(user(&Value::from(format!("{id}#{n}")), vec![block], &ts));
    }
    skipped
}

/// Appends the Claude Code records for a Qwen Code session file's `line` to `out`. Returns
/// 1 for a line that is not a JSON object, and the number of its tool calls and results
/// that have no non-empty string id (or, for a call, name), which are left out.
fn qwen(line: &str, out: &mut Vec<String>) -> u64 {
    if line.trim().is_empty() {
        return 0;
    }
    let Some(v) = object(line) else {
        return 1;
    };
    let ts = v.get("timestamp").cloned().unwrap_or(Value::Null);
    let uuid = v.get("uuid").cloned().unwrap_or(Value::Null);
    let mut bad = not_list(v.pointer("/message/parts"));
    let parts = v.pointer("/message/parts").and_then(Value::as_array);
    let parts = parts.into_iter().flatten();
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            let calls: Vec<&Value> = parts.filter_map(|p| p.get("functionCall")).collect();
            let uses: Vec<Value> = calls.iter().copied().filter_map(tool_use).collect();
            bad += (calls.len() - uses.len()) as u64;
            let u = v.get("usageMetadata");
            let count = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64);
            let usage = usage(
                count("promptTokenCount"),
                count("cachedContentTokenCount"),
                count("candidatesTokenCount"),
            );
            out.push(assistant(&uuid, v.get("model"), uses, &usage, &ts));
        }
        Some("tool_result") => {
            let error = failed(v.pointer("/toolCallResult/status"));
            let responses: Vec<&Value> = parts.filter_map(|p| p.get("functionResponse")).collect();
            let blocks: Vec<Value> = responses
                .iter()
                .filter(|r| r.get("id").is_some_and(non_empty))
                .map(|r| tool_result(&r["id"], &Value::from(response_text(r)), error))
                .collect();
            bad += (responses.len() - blocks.len()) as u64;
            out.push(user(&uuid, blocks, &ts));
        }
        _ => out.push(json!({"type": "qwen", "timestamp": ts}).to_string()),
    }
    bad
}

/// `line` as a JSON object, after any byte order mark.
fn object(line: &str) -> Option<Value> {
    serde_json::from_str::<Value>(line.trim_start_matches('\u{feff}'))
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

/// Tool call `c` (its `id`, `name` and `args`) as a Claude `tool_use` block, or `None`
/// without a non-empty string id and name. The 2 CLIs' built-in tools are named as Claude
/// Code's: `run_shell_command` runs `command` as `Bash` does, `read_file` reads `file_path`
/// as `Read` does, and `replace` (Gemini CLI), `edit` (Qwen Code) and `write_file` change
/// a file. Their arguments already carry Claude Code's names.
fn tool_use(c: &Value) -> Option<Value> {
    let id = c.get("id").filter(|id| non_empty(id))?;
    let name = match c
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())?
    {
        "run_shell_command" => "Bash",
        "read_file" => "Read",
        "replace" | "edit" => "Edit",
        "write_file" => "Write",
        other => other,
    };
    let input = c.get("args").cloned().unwrap_or(Value::Null);
    Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
}

/// Whether a call's status says it failed: `error`, or `cancelled`, as Claude Code records
/// a refused call as an error.
fn failed(status: Option<&Value>) -> bool {
    matches!(status.and_then(Value::as_str), Some("error" | "cancelled"))
}

/// 1 for a field that is present but not a list, else 0.
fn not_list(v: Option<&Value>) -> u64 {
    u64::from(v.is_some_and(|v| !v.is_array()))
}

/// Whether `v` is a string holding at least one character.
fn non_empty(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty())
}

fn tool_result(id: &Value, content: &Value, error: bool) -> Value {
    json!({"type": "tool_result", "tool_use_id": id, "content": content, "is_error": error})
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

    /// A Gemini CLI file's `text` read as the scan reads it, in 2 passes.
    fn gemini(text: &str) -> (Vec<String>, u64) {
        let mut g = Gemini::default();
        text.lines().for_each(|l| g.index(l));
        read_all(Rewrite::Gemini(g), text)
    }

    fn qwen(text: &str) -> (Vec<String>, u64) {
        read_all(Rewrite::Qwen, text)
    }

    fn read_all(mut r: Rewrite, text: &str) -> (Vec<String>, u64) {
        let mut out = Vec::new();
        let bad = text.lines().map(|l| r.read(l, &mut out)).sum();
        (out, bad)
    }

    #[test]
    fn a_gemini_reply_written_again_counts_once_in_its_last_place() {
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
        assert_eq!(
            parsed(&lines[0]),
            json!({"type": "gemini", "timestamp": "t2"})
        );
        assert!(
            lines[1].contains("\"message\":{\"model\":\"m\""),
            "{}",
            lines[1]
        );
        let reply = parsed(&lines[1]);
        assert_eq!(
            reply["message"]["content"],
            json!([{"type": "tool_use", "id": "c", "name": "Bash", "input": {"command": "ls"}}])
        );
        assert_eq!(
            reply["message"]["usage"],
            json!({"input_tokens": 6, "cache_read_input_tokens": 4, "output_tokens": 2})
        );
        let result = parsed(&lines[2]);
        assert_eq!(result["uuid"], "g#0");
        assert_eq!(
            result["message"]["content"],
            json!([{"type": "tool_result", "tool_use_id": "c", "content": "no", "is_error": true}])
        );
    }

    /// A tool call without a non-empty string id and name, or a result without such an
    /// id, is not reported: it counts as skipped.
    #[test]
    fn a_call_without_an_id_or_name_is_skipped() {
        let text = concat!(
            "{\"id\":\"g\",\"type\":\"gemini\",\"model\":\"m\",\"toolCalls\":[{},",
            "{\"id\":\"c\",\"name\":\"read_file\",\"args\":{}},{\"name\":\"x\"},",
            "{\"id\":\"\",\"name\":\"read_file\"},{\"id\":\"d\",\"name\":\"\"}]}\n",
        );
        let (lines, bad) = gemini(text);
        assert_eq!(bad, 4);
        assert_eq!(
            parsed(&lines[0])["message"]["content"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        let text = concat!(
            "{\"uuid\":\"a\",\"type\":\"assistant\",\"message\":{\"parts\":[{\"functionCall\":{}},",
            "{\"functionCall\":{\"id\":\"c\",\"name\":\"edit\"}},",
            "{\"functionCall\":{\"id\":\"\",\"name\":\"edit\"}}]}}\n",
            "{\"uuid\":\"r\",\"type\":\"tool_result\",\"message\":{\"parts\":[",
            "{\"functionResponse\":{\"response\":{}}},",
            "{\"functionResponse\":{\"id\":\"\",\"response\":{}}}]}}\n",
        );
        let (lines, bad) = qwen(text);
        assert_eq!(bad, 4);
        assert_eq!(
            parsed(&lines[0])["message"]["content"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(parsed(&lines[1])["message"]["content"], json!([]));
    }

    /// A record shape these readers do not know, or a list field that is not a list, counts
    /// as skipped; the header and the `$set`, `$patch` and `$rewindTo` records do not.
    #[test]
    fn unknown_record_shapes_count_as_skipped() {
        let text = concat!(
            "{\"sessionId\":\"s\",\"projectHash\":\"h\",\"kind\":\"main\"}\n",
            "{\"$set\":{}}\n{\"$patch\":{}}\n{\"$rewindTo\":\"x\"}\n",
            "{\"$newKind\":{}}\n",
            "{\"id\":\"g\",\"type\":\"gemini\",\"toolCalls\":{}}\n",
            "{\"id\":\"h\",\"type\":\"gemini\",\"toolCalls\":[",
            "{\"id\":\"c\",\"name\":\"read_file\",\"result\":\"x\"}]}\n",
        );
        assert_eq!(gemini(text).1, 3);
        let text = "{\"uuid\":\"a\",\"type\":\"assistant\",\"message\":{\"parts\":{}}}\n";
        assert_eq!(qwen(text).1, 1);
    }

    /// A cancelled call counts as an error, as Claude Code records a refused call.
    #[test]
    fn a_cancelled_call_is_an_error() {
        let text = concat!(
            "{\"id\":\"g\",\"type\":\"gemini\",\"toolCalls\":[{\"id\":\"c\",\"name\":\"x\",",
            "\"status\":\"cancelled\",\"result\":[]}]}\n",
        );
        let (lines, _) = gemini(text);
        assert_eq!(
            parsed(&lines[1])["message"]["content"][0]["is_error"],
            json!(true)
        );
        let text = concat!(
            "{\"uuid\":\"r\",\"type\":\"tool_result\",\"message\":{\"parts\":[{\"functionResponse\":",
            "{\"id\":\"c\",\"response\":{}}}]},\"toolCallResult\":{\"status\":\"cancelled\"}}\n",
        );
        let (lines, _) = qwen(text);
        assert_eq!(
            parsed(&lines[0])["message"]["content"][0]["is_error"],
            json!(true)
        );
    }

    /// A record of another type with a reply's id neither replaces the reply nor is lost.
    #[test]
    fn an_id_shared_across_record_types_keeps_both_records() {
        let text = concat!(
            "{\"id\":\"x\",\"type\":\"gemini\",\"timestamp\":\"t1\",\"toolCalls\":[",
            "{\"id\":\"c\",\"name\":\"read_file\",\"args\":{}}]}\n",
            "{\"id\":\"x\",\"type\":\"user\",\"timestamp\":\"t2\"}\n",
        );
        let (lines, _) = gemini(text);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            parsed(&lines[0])["message"]["content"][0]["name"],
            json!("Read")
        );
        assert_eq!(
            parsed(&lines[1]),
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
