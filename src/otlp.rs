//! OpenTelemetry GenAI spans in OTLP/JSON trace files, rewritten into the Claude Code
//! records the detectors read.
//!
//! An OTLP/JSON trace export (<https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding>)
//! is an object whose `resourceSpans[].scopeSpans[].spans[]` each carry `traceId`,
//! `spanId`, `endTimeUnixNano`, a `status` and `attributes`, a list of `{key, value}`
//! where `value` holds one of `stringValue`, `intValue` (a JSON string or number),
//! `doubleValue` or `boolValue`. A `*.jsonl` or `*.jsonl.zst` file holds one such object
//! a line, as a collector's file exporter writes; the scan reads a `*.json` file as one
//! object, compact or not.
//!
//! The GenAI semantic conventions name the attributes read here:
//!
//! - An inference span has a model (`gen_ai.response.model`, else
//!   `gen_ai.request.model`) and `gen_ai.usage.input_tokens` or
//!   `gen_ai.usage.output_tokens`. It becomes a Claude `assistant` record whose id is the
//!   span's, in the session named by `gen_ai.conversation.id`, else the trace. Cache reads
//!   are `gen_ai.usage.cache_read.input_tokens` and cache writes
//!   `gen_ai.usage.cache_write.input_tokens`, or the older
//!   `gen_ai.usage.cache_creation.input_tokens`. The conventions count both inside
//!   `gen_ai.usage.input_tokens`, so they are taken out of it; a span whose cache tokens
//!   exceed its input tokens breaks that rule, and its input is left out rather than
//!   guessed, so `models` counts the reply as skipped.
//! - A tool span has `gen_ai.operation.name` `execute_tool` and a `gen_ai.tool.name`. It
//!   becomes a synthetic `assistant` record, which `models` passes over, holding one
//!   `tool_use` block with an empty input, and a
//!   `user` record holding its `tool_result`, with no content, which is an error when the
//!   span's status code is 2 (error) or it has an `error.type`. Tool inputs and results
//!   are never read.
//!
//! Each record is written at its span's end time, the spans of a line in end-time order.
//! Other spans are ignored. A line that is not a JSON object with a `resourceSpans` list
//! counts as a skipped line, as does a GenAI span without a span id or an end time.

use serde_json::{Map, Value, json};

/// Appends the Claude Code records for one OTLP/JSON trace export to `out`. Returns 1 for
/// a line that is not one, plus the GenAI spans left out for lacking an id or a time.
pub fn read(line: &str, out: &mut Vec<String>) -> u64 {
    let line = line.trim_start_matches('\u{feff}');
    if line.trim().is_empty() {
        return 0;
    }
    let parsed = serde_json::from_str::<Value>(line).ok();
    let Some(resources) = parsed
        .as_ref()
        .and_then(|v| v.get("resourceSpans"))
        .and_then(Value::as_array)
    else {
        return 1;
    };
    let mut bad = 0;
    let mut spans: Vec<(i128, Vec<String>)> = Vec::new();
    let all = resources
        .iter()
        .filter_map(|r| r.get("scopeSpans").and_then(Value::as_array))
        .flatten()
        .filter_map(|s| s.get("spans").and_then(Value::as_array))
        .flatten();
    for span in all {
        match span_records(span) {
            Ok(Some(found)) => spans.push(found),
            Ok(None) => {}
            Err(()) => bad += 1,
        }
    }
    spans.sort_by_key(|(end, _)| *end);
    out.extend(spans.into_iter().flat_map(|(_, r)| r));
    bad
}

/// A GenAI span's end time and its records; `None` for another span, and an error for a
/// GenAI span without a span id or an end time.
fn span_records(span: &Value) -> Result<Option<(i128, Vec<String>)>, ()> {
    let a = Attrs(span.get("attributes").and_then(Value::as_array));
    let tool = a.str("gen_ai.operation.name") == Some("execute_tool")
        && a.str("gen_ai.tool.name").is_some_and(|n| !n.is_empty());
    let model = a
        .str("gen_ai.response.model")
        .or_else(|| a.str("gen_ai.request.model"))
        .filter(|m| !m.is_empty());
    let usage = a.int("gen_ai.usage.input_tokens").is_some()
        || a.int("gen_ai.usage.output_tokens").is_some();
    if !tool && !(model.is_some() && usage) {
        return Ok(None);
    }
    let id = span
        .get("spanId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let end = span.get("endTimeUnixNano").and_then(nanos);
    let ts = end.and_then(|n| jiff::Timestamp::from_nanosecond(n).ok());
    let (Some(id), Some(end), Some(ts)) = (id, end, ts) else {
        return Err(());
    };
    let ts = Value::from(ts.to_string());
    let session = a
        .str("gen_ai.conversation.id")
        .or_else(|| span.get("traceId").and_then(Value::as_str))
        .unwrap_or("");
    let records = if tool {
        tool_records(span, &a, id, session, &ts)
    } else {
        vec![reply(&a, model.unwrap_or(""), id, session, &ts)]
    };
    Ok(Some((end, records)))
}

/// A span's attributes.
struct Attrs<'a>(Option<&'a Vec<Value>>);

impl<'a> Attrs<'a> {
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.0?
            .iter()
            .find(|kv| kv.get("key").and_then(Value::as_str) == Some(key))
            .and_then(|kv| kv.get("value"))
    }

    fn str(&self, key: &str) -> Option<&'a str> {
        self.get(key)?.get("stringValue")?.as_str()
    }

    /// An `intValue`, which OTLP/JSON writes as a string or a number.
    fn int(&self, key: &str) -> Option<u64> {
        match self.get(key)?.get("intValue")? {
            Value::String(s) => s.parse().ok(),
            v => v.as_u64(),
        }
    }
}

/// A time in nanoseconds since the epoch, written as a string or a number.
fn nanos(v: &Value) -> Option<i128> {
    match v {
        Value::String(s) => s.parse().ok(),
        v => v.as_u64().map(i128::from),
    }
}

/// An inference span as a Claude `assistant` record. Written as text: `model` must come
/// first in the message, where the scan looks for a reply's model.
fn reply(a: &Attrs<'_>, model: &str, id: &str, session: &str, ts: &Value) -> String {
    let read = a.int("gen_ai.usage.cache_read.input_tokens");
    let write = a
        .int("gen_ai.usage.cache_write.input_tokens")
        .or_else(|| a.int("gen_ai.usage.cache_creation.input_tokens"));
    let mut usage = Map::new();
    let cached = read.unwrap_or(0).saturating_add(write.unwrap_or(0));
    if let Some(input) = a.int("gen_ai.usage.input_tokens")
        && cached <= input
    {
        usage.insert("input_tokens".to_owned(), (input - cached).into());
    }
    if let Some(r) = read {
        usage.insert("cache_read_input_tokens".to_owned(), r.into());
    }
    if let Some(w) = write {
        usage.insert("cache_creation_input_tokens".to_owned(), w.into());
    }
    if let Some(o) = a.int("gen_ai.usage.output_tokens") {
        usage.insert("output_tokens".to_owned(), o.into());
    }
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":{},\"message\":{{\"model\":{},\"id\":{},\"role\":\"assistant\",\"content\":[],\"usage\":{}}},\"timestamp\":{ts}}}",
        Value::from(session),
        Value::from(model),
        Value::from(id),
        Value::Object(usage),
    )
}

/// A tool span as a call and its result.
fn tool_records(span: &Value, a: &Attrs<'_>, id: &str, session: &str, ts: &Value) -> Vec<String> {
    let call = a
        .str("gen_ai.tool.call.id")
        .filter(|c| !c.is_empty())
        .unwrap_or(id);
    let name = a.str("gen_ai.tool.name").unwrap_or("");
    let error = span.pointer("/status/code").and_then(Value::as_u64) == Some(2)
        || a.get("error.type").is_some();
    let call_block = json!({"type": "tool_use", "id": call, "name": name, "input": {}});
    let result =
        json!({"type": "tool_result", "tool_use_id": call, "content": "", "is_error": error});
    vec![
        json!({"type": "assistant", "sessionId": session, "uuid": format!("{id}-call"),
               "message": {"model": "<synthetic>", "role": "assistant", "content": [call_block]}, "timestamp": ts})
        .to_string(),
        json!({"type": "user", "sessionId": session, "uuid": format!("{id}-result"),
               "message": {"role": "user", "content": [result]}, "timestamp": ts})
        .to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(line: &str) -> (Vec<Value>, u64) {
        let mut out = Vec::new();
        let bad = read(line, &mut out);
        let parsed = out
            .iter()
            .map(|l| serde_json::from_str(l).expect("JSON"))
            .collect();
        (parsed, bad)
    }

    /// One export holding `spans`, each a JSON object.
    fn export(spans: &[Value]) -> String {
        json!({"resourceSpans": [{"scopeSpans": [{"spans": spans}]}]}).to_string()
    }

    fn attr(key: &str, value: Value) -> Value {
        json!({"key": key, "value": value})
    }

    fn inference(id: &str, end: &str, attrs: Vec<Value>) -> Value {
        json!({"traceId": "t1", "spanId": id, "endTimeUnixNano": end, "attributes": attrs})
    }

    #[test]
    fn cache_tokens_are_taken_out_of_the_input_they_are_counted_in() {
        let span = inference(
            "s1",
            "1763600000000000000",
            vec![
                attr("gen_ai.request.model", json!({"stringValue": "m-req"})),
                attr("gen_ai.response.model", json!({"stringValue": "m-resp"})),
                attr("gen_ai.usage.input_tokens", json!({"intValue": "1000"})),
                attr(
                    "gen_ai.usage.cache_read.input_tokens",
                    json!({"intValue": 600}),
                ),
                attr(
                    "gen_ai.usage.cache_creation.input_tokens",
                    json!({"intValue": "100"}),
                ),
                attr("gen_ai.usage.output_tokens", json!({"intValue": "50"})),
            ],
        );
        let (out, bad) = records(&export(&[span]));
        assert_eq!(bad, 0);
        let r = &out[0];
        assert_eq!(r["type"], "assistant");
        assert_eq!(r["sessionId"], "t1");
        assert_eq!(r["message"]["id"], "s1");
        assert_eq!(r["message"]["model"], "m-resp");
        assert_eq!(r["timestamp"], "2025-11-20T00:53:20Z");
        assert_eq!(
            r["message"]["usage"],
            json!({"input_tokens": 300, "cache_read_input_tokens": 600,
                   "cache_creation_input_tokens": 100, "output_tokens": 50})
        );
    }

    #[test]
    fn the_current_cache_write_key_wins_and_the_conversation_names_the_session() {
        let span = inference(
            "s1",
            "1",
            vec![
                attr("gen_ai.request.model", json!({"stringValue": "m"})),
                attr("gen_ai.conversation.id", json!({"stringValue": "conv"})),
                attr("gen_ai.usage.input_tokens", json!({"intValue": "100"})),
                attr(
                    "gen_ai.usage.cache_write.input_tokens",
                    json!({"intValue": "40"}),
                ),
                attr(
                    "gen_ai.usage.cache_creation.input_tokens",
                    json!({"intValue": "90"}),
                ),
            ],
        );
        let (out, _) = records(&export(&[span]));
        assert_eq!(out[0]["sessionId"], "conv");
        assert_eq!(out[0]["message"]["model"], "m");
        assert_eq!(
            out[0]["message"]["usage"],
            json!({"input_tokens": 60, "cache_creation_input_tokens": 40})
        );
    }

    #[test]
    fn input_is_left_out_when_missing_or_smaller_than_its_cache_tokens() {
        let model = attr("gen_ai.request.model", json!({"stringValue": "m"}));
        let over = inference(
            "a",
            "1",
            vec![
                model.clone(),
                attr("gen_ai.usage.input_tokens", json!({"intValue": "10"})),
                attr(
                    "gen_ai.usage.cache_read.input_tokens",
                    json!({"intValue": "600"}),
                ),
                attr("gen_ai.usage.output_tokens", json!({"intValue": "5"})),
            ],
        );
        let output_only = inference(
            "b",
            "2",
            vec![
                model,
                attr("gen_ai.usage.output_tokens", json!({"intValue": "5"})),
            ],
        );
        let (out, bad) = records(&export(&[over, output_only]));
        assert_eq!(bad, 0);
        assert_eq!(
            out[0]["message"]["usage"],
            json!({"cache_read_input_tokens": 600, "output_tokens": 5})
        );
        assert_eq!(out[1]["message"]["usage"], json!({"output_tokens": 5}));
    }

    #[test]
    fn a_tool_span_is_a_call_and_its_result_with_no_content() {
        let ok = json!({"traceId": "t", "spanId": "x1", "endTimeUnixNano": "20",
        "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "search"})),
            attr("gen_ai.tool.call.id", json!({"stringValue": "call-9"})),
            attr("gen_ai.tool.call.arguments", json!({"stringValue": "SENTINEL"})),
        ]});
        let failed = json!({"traceId": "t", "spanId": "x2", "endTimeUnixNano": "10",
        "status": {"code": 2},
        "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
        ]});
        let typed = json!({"traceId": "t", "spanId": "x3", "endTimeUnixNano": "30",
        "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
            attr("error.type", json!({"stringValue": "timeout"})),
        ]});
        let mut out = Vec::new();
        assert_eq!(read(&export(&[ok, failed, typed]), &mut out), 0);
        assert!(!out.concat().contains("SENTINEL"));
        let out: Vec<Value> = out
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out.len(), 6);
        // In end-time order: x2, x1, x3.
        let call = &out[0]["message"]["content"][0];
        assert_eq!(call["id"], "x2");
        assert_eq!(call["name"], "fetch");
        assert_eq!(call["input"], json!({}));
        assert_eq!(out[0]["message"]["model"], "<synthetic>");
        let result = &out[1]["message"]["content"][0];
        assert_eq!(result["tool_use_id"], "x2");
        assert_eq!(result["is_error"], true);
        assert_eq!(out[2]["message"]["content"][0]["id"], "call-9");
        assert_eq!(out[3]["message"]["content"][0]["is_error"], false);
        assert_eq!(out[5]["message"]["content"][0]["is_error"], true);
    }

    #[test]
    fn other_spans_are_ignored_and_broken_ones_counted() {
        let http = json!({"spanId": "h", "endTimeUnixNano": "1",
            "attributes": [attr("http.method", json!({"stringValue": "GET"}))]});
        let no_id = inference(
            "",
            "1",
            vec![
                attr("gen_ai.request.model", json!({"stringValue": "m"})),
                attr("gen_ai.usage.input_tokens", json!({"intValue": "1"})),
            ],
        );
        let no_time = json!({"spanId": "z", "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "t"})),
        ]});
        let (out, bad) = records(&export(&[http, no_id, no_time]));
        assert!(out.is_empty());
        assert_eq!(bad, 2);
        assert_eq!(records("not JSON").1, 1);
        assert_eq!(records("{\"resourceLogs\":[]}").1, 1);
        assert_eq!(records("  ").1, 0);
    }
}
