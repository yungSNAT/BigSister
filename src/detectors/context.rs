//! What each Claude Code API request's input held, rebuilt from Claude Code's request log.
//!
//! Claude Code writes an `api-request` record for each request: the hash of its shape
//! (system prompt and tool definitions, an `api-request-shape` record), the request it
//! extends (`base`), how many of that request's messages it keeps (`keep`) and the hashes
//! of the messages it adds (`tail`). A message's hash is the SHA-256 of `{role, content}`
//! as compact JSON with sorted keys. A message the transcript does not hold as it was sent
//! is written as an `api-request-blob`; the rest are user records, the blocks of a reply,
//! or a run of consecutive user records sent as one message.

use std::collections::{BTreeMap, HashMap, HashSet};

use jiff::Timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::startup::{input_tokens, loaded_tools};
use super::tools::printed;
use super::{chars, json_chars};
use crate::report::{Opts, Section, Table, median, ratio, rfc3339};
use crate::scan::{Agent, Detector, Record, Session};
use crate::time::parse_ts;

/// The parts a request's input is split into, in report order.
const PARTS: [&str; 10] = [
    "system prompt",
    "built-in tools",
    "MCP tools",
    "system messages",
    "reminders",
    "tool results",
    "tool calls",
    "thinking",
    "text",
    "other blocks",
];
const SYSTEM_PROMPT: usize = 0;
const BUILT_IN_TOOLS: usize = 1;
const MCP_TOOLS: usize = 2;
const SYSTEM_MESSAGES: usize = 3;
const REMINDERS: usize = 4;
const TOOL_RESULTS: usize = 5;
const TOOL_CALLS: usize = 6;
const THINKING: usize = 7;
const TEXT: usize = 8;
const OTHER_BLOCKS: usize = 9;

const LARGEST_COLUMNS: [&str; 15] = [
    "time",
    "scope",
    "model",
    "input_tokens",
    "messages",
    "system_prompt",
    "built_in_tools",
    "mcp_tools",
    "system_messages",
    "reminders",
    "tool_results",
    "tool_calls",
    "thinking",
    "text",
    "other_blocks",
];

/// Characters by part.
type Parts = [u64; PARTS.len()];

/// Most consecutive user records read together as one message.
///
/// shortcut: Claude Code 2.1.325 was seen to send a run of user records with no reply
/// between them as one message, which no record or blob holds (its request logs, 19 November
/// 2026; runs of up to 7 records seen). Runs of up to 16 records are matched, each longer run
/// hashed again in full, so a run costs up to 16 times its records' size in hashing. The run
/// holds a copy of its records' blocks, at most 16 lines of up to 64 MiB each (1 GiB), and
/// hashing writes the merged message out once more. A longer run's message is not found.
/// Hashing a run as it grows would be the upgrade.
const MAX_RUN: usize = 16;

/// A shape's system prompt and loaded tools, with the MCP tools' characters by server.
struct Shape {
    parts: Parts,
    servers: BTreeMap<String, u64>,
}

struct Request {
    time: Option<Timestamp>,
    in_window: bool,
    model: String,
    shape: usize,
    base: Option<usize>,
    /// How many of the base request's messages come first: `keep`, at most all of them.
    kept: usize,
    tail: Vec<u32>,
}

/// One transcript's request log, read in full before its requests are measured, since a
/// request can name a message whose record comes after it.
#[derive(Default)]
struct File {
    /// Each message hash seen, as an index into `messages`.
    ids: HashMap<[u8; 32], u32>,
    /// The parts of each message, or `None` for one only a request names.
    messages: Vec<Option<Parts>>,
    shapes: HashMap<String, usize>,
    shape_list: Vec<Shape>,
    requests: Vec<Request>,
    request_ids: HashMap<String, usize>,
    /// Recorded input tokens of the first reply that names each request.
    tokens: HashMap<String, u64>,
    request_refs: Vec<Option<String>>,
    not_rebuilt: u64,
    /// The reply being read: its message id and its blocks so far.
    reply: Option<(String, Vec<Value>)>,
    /// The blocks of the user records since the last reply, and how many records.
    run: (Vec<Value>, usize),
}

impl File {
    fn id(&mut self, hash: [u8; 32]) -> u32 {
        let next = u32::try_from(self.messages.len()).unwrap_or(u32::MAX);
        *self.ids.entry(hash).or_insert_with(|| {
            self.messages.push(None);
            next
        })
    }

    /// Holds a message's parts under its hash, unless a message with that hash is held.
    fn add_message(&mut self, hash: [u8; 32], role: &Value, content: &Value) {
        let i = self.id(hash) as usize;
        if self.messages[i].is_none() {
            self.messages[i] = Some(message_parts(role, content));
        }
    }

    fn add_sent(&mut self, role: &Value, content: &Value) {
        self.add_message(hash_of(role, content), role, content);
    }

    /// Holds a user record's message as it can be sent: as written; with text content as
    /// one text block; and with the records before it since the last reply, as one message.
    fn add_user(&mut self, role: &Value, content: &Value) {
        self.add_sent(role, content);
        let blocks = match content {
            Value::String(s) => {
                let blocks = json!([{"type": "text", "text": s}]);
                self.add_sent(role, &blocks);
                blocks
            }
            Value::Array(_) => content.clone(),
            _ => Value::Null,
        };
        let (Value::Array(blocks), Some("user")) = (blocks, role.as_str()) else {
            self.run = (Vec::new(), 0);
            return;
        };
        if self.run.1 == MAX_RUN {
            self.run = (Vec::new(), 0);
        }
        self.run.0.extend(blocks);
        self.run.1 += 1;
        if self.run.1 > 1 {
            let merged = Value::Array(std::mem::take(&mut self.run.0));
            self.add_sent(role, &merged);
            if let Value::Array(blocks) = merged {
                self.run.0 = blocks;
            }
        }
    }

    /// Ends the reply being read: its blocks together are the message a later request sends.
    fn close_reply(&mut self) {
        if let Some((_, blocks)) = self.reply.take() {
            self.add_sent(&json!("assistant"), &Value::Array(blocks));
        }
    }

    /// Calls `f` with each message of request `i`, in order: its base request's first
    /// `kept` messages, then its tail.
    fn each_message(&self, i: usize, mut f: impl FnMut(u32)) {
        let mut slices = Vec::new();
        let mut at = Some((i, usize::MAX));
        while let Some((r, want)) = at {
            let req = &self.requests[r];
            let from_base = req.kept.min(want);
            let from_tail = want.saturating_sub(req.kept).min(req.tail.len());
            slices.push(&req.tail[..from_tail]);
            at = req.base.filter(|_| from_base > 0).map(|b| (b, from_base));
        }
        for slice in slices.into_iter().rev() {
            for &m in slice {
                f(m);
            }
        }
    }

    fn len(&self, i: usize) -> usize {
        let r = &self.requests[i];
        r.kept + r.tail.len()
    }
}

/// One rebuilt request in the window.
struct Measured {
    time: Option<Timestamp>,
    scope: &'static str,
    model: String,
    tokens: Option<u64>,
    messages: usize,
    parts: Parts,
    /// The recorded input tokens split across `parts` ([`apportion`]); 0s without tokens.
    est: Parts,
    servers: BTreeMap<String, u64>,
}

impl Measured {
    fn total(&self) -> u64 {
        self.parts.iter().sum()
    }

    /// The MCP tools' estimated tokens split across their servers, by printed name.
    fn server_tokens(&self) -> Vec<(String, u64, u64)> {
        let mut chars: BTreeMap<String, u64> = BTreeMap::new();
        for (server, n) in &self.servers {
            *chars.entry(printed(server)).or_default() += n;
        }
        let weights: Vec<u64> = chars.values().copied().collect();
        let est = apportion(self.est[MCP_TOOLS], &weights);
        chars
            .into_iter()
            .zip(est)
            .map(|((server, n), e)| (server, n, e))
            .collect()
    }
}

/// `total` split across `weights` by their shares, by largest remainder: each share is
/// rounded down, then each token left goes to the next largest remainder, the earlier on a
/// tie, so the shares add up to `total`. All 0 when every weight is.
fn apportion(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum: u128 = weights.iter().map(|&w| u128::from(w)).sum();
    if sum == 0 {
        return vec![0; weights.len()];
    }
    let t = u128::from(total);
    let mut out: Vec<u64> = weights
        .iter()
        .map(|&w| u64::try_from(t * u128::from(w) / sum).unwrap_or(u64::MAX))
        .collect();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(t * u128::from(weights[i]) % sum));
    let left = total.saturating_sub(out.iter().sum());
    for &i in order
        .iter()
        .take(usize::try_from(left).unwrap_or(usize::MAX))
    {
        out[i] += 1;
    }
    out
}

/// What each request's input held, by part.
#[derive(Default)]
pub struct Context {
    cur: File,
    measured: Vec<Measured>,
    not_rebuilt: u64,
    partly_rebuilt: u64,
    not_found: u64,
}

fn context_wants(_s: &Session, line: &str) -> bool {
    line.contains("\"type\":\"api-request")
        || line.contains("\"type\":\"assistant\"")
        || line.contains("\"type\":\"user\"")
}

impl Detector for Context {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        context_wants(s, line)
    }

    fn scans_whole_file(&self) -> bool {
        true
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Exact(context_wants))
    }

    fn reads_request_blobs(&self) -> bool {
        true
    }

    fn begin(&mut self, _s: &Session) {
        self.cur = File::default();
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        let v = r.value;
        let f = &mut self.cur;
        let str_field = |k: &str| v.get(k).and_then(Value::as_str);
        match r.kind() {
            "api-request-blob" => {
                if let (Some(hash), Some(message)) =
                    (str_field("hash").and_then(hex32), v.get("message"))
                {
                    let field = |k| message.get(k).unwrap_or(&Value::Null);
                    f.add_message(hash, field("role"), field("content"));
                }
            }
            "api-request-shape" => {
                if let (Some(hash), Some(shape)) = (str_field("shapeHash"), v.get("shape")) {
                    let i = f.shape_list.len();
                    f.shape_list.push(shape_parts(shape));
                    f.shapes.insert(hash.to_owned(), i);
                }
            }
            "user" => {
                if let Some(m) = v.get("message").filter(|m| m.is_object()) {
                    let field = |k| m.get(k).unwrap_or(&Value::Null);
                    f.add_user(field("role"), field("content"));
                }
            }
            "assistant" => {
                let Some(m) = v.get("message").filter(|m| m.is_object()) else {
                    return;
                };
                f.run = (Vec::new(), 0);
                let id = m.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                if f.reply.as_ref().is_none_or(|(cur, _)| *cur != id) {
                    f.close_reply();
                    f.reply = Some((id, Vec::new()));
                }
                if let (Some((_, blocks)), Some(content)) =
                    (f.reply.as_mut(), m.get("content").and_then(Value::as_array))
                {
                    blocks.extend(content.iter().cloned());
                }
                if let (Some(request), Some(tokens)) = (str_field("requestRef"), input_tokens(v)) {
                    f.tokens.entry(request.to_owned()).or_insert(tokens);
                }
            }
            "api-request" => add_request(f, v, r.in_window),
            _ => {}
        }
    }

    fn end(&mut self, s: &Session) {
        let mut f = std::mem::take(&mut self.cur);
        f.close_reply();
        self.not_rebuilt += f.not_rebuilt;
        let mut missing = HashSet::new();
        for i in 0..f.requests.len() {
            let req = &f.requests[i];
            if !req.in_window {
                continue;
            }
            let shape = &f.shape_list[req.shape];
            let mut parts = shape.parts;
            let mut found = true;
            f.each_message(i, |m| match f.messages[m as usize] {
                Some(p) => {
                    for (total, n) in parts.iter_mut().zip(p) {
                        *total += n;
                    }
                }
                None => {
                    found = false;
                    missing.insert(m);
                }
            });
            if !found {
                self.partly_rebuilt += 1;
                continue;
            }
            let tokens = f.request_refs[i]
                .as_ref()
                .and_then(|id| f.tokens.get(id))
                .copied();
            let mut est = [0; PARTS.len()];
            est.copy_from_slice(&apportion(tokens.unwrap_or(0), &parts));
            self.measured.push(Measured {
                time: req.time,
                scope: s.scope(),
                model: req.model.clone(),
                tokens,
                messages: f.len(i),
                parts,
                est,
                servers: shape.servers.clone(),
            });
        }
        self.not_found += missing.len() as u64;
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut tables = vec![self.by_part(), self.by_server(), self.largest()];
        for t in &mut tables {
            t.truncate(opts.limit);
        }
        Section {
            name: "context",
            title: "what each request's input held, rebuilt from Claude Code's request log (Claude)",
            summary: self.summary(),
            tables,
            notes: vec![
                "Requests come from the request log Claude Code writes in its transcripts (api-request, api-request-shape and api-request-blob records); a transcript without one adds none.".to_owned(),
                "Characters: text blocks count their text, thinking blocks their thinking text, other blocks and loaded tool schemas their compact JSON; reminders are user text blocks starting <system-reminder>.".to_owned(),
                "est input tokens split each request's recorded input tokens (uncached, cache writes and cache reads, from the first reply that names the request) across its parts by their share of its characters, by largest remainder so that they add up to its input tokens, and the MCP tools' across their servers; they are estimates, as tokens per character differ between parts.".to_owned(),
                "Medians by part are over every rebuilt request, a request without the part counting 0; by MCP server, over the requests that loaded its tools.".to_owned(),
                "A request not rebuilt names a base request or shape its transcript does not hold, or has a base, keep or tail field it cannot read. A request partly rebuilt lists a message not found: one that no request blob, user record, run of consecutive user records or reply in its transcript matches, counted once a transcript. Neither counts in any table.".to_owned(),
            ],
        }
    }
}

impl Context {
    fn summary(&self) -> Vec<(String, Value)> {
        let tokens: Vec<u64> = self.measured.iter().filter_map(|m| m.tokens).collect();
        vec![
            ("requests_rebuilt".to_owned(), json!(self.measured.len())),
            ("requests_not_rebuilt".to_owned(), json!(self.not_rebuilt)),
            (
                "requests_partly_rebuilt".to_owned(),
                json!(self.partly_rebuilt),
            ),
            ("requests_with_tokens".to_owned(), json!(tokens.len())),
            ("messages_not_found".to_owned(), json!(self.not_found)),
            (
                "median_input_tokens".to_owned(),
                json!((!tokens.is_empty()).then(|| median(&tokens))),
            ),
        ]
    }

    fn by_part(&self) -> Table {
        let all: u64 = self.measured.iter().map(Measured::total).sum();
        let mut t = Table::new(
            "By part",
            vec![
                "part",
                "requests",
                "median_chars",
                "max_chars",
                "share_of_chars",
                "est_input_tokens",
            ],
        );
        for (i, part) in PARTS.iter().enumerate() {
            let v: Vec<u64> = self.measured.iter().map(|m| m.parts[i]).collect();
            t.rows.push(vec![
                json!(part),
                json!(v.iter().filter(|&&n| n > 0).count()),
                json!(median(&v)),
                json!(v.iter().max().copied().unwrap_or(0)),
                ratio(v.iter().sum(), all),
                json!(self.measured.iter().map(|m| m.est[i]).sum::<u64>()),
            ]);
        }
        t
    }

    fn by_server(&self) -> Table {
        let mut servers: BTreeMap<String, (Vec<u64>, u64)> = BTreeMap::new();
        for m in &self.measured {
            for (server, n, est) in m.server_tokens() {
                let e = servers.entry(server).or_default();
                e.0.push(n);
                e.1 += est;
            }
        }
        let mut rows: Vec<(u64, Vec<Value>)> = servers
            .into_iter()
            .map(|(server, (v, est))| {
                (
                    v.iter().sum(),
                    vec![
                        json!(server),
                        json!(v.len()),
                        json!(median(&v)),
                        json!(v.iter().max()),
                        json!(est),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(total, _)| std::cmp::Reverse(*total));
        let mut t = Table::new(
            "By MCP server",
            vec![
                "server",
                "requests",
                "median_chars",
                "max_chars",
                "est_input_tokens",
            ],
        );
        t.rows = rows.into_iter().map(|(_, r)| r).collect();
        t
    }

    fn largest(&self) -> Table {
        let mut with_tokens: Vec<&Measured> = self
            .measured
            .iter()
            .filter(|m| m.tokens.is_some())
            .collect();
        with_tokens.sort_by_key(|m| (std::cmp::Reverse(m.tokens), m.time));
        let mut t = Table::new(
            "Largest requests by recorded input tokens, with each part's est input tokens",
            LARGEST_COLUMNS.to_vec(),
        );
        for m in with_tokens {
            let mut row = vec![
                json!(m.time.map(rfc3339)),
                json!(m.scope),
                json!(m.model),
                json!(m.tokens),
                json!(m.messages),
            ];
            row.extend(m.est.iter().map(|&n| json!(n)));
            t.rows.push(row);
        }
        t
    }
}

/// Reads one `api-request` record into `f`, or counts it as not rebuilt when its file
/// does not hold its base request or shape, or a field it needs is missing or malformed:
/// `base` absent, null or a string; `keep` a whole number; `tail` a list of hashes.
fn add_request(f: &mut File, v: &Value, in_window: bool) {
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
fn hash_of(role: &Value, content: &Value) -> [u8; 32] {
    Sha256::digest(format!("{{\"content\":{content},\"role\":{role}}}").as_bytes()).into()
}

/// 64 hexadecimal digits as 32 bytes, or `None` for anything else.
fn hex32(s: &str) -> Option<[u8; 32]> {
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

fn message_parts(role: &Value, content: &Value) -> Parts {
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

fn shape_parts(shape: &Value) -> Shape {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn value(s: &str) -> Value {
        serde_json::from_str(s).expect("valid JSON")
    }

    /// A file holding shape `s` and the requests `lines`, each in the window.
    fn file_of(lines: &[&str]) -> File {
        let mut f = File::default();
        f.shapes.insert("s".to_owned(), 0);
        f.shape_list
            .push(shape_parts(&value(r#"{"system":[],"tools":[]}"#)));
        for line in lines {
            add_request(&mut f, &value(line), true);
        }
        f
    }

    fn messages(f: &File, i: usize) -> Vec<u32> {
        let mut out = Vec::new();
        f.each_message(i, |m| out.push(m));
        out
    }

    /// Each request's estimated tokens add up to its recorded tokens; a tie in remainders
    /// gives the token to the earlier part.
    #[test]
    fn apportion_splits_by_largest_remainder() {
        assert_eq!(apportion(300, &[37, 153, 37]), [49, 202, 49]);
        assert_eq!(apportion(2, &[1, 1, 1]), [1, 1, 0]);
        assert_eq!(apportion(5, &[0, 0]), [0, 0]);
        assert_eq!(apportion(0, &[3, 4]), [0, 0]);
    }

    /// Claude Code sends text content as one text block, and consecutive user records as
    /// one message holding their blocks, up to `MAX_RUN` records; a reply ends a run.
    #[test]
    fn user_records_match_as_sent_alone_and_in_runs() {
        let role = json!("user");
        let text = |t: &str| json!({"type": "text", "text": t});
        let held = |f: &File, blocks: Vec<Value>| {
            f.ids.contains_key(&hash_of(&role, &Value::Array(blocks)))
        };
        let mut f = File::default();
        f.add_user(&role, &json!("a"));
        f.add_user(&role, &json!([text("b")]));
        assert!(held(&f, vec![text("a")]));
        assert!(held(&f, vec![text("a"), text("b")]));
        f.run = (Vec::new(), 0);
        f.add_user(&role, &json!([text("c")]));
        assert!(!held(&f, vec![text("a"), text("b"), text("c")]));
        for i in 1..=MAX_RUN {
            f.add_user(&role, &json!([text(&i.to_string())]));
        }
        let run: Vec<Value> = std::iter::once(text("c"))
            .chain((1..=MAX_RUN).map(|i| text(&i.to_string())))
            .collect();
        assert!(held(&f, run[..MAX_RUN].to_vec()));
        assert!(!held(&f, run), "a run longer than MAX_RUN is not matched");
    }

    /// Only a reply ends a run of user records: a user or assistant record without a
    /// message object, and a record of another kind, leave it whole.
    #[test]
    fn only_a_reply_with_a_message_ends_a_run() {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let user = |t: &str| {
            json!({"type": "user", "message": {"role": "user",
                                    "content": [{"type": "text", "text": t}]}})
        };
        let mut d = Context::default();
        d.begin(&s);
        for v in [
            user("a"),
            json!({"type": "user", "message": "a"}),
            json!({"type": "assistant", "message": null}),
            json!({"type": "api-request-other"}),
            user("b"),
        ] {
            d.record(
                &s,
                &Record {
                    value: &v,
                    in_window: true,
                },
            );
        }
        let blocks = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
        assert!(d.cur.ids.contains_key(&hash_of(&json!("user"), &blocks)));
    }

    /// A user record whose content is neither text nor blocks, or whose role is not
    /// `user`, ends a run: the records either side are not one message.
    #[test]
    fn a_record_of_another_shape_or_role_ends_a_run() {
        let user = json!("user");
        let text = |t: &str| json!({"type": "text", "text": t});
        let held = |f: &File, blocks: Vec<Value>| {
            f.ids.contains_key(&hash_of(&user, &Value::Array(blocks)))
        };
        for (role, content) in [(json!("user"), json!(7)), (json!("system"), json!("x"))] {
            let mut f = File::default();
            f.add_user(&user, &json!([text("a")]));
            f.add_user(&role, &content);
            f.add_user(&user, &json!([text("b")]));
            assert!(held(&f, vec![text("b")]), "{role} {content}");
            assert!(!held(&f, vec![text("a"), text("b")]), "{role} {content}");
        }
        assert_eq!(message_parts(&user, &json!(7)), [0; PARTS.len()]);
    }

    /// A request keeps its base's first `keep` messages, all of them when `keep` is
    /// larger, then adds its tail, through any number of generations.
    #[test]
    fn each_message_follows_bases_through_several_generations() {
        let h = |c: char| format!("\"{}\"", c.to_string().repeat(64));
        let (a, b, c, d, e, g) = (h('a'), h('b'), h('c'), h('d'), h('e'), h('f'));
        let f = file_of(&[
            &format!(r#"{{"id":"r1","keep":0,"tail":[{a},{b},{c}],"shapeHash":"s"}}"#),
            &format!(r#"{{"id":"r2","base":"r1","keep":2,"tail":[{d}],"shapeHash":"s"}}"#),
            &format!(r#"{{"id":"r3","base":"r2","keep":9,"tail":[{e}],"shapeHash":"s"}}"#),
            &format!(r#"{{"id":"r4","base":"r3","keep":0,"tail":[{g}],"shapeHash":"s"}}"#),
            r#"{"id":"r5","base":"r3","keep":1,"tail":[],"shapeHash":"s"}"#,
        ]);
        assert_eq!(f.not_rebuilt, 0);
        assert_eq!(messages(&f, 0), [0, 1, 2]);
        assert_eq!(messages(&f, 1), [0, 1, 3]);
        assert_eq!(messages(&f, 2), [0, 1, 3, 4]);
        assert_eq!(messages(&f, 3), [5]);
        assert_eq!(messages(&f, 4), [0]);
        assert_eq!((1..5).map(|i| f.len(i)).collect::<Vec<_>>(), [3, 4, 1, 1]);
    }

    /// A request whose `base`, `keep` or `tail` is missing where needed or of the wrong
    /// type is not rebuilt, rather than read as a request with fewer messages.
    #[test]
    fn a_request_with_a_malformed_field_is_not_rebuilt() {
        let f = file_of(&[
            r#"{"id":"r0","keep":0,"tail":[],"shapeHash":"s"}"#,
            r#"{"id":"r1","base":5,"keep":0,"tail":[],"shapeHash":"s"}"#,
            r#"{"id":"r2","base":"r0","keep":"0","tail":[],"shapeHash":"s"}"#,
            r#"{"id":"r3","base":"r0","keep":-1,"tail":[],"shapeHash":"s"}"#,
            r#"{"id":"r4","base":"r0","tail":[],"shapeHash":"s"}"#,
            r#"{"id":"r5","keep":0,"tail":"x","shapeHash":"s"}"#,
            r#"{"id":"r6","keep":0,"shapeHash":"s"}"#,
            r#"{"id":"r7","keep":0,"tail":[1],"shapeHash":"s"}"#,
            r#"{"id":"r8","base":null,"keep":0,"tail":[],"shapeHash":"s"}"#,
        ]);
        assert_eq!(f.not_rebuilt, 7);
        assert_eq!(f.requests.len(), 2, "r0, and r8 with a null base");
    }

    /// The digest Python's `hashlib.sha256(json.dumps(m, separators=(",", ":"),
    /// ensure_ascii=False, sort_keys=True).encode())` gives for the fixture's first prompt.
    #[test]
    fn a_message_hashes_as_sorted_compact_json() {
        let m: Value =
            serde_json::from_str(r#"{"role":"user","content":"Find the sentinel-prompt file."}"#)
                .expect("valid JSON");
        assert_eq!(
            hash_of(&m["role"], &m["content"]),
            hex32("f90cf44c749421a11b1900b7762b044b9477e3bb6bf90ad40849adf1a2884654").expect("hex")
        );
    }

    #[test]
    fn hex32_takes_only_64_hex_digits() {
        assert_eq!(hex32(&"0".repeat(63)), None);
        assert_eq!(hex32(&"g".repeat(64)), None);
        assert_eq!(hex32(&"aB".repeat(32)), Some([0xab; 32]));
    }

    #[test]
    fn each_block_counts_in_one_part() {
        let m: Value = serde_json::from_str(
            r#"{"role":"user","content":[
                {"type":"text","text":"  <system-reminder>r</system-reminder>"},
                {"type":"text","text":"hi"},
                {"type":"tool_result","tool_use_id":"t","content":"x"},
                {"type":"redacted_thinking","data":"abc"},
                {"type":"document","source":{}}]}"#,
        )
        .expect("valid JSON");
        let p = message_parts(&m["role"], &m["content"]);
        assert_eq!(p[REMINDERS], 38);
        assert_eq!(p[TEXT], 2);
        assert_eq!(p[TOOL_RESULTS], 54);
        assert_eq!(p[THINKING], 3);
        assert_eq!(p[OTHER_BLOCKS], 31);
        assert_eq!(p.iter().sum::<u64>(), 38 + 2 + 54 + 3 + 31);
    }
}
