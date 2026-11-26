//! Measuring a request's messages and shape, and reading `api-request` records.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    BUILT_IN_TOOLS, File, MCP_TOOLS, OTHER_BLOCKS, PARTS, Parts, REMINDERS, Request,
    SYSTEM_MESSAGES, SYSTEM_PROMPT, Shape, TEXT, THINKING, TOOL_CALLS, TOOL_RESULTS,
};
use crate::detectors::startup::loaded_tools;
use crate::detectors::tools::printed;
use crate::detectors::{chars, json_chars};
use crate::time::parse_ts;

/// Reads one `api-request` record into `f`, or counts it as not rebuilt when its file
/// does not hold its base request or shape, or a field it needs is missing or malformed:
/// `base` absent, null or a string; `keep` a whole number; `tail` a list of hashes.
pub(super) fn add_request(f: &mut File, v: &Value, in_window: bool) {
    let str_field = |k: &str| v.get(k).and_then(Value::as_str);
    let shape = str_field("shapeHash")
        .and_then(|h| f.shapes.get(h))
        .copied();
    let base = match v.get("base") {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(b)) => f.request_ids.get(b).map(|&i| Some(i)),
        Some(_) => None,
    };
    let tail: Option<Vec<[u8; 32]>> = v
        .get("tail")
        .and_then(Value::as_array)
        .and_then(|t| t.iter().map(|h| h.as_str().and_then(hex32)).collect());
    let keep = v
        .get("keep")
        .and_then(Value::as_u64)
        .and_then(|k| usize::try_from(k).ok());
    let (Some(shape), Some(base), Some(tail), Some(keep)) = (shape, base, tail, keep) else {
        f.not_rebuilt += u64::from(in_window);
        return;
    };
    let kept = base.map_or(0, |b| keep.min(f.len(b)));
    let tail = tail.into_iter().map(|h| f.id(h)).collect();
    let model = v
        .pointer("/params/model")
        .and_then(Value::as_str)
        .map_or_else(|| "unknown".to_owned(), printed);
    let i = f.requests.len();
    f.requests.push(Request {
        time: str_field("timestamp").and_then(parse_ts),
        in_window,
        model,
        shape,
        base,
        kept,
        tail,
    });
    f.request_refs.push(str_field("id").map(str::to_owned));
    if let Some(id) = str_field("id") {
        f.request_ids.insert(id.to_owned(), i);
    }
}

/// The SHA-256 of a message as Claude Code hashes it: compact JSON with sorted keys, as
/// `serde_json` writes a `Value` whose objects keep their keys sorted.
/// The hash of message `{role, content}`: the SHA-256 of its compact JSON with sorted keys,
/// written out here so that the content is not copied.
pub(super) fn hash_of(role: &Value, content: &Value) -> [u8; 32] {
    Sha256::digest(format!("{{\"content\":{content},\"role\":{role}}}").as_bytes()).into()
}

/// 64 hexadecimal digits as 32 bytes, or `None` for anything else.
pub(super) fn hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let digit = |c: u8| {
        char::from(c)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    };
    let mut out = [0; 32];
    for (o, pair) in out.iter_mut().zip(b.chunks(2)) {
        *o = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

pub(super) fn message_parts(role: &Value, content: &Value) -> Parts {
    let mut p = [0; PARTS.len()];
    let role = role.as_str().unwrap_or("");
    let text;
    let blocks: &[Value] = match content {
        Value::String(s) => {
            text = [json!({"type": "text", "text": s})];
            &text
        }
        Value::Array(a) => a,
        _ => &[],
    };
    for b in blocks {
        let kind = b.get("type").and_then(Value::as_str);
        let text = |k: &str| b.get(k).and_then(Value::as_str);
        let (part, n) = match (kind, text("text")) {
            (Some("text"), Some(t)) => (
                match role {
                    "system" => SYSTEM_MESSAGES,
                    "user" if t.trim_start().starts_with("<system-reminder>") => REMINDERS,
                    _ => TEXT,
                },
                chars(t),
            ),
            _ if role == "system" => (SYSTEM_MESSAGES, json_chars(b)),
            (Some("tool_result"), _) => (TOOL_RESULTS, json_chars(b)),
            (Some("tool_use"), _) => (TOOL_CALLS, json_chars(b)),
            (Some("thinking"), _) => (THINKING, text("thinking").map_or(0, chars)),
            (Some("redacted_thinking"), _) => (THINKING, text("data").map_or(0, chars)),
            _ => (OTHER_BLOCKS, json_chars(b)),
        };
        p[part] += n;
    }
    p
}

pub(super) fn shape_parts(shape: &Value) -> Shape {
    let mut parts = [0; PARTS.len()];
    let mut servers = BTreeMap::new();
    for b in shape
        .get("system")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        parts[SYSTEM_PROMPT] += b.get("text").and_then(Value::as_str).map_or(0, chars);
    }
    let wrapped = json!({"shape": shape});
    for (name, n) in loaded_tools(&wrapped) {
        match name.strip_prefix("mcp__") {
            Some(rest) => {
                parts[MCP_TOOLS] += n;
                let server = rest.split("__").next().unwrap_or("");
                *servers.entry(server.to_owned()).or_default() += n;
            }
            None => parts[BUILT_IN_TOOLS] += n,
        }
    }
    Shape { parts, servers }
}
