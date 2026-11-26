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
//!   guessed, so `models` counts the reply as skipped. A span naming a
//!   `gen_ai.operation.name` other than `chat`, `generate_content`, `text_completion` or
//!   `realtime_inference` (such as `embeddings` or `invoke_agent`) is not a reply, as the
//!   conventions sum usage only over those operations.
//! - A tool span has `gen_ai.operation.name` `execute_tool` and a `gen_ai.tool.name`. It
//!   becomes a synthetic `assistant` record, which `models` passes over, holding one
//!   `tool_use` block with an empty input under the span's id (a tool call id need not be
//!   unique across traces), and a `user` record holding its `tool_result`, with no
//!   content, which is an error when the
//!   span's status code is 2 (error) or it has an `error.type`. Tool inputs and results
//!   are never read.
//!
//! Each record is written at its span's end time, the spans of a line in end-time order.
//! Other spans are ignored, as is a span whose trace and span ids already produced records
//! in this file, as when an exporter writes a batch again. A line that is not a JSON object
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

/// Explicit export keys for a mixed-source report. Different spans reusing a key
/// make it ambiguous. Repeating the same trace/span pair leaves it unique.
#[derive(Default)]
pub struct ExportKeys {
    models: Keys,
    tools: Keys,
}
type Key = (String, String, String);
#[derive(Default)]
struct Keys {
    by_native: std::collections::HashMap<Key, Option<String>>,
    by_span: std::collections::HashMap<String, Option<Key>>,
}

fn agree<T: PartialEq + Clone>(
    entry: std::collections::hash_map::Entry<'_, impl Eq + std::hash::Hash, Option<T>>,
    value: T,
) {
    let observed = entry.or_insert_with(|| Some(value.clone()));
    if observed.as_ref() != Some(&value) {
        *observed = None;
    }
}

impl Keys {
    fn observe(&mut self, session: &str, name: &str, id: &str, span: &str) {
        let key = (session.to_owned(), name.to_owned(), id.to_owned());
        if span.starts_with('/') {
            self.by_native.insert(key, None);
        } else {
            agree(self.by_span.entry(span.to_owned()), key.clone());
            agree(self.by_native.entry(key), span.to_owned());
        }
    }
}

impl crate::scan::Detector for ExportKeys {
    fn agents(&self) -> &'static [crate::scan::Agent] {
        &[crate::scan::Agent::Otlp]
    }
    fn wants(&self, _: &crate::scan::Session, line: &str) -> bool {
        line.contains("otelNativeId")
    }
    fn record(&mut self, _: &crate::scan::Session, r: &crate::scan::Record<'_>) {
        if !r.in_window {
            return;
        }
        let (Some(native), Some(session)) = (
            r.value["otelNativeId"].as_str(),
            r.value["sessionId"].as_str(),
        ) else {
            return;
        };
        if let Some((model, id)) = crate::detectors::models::countable_identity(r.value) {
            self.models.observe(session, model, native, id);
        }
        for call in crate::detectors::tool_uses(r) {
            if let (Some(name), Some(id)) = (call["name"].as_str(), call["id"].as_str()) {
                self.tools.observe(session, name, native, id);
            }
        }
    }
    fn section(&self, _: &crate::report::Opts) -> crate::report::Section {
        crate::report::Section::default()
    }
}

/// Keeps counted spans and unique, complete native counterparts for one section.
#[derive(Default)]
pub(crate) struct Overlap {
    spans: HashSet<String>,
    native: std::collections::HashMap<Key, bool>,
    exports: Option<std::rc::Rc<ExportKeys>>,
}

impl Overlap {
    pub(crate) fn enable(&mut self, exports: std::rc::Rc<ExportKeys>) {
        self.exports = Some(exports);
    }

    pub(crate) fn note_native(&mut self, session: &str, name: &str, id: &str, tool: bool) {
        let Some(exports) = &self.exports else { return };
        let keys = if tool {
            &exports.tools
        } else {
            &exports.models
        };
        let key = (session.into(), name.into(), id.into());
        if keys.by_native.get(&key).is_some_and(Option::is_some) {
            self.native
                .entry(key)
                .and_modify(|unique| *unique = false)
                .or_insert(true);
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.exports.is_some()
    }

    pub(crate) fn skip_export(&mut self, span: &str, tool: bool) -> bool {
        if span.starts_with('/') {
            return false;
        }
        if !self.spans.insert(span.to_owned()) {
            return true;
        }
        let Some(exports) = &self.exports else {
            return false;
        };
        let keys = if tool {
            &exports.tools
        } else {
            &exports.models
        };
        let Some(Some(key)) = keys.by_span.get(span) else {
            return false;
        };
        self.native.get(key) == Some(&true)
            && keys.by_native.get(key).and_then(Option::as_deref) == Some(span)
    }
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
        let key = span["traceId"]
            .as_str()
            .zip(span["spanId"].as_str())
            .map(|(trace, id)| (trace.to_owned(), id.to_owned()));
        if key.as_ref().is_some_and(|id| seen.contains(id)) {
            continue;
        }
        match span_records(span) {
            Ok(Some(found)) => {
                if let Some(id) = key {
                    seen.insert(id);
                }
                spans.push(found);
            }
            Ok(None) => {}
            Err(()) => bad += 1,
        }
    }
    spans.sort_by_key(|(end, _)| *end);
    out.extend(spans.into_iter().flat_map(|(_, r)| r));
    bad
}

/// The operations whose usage the conventions sum as inference; agent and embeddings
/// spans also carry usage, an agent's the total of its inference spans'.
const INFERENCE: [&str; 4] = [
    "chat",
    "generate_content",
    "text_completion",
    "realtime_inference",
];

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
    let inference = a
        .str("gen_ai.operation.name")
        .is_none_or(|op| INFERENCE.contains(&op));
    if !tool && !(inference && model.is_some() && usage) {
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
    fn native_id(&self, key: &str) -> Value {
        if self.str("gen_ai.provider.name") == Some("anthropic")
            && self
                .str("gen_ai.conversation.id")
                .is_some_and(|s| !s.is_empty())
        {
            Value::from(self.str(key).filter(|s| !s.is_empty()))
        } else {
            Value::Null
        }
    }

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
        "{{\"type\":\"assistant\",\"sessionId\":{},\"otelNativeId\":{},\"message\":{{\"model\":{},\"id\":{},\"role\":\"assistant\",\"content\":[],\"usage\":{}}},\"timestamp\":{ts}}}",
        Value::from(session),
        a.native_id("gen_ai.response.id"),
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
               "otelNativeId": a.native_id("gen_ai.tool.call.id"),
               "message": {"model": "<synthetic>", "role": "assistant", "content": [call_block]}, "timestamp": ts})
        .to_string(),
        json!({"type": "user", "sessionId": session, "uuid": format!("{id}-result"),
               "message": {"role": "user", "content": [result]}, "timestamp": ts})
        .to_string(),
    ]
}

#[cfg(test)]
mod tests;
