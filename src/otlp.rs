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
//!   `gen_ai.usage.output_tokens`, or their deprecated names `gen_ai.usage.prompt_tokens`
//!   and `gen_ai.usage.completion_tokens`. It becomes a Claude `assistant` record whose id is the
//!   span's, in the session named by `gen_ai.conversation.id`, else the trace. Cache reads
//!   are `gen_ai.usage.cache_read.input_tokens` and cache writes
//!   `gen_ai.usage.cache_write.input_tokens`, or the older
//!   `gen_ai.usage.cache_creation.input_tokens`; an absent count is 0, as the conventions
//!   record one only when it is available and not 0. The conventions count both inside
//!   `gen_ai.usage.input_tokens`, so they are taken out of it; a span whose cache tokens
//!   exceed its input tokens breaks that rule, and its input is left out rather than
//!   guessed, so `models` counts the reply as skipped.
//! - A tool span has `gen_ai.operation.name` `execute_tool` and a `gen_ai.tool.name`. It
//!   becomes a synthetic `assistant` record, which `models` passes over, holding one
//!   `tool_use` block with an empty input under the span's id (a tool call id need not be
//!   unique across traces), and a `user` record holding its `tool_result`, with no
//!   content, which is an error when the
//!   span's status code is 2 (error) or it has an `error.type`. Tool inputs and results
//!   are never read.
//!
//! Each record is written at its span's end time, the spans of a line in end-time order.
//! Other spans are ignored, as is a span whose trace and span ids an earlier line of the
//! file held, as when an exporter writes a batch again. A line that is not a JSON object
//! with a `resourceSpans` list counts as a skipped line, as does a GenAI span without a
//! span id or an end time.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

/// An OTLP/JSON trace file, read a line at a time.
#[derive(Default)]
pub struct Otlp {
    /// The trace and span ids of the spans read so far.
    seen: HashSet<(String, String)>,
}

impl Otlp {
    /// Appends the Claude Code records for one OTLP/JSON trace export to `out`. Returns 1
    /// for a line that is not one, plus the GenAI spans left out for lacking an id or a time.
    pub fn read(&mut self, line: &str, out: &mut Vec<String>) -> u64 {
        read(line, out, &mut self.seen)
    }
}

fn read(line: &str, out: &mut Vec<String>, seen: &mut HashSet<(String, String)>) -> u64 {
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
        let ids = ["traceId", "spanId"].map(|k| span.get(k).and_then(Value::as_str));
        if let [Some(trace), Some(id)] = ids
            && !seen.insert((trace.to_owned(), id.to_owned()))
        {
            continue;
        }
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
    let usage = a.input().is_some() || a.output().is_some();
    if !tool && !(model.is_some() && usage) {
        return Ok(None);
    }
    let trace = span.get("traceId").and_then(Value::as_str);
    // A span id is unique only within its trace, so the trace qualifies it.
    let id = span
        .get("spanId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("{}/{s}", trace.unwrap_or("")));
    let end = span.get("endTimeUnixNano").and_then(nanos);
    let ts = end.and_then(|n| jiff::Timestamp::from_nanosecond(n).ok());
    let (Some(id), Some(end), Some(ts)) = (id, end, ts) else {
        return Err(());
    };
    let ts = Value::from(ts.to_string());
    let session = a.str("gen_ai.conversation.id").or(trace).unwrap_or("");
    let records = if tool {
        tool_records(span, &a, &id, session, &ts)
    } else {
        vec![reply(&a, model.unwrap_or(""), &id, session, &ts)]
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

    /// `gen_ai.usage.input_tokens`, or its deprecated name.
    fn input(&self) -> Option<u64> {
        self.int("gen_ai.usage.input_tokens")
            .or_else(|| self.int("gen_ai.usage.prompt_tokens"))
    }

    /// `gen_ai.usage.output_tokens`, or its deprecated name.
    fn output(&self) -> Option<u64> {
        self.int("gen_ai.usage.output_tokens")
            .or_else(|| self.int("gen_ai.usage.completion_tokens"))
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
    // The conventions record a cache count only when it is available and not 0, so an
    // absent one reads as 0. An output count has no such condition: a span without one
    // gets none, and `models` counts the reply skipped rather than taking 0.
    let read = a.int("gen_ai.usage.cache_read.input_tokens").unwrap_or(0);
    let write = a
        .int("gen_ai.usage.cache_write.input_tokens")
        .or_else(|| a.int("gen_ai.usage.cache_creation.input_tokens"))
        .unwrap_or(0);
    let mut usage = Map::new();
    let cached = read.saturating_add(write);
    if let Some(input) = a.input()
        && cached <= input
    {
        usage.insert("input_tokens".to_owned(), (input - cached).into());
    }
    usage.insert("cache_read_input_tokens".to_owned(), read.into());
    usage.insert("cache_creation_input_tokens".to_owned(), write.into());
    if let Some(output) = a.output() {
        usage.insert("output_tokens".to_owned(), output.into());
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
    let name = a.str("gen_ai.tool.name").unwrap_or("");
    let error = span.pointer("/status/code").and_then(Value::as_u64) == Some(2)
        || a.get("error.type").is_some();
    let call_block = json!({"type": "tool_use", "id": id, "name": name, "input": {}});
    let result =
        json!({"type": "tool_result", "tool_use_id": id, "content": "", "is_error": error});
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
        let bad = Otlp::default().read(line, &mut out);
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
        assert_eq!(r["message"]["id"], "t1/s1");
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
            json!({"input_tokens": 60, "cache_read_input_tokens": 0,
                   "cache_creation_input_tokens": 40})
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
            json!({"cache_read_input_tokens": 600, "cache_creation_input_tokens": 0,
                   "output_tokens": 5})
        );
        assert_eq!(
            out[1]["message"]["usage"],
            json!({"cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
                   "output_tokens": 5})
        );
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
        assert_eq!(
            Otlp::default().read(&export(&[ok, failed, typed]), &mut out),
            0
        );
        assert!(!out.concat().contains("SENTINEL"));
        let out: Vec<Value> = out
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out.len(), 6);
        // In end-time order: x2, x1, x3.
        let call = &out[0]["message"]["content"][0];
        assert_eq!(call["id"], "t/x2");
        assert_eq!(call["name"], "fetch");
        assert_eq!(call["input"], json!({}));
        assert_eq!(out[0]["message"]["model"], "<synthetic>");
        let result = &out[1]["message"]["content"][0];
        assert_eq!(result["tool_use_id"], "t/x2");
        assert_eq!(result["is_error"], true);
        // The trace and span ids, as a call id need not be unique across traces.
        assert_eq!(out[2]["message"]["content"][0]["id"], "t/x1");
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

    #[test]
    fn deprecated_token_names_are_read_when_the_current_ones_are_absent() {
        let span = inference(
            "d",
            "1",
            vec![
                attr("gen_ai.request.model", json!({"stringValue": "m"})),
                attr("gen_ai.usage.prompt_tokens", json!({"intValue": "70"})),
                attr("gen_ai.usage.completion_tokens", json!({"intValue": "7"})),
            ],
        );
        let (out, bad) = records(&export(&[span]));
        assert_eq!(bad, 0);
        assert_eq!(
            out[0]["message"]["usage"],
            json!({"input_tokens": 70, "cache_read_input_tokens": 0,
                   "cache_creation_input_tokens": 0, "output_tokens": 7})
        );
    }

    #[test]
    fn a_span_written_again_counts_once_and_calls_sharing_an_id_stay_apart() {
        let call = |id: &str| {
            json!({"traceId": format!("t-{id}"), "spanId": id, "endTimeUnixNano": "5",
            "attributes": [
                attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
                attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
                attr("gen_ai.tool.call.id", json!({"stringValue": "call_0"})),
            ]})
        };
        let line = export(&[call("p"), call("q")]);
        let mut file = Otlp::default();
        let mut out = Vec::new();
        assert_eq!(file.read(&line, &mut out), 0);
        assert_eq!(file.read(&line, &mut out), 0);
        let ids: Vec<Value> = out
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).expect("JSON"))
            .map(|v| v["message"]["content"][0].clone())
            .map(|b| {
                b.get("id")
                    .or_else(|| b.get("tool_use_id"))
                    .cloned()
                    .unwrap()
            })
            .collect();
        assert_eq!(ids, ["t-p/p", "t-p/p", "t-q/q", "t-q/q"]);
    }

    #[test]
    fn spans_sharing_a_span_id_across_traces_stay_apart() {
        let call = |trace: &str, code: u64| {
            json!({"traceId": trace, "spanId": "s", "endTimeUnixNano": "5",
            "status": {"code": code},
            "attributes": [
                attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
                attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
            ]})
        };
        let (out, bad) = records(&export(&[call("t1", 0), call("t2", 2)]));
        assert_eq!(bad, 0);
        let blocks: Vec<&Value> = out.iter().map(|v| &v["message"]["content"][0]).collect();
        assert_eq!(blocks[0]["id"], "t1/s");
        assert_eq!(blocks[1]["tool_use_id"], "t1/s");
        assert_eq!(blocks[1]["is_error"], false);
        assert_eq!(blocks[2]["id"], "t2/s");
        assert_eq!(blocks[3]["tool_use_id"], "t2/s");
        assert_eq!(blocks[3]["is_error"], true);
    }
}
