//! OpenCode's sessions, read from its SQLite database and rewritten into the Claude Code
//! records the detectors read.
//!
//! OpenCode (<https://github.com/anomalyco/opencode>) keeps sessions in `opencode.db` in
//! its data folder: a `session` table, a `message` table and a `part` table whose `data`
//! columns hold JSON (`packages/schema/src/v1/session.ts`). The same database holds
//! account tokens and credentials, so only those 3 tables are queried, with fixed SQL,
//! and the file is opened read-only.
//!
//! An assistant message holds one or more steps, each a model call: a `step-start` part,
//! its `tool` parts and a `step-finish` part with the call's token usage. Each step
//! becomes a Claude `assistant` record holding a `tool_use` block for each of its tool
//! calls, followed by a Claude `user` record for each call that finished, holding one
//! `tool_result`; a `compaction` part becomes a `compact_boundary`. Every other message
//! and part becomes a record holding only its time, which the detectors ignore.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Map, Value, json};

/// One OpenCode session, as Claude Code records.
pub struct Session {
    /// Whether another session started it, as a subagent's is.
    pub subagent: bool,
    /// One record a line.
    pub lines: Vec<String>,
}

/// The sessions in database `db` updated at or after `since`, oldest first.
///
/// # Errors
/// When the database cannot be opened or read, or lacks the tables or columns used.
pub fn read(db: &Path, since: DateTime<Utc>) -> rusqlite::Result<Vec<Session>> {
    let conn = open(db)?;
    let sessions: Vec<(String, bool)> = conn
        .prepare(
            "SELECT id, parent_id IS NOT NULL FROM session WHERE time_updated >= ?1 \
             ORDER BY time_created, id",
        )?
        .query_map(params![since.timestamp_millis()], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut messages = conn.prepare(
        "SELECT id, time_created, data FROM message WHERE session_id = ?1 \
         ORDER BY time_created, id",
    )?;
    let mut parts = conn.prepare(
        "SELECT id, message_id, time_created, data FROM part WHERE session_id = ?1 \
         ORDER BY time_created, id",
    )?;
    let mut out = Vec::with_capacity(sessions.len());
    for (id, subagent) in sessions {
        let mut by_message: HashMap<String, Vec<Part>> = HashMap::new();
        for p in parts.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(1)?,
                Part::new(r.get(0)?, r.get(2)?, &r.get::<_, String>(3)?),
            ))
        })? {
            let (message, part) = p?;
            by_message.entry(message).or_default().push(part);
        }
        let mut lines = Vec::new();
        for m in messages.query_map(params![id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (mid, at, data) = m?;
            let parts = by_message.remove(&mid).unwrap_or_default();
            message(&mid, at, &data, &parts, &mut lines);
        }
        out.push(Session { subagent, lines });
    }
    Ok(out)
}

/// Opens database `db` read-only, creating no file beside it.
///
/// Whilst OpenCode runs, its write-ahead log (`-wal`) and the log's index (`-shm`) sit
/// beside the database; a read-only connection reads the log and shares the index, as
/// any reader does, changing neither the database nor the log. With no log beside it,
/// SQLite would create both, so the file is opened as immutable instead: nothing is
/// pending in a log that does not exist. A log without
/// its index fails, rather than creating the index or reading the database without the
/// log's content.
fn open(db: &Path) -> rusqlite::Result<Connection> {
    let beside = |suffix: &str| {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        Path::new(&name).exists()
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match (beside("-wal"), beside("-shm")) {
        (true, true) => Connection::open_with_flags(db, flags),
        (true, false) => Err(rusqlite::Error::InvalidPath(db.to_path_buf())),
        (false, _) => {
            Connection::open_with_flags(immutable(db)?, flags | OpenFlags::SQLITE_OPEN_URI)
        }
    }
}

/// An SQLite URI naming `db` with `immutable=1`: its absolute path with forward slashes,
/// and `%`, `?` and `#` escaped, as SQLite's URI rules ask.
fn immutable(db: &Path) -> rusqlite::Result<String> {
    let absolute =
        std::path::absolute(db).map_err(|_| rusqlite::Error::InvalidPath(db.to_path_buf()))?;
    let path = absolute
        .to_str()
        .ok_or_else(|| rusqlite::Error::InvalidPath(db.to_path_buf()))?
        .replace('\\', "/")
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let slash = if path.starts_with('/') { "" } else { "/" };
    Ok(format!("file://{slash}{path}?immutable=1"))
}

/// A part of a message: its id, time and parsed `data`.
struct Part {
    id: String,
    at: i64,
    data: Value,
}

impl Part {
    fn new(id: String, at: i64, data: &str) -> Self {
        Self {
            id,
            at,
            data: serde_json::from_str(data).unwrap_or(Value::Null),
        }
    }

    fn kind(&self) -> &str {
        self.data.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// An RFC 3339 time from milliseconds since 1970, or null for an impossible one.
fn time(ms: i64) -> Value {
    DateTime::from_timestamp_millis(ms).map_or(Value::Null, |t| {
        Value::from(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    })
}

/// Appends the records of message `id`, created at `at`, with `data` and `parts`.
fn message(id: &str, at: i64, data: &str, parts: &[Part], lines: &mut Vec<String>) {
    let data = serde_json::from_str::<Value>(data).unwrap_or(Value::Null);
    lines.push(json!({"type": "opencode", "timestamp": time(at)}).to_string());
    if data.get("role").and_then(Value::as_str) != Some("assistant") {
        for p in parts.iter().filter(|p| p.kind() == "compaction") {
            lines.push(
                json!({"type": "system", "subtype": "compact_boundary",
                       "uuid": format!("{id}/{}", p.id), "timestamp": time(p.at)})
                .to_string(),
            );
        }
        return;
    }
    let model = data.get("modelID").and_then(Value::as_str).unwrap_or("");
    let mut step: Vec<&Part> = Vec::new();
    let mut start: Option<&Part> = None;
    for p in parts {
        match p.kind() {
            "step-start" => {
                step.clear();
                start = Some(p);
            }
            "tool" => step.push(p),
            "step-finish" => {
                let first = start.unwrap_or(p);
                self::step(model, first, Some(p), &step, lines);
                step.clear();
                start = None;
            }
            _ => {}
        }
    }
    // A step still running when OpenCode stopped writing has no usage.
    if let Some(first) = start.or(step.first().copied()) {
        self::step(model, first, None, &step, lines);
    }
}

/// Appends a step's `assistant` record and its results. `first` names and times the
/// step; `finish`, when written, gives its usage.
fn step(
    model: &str,
    first: &Part,
    finish: Option<&Part>,
    tools: &[&Part],
    lines: &mut Vec<String>,
) {
    let blocks: Vec<Value> = tools.iter().map(|p| tool_use(p)).collect();
    let usage = usage(finish.and_then(|f| f.data.get("tokens")));
    // Written as text: `model` must come first in the message, where the scan looks for
    // a reply's model, and a JSON object would sort its keys.
    lines.push(format!(
        "{{\"type\":\"assistant\",\"uuid\":{},\"message\":{{\"model\":{},\"role\":\"assistant\",\"content\":{},\"usage\":{}}},\"timestamp\":{}}}",
        Value::from(first.id.as_str()),
        Value::from(model),
        Value::from(blocks),
        usage,
        time(first.at),
    ));
    let done = finish.map_or(first.at, |f| f.at);
    for p in tools {
        if let Some(block) = tool_result(p) {
            lines.push(
                json!({"type": "user", "uuid": format!("{}/result", p.id),
                       "message": {"role": "user", "content": [block]}, "timestamp": time(done)})
                .to_string(),
            );
        }
    }
}

/// A tool part's call as a Claude `tool_use` block, with OpenCode's built-in tools named
/// as Claude Code's: `bash` runs commands as `Bash` does, `read` reads `filePath` as
/// `Read` reads `file_path`, and `edit` and `write` change files.
fn tool_use(p: &Part) -> Value {
    let input = p
        .data
        .pointer("/state/input")
        .cloned()
        .unwrap_or(Value::Null);
    let path = || json!({"file_path": input.get("filePath").cloned().unwrap_or(Value::Null)});
    let (name, input) = match p.data.get("tool").and_then(Value::as_str).unwrap_or("") {
        "bash" => ("Bash", input.clone()),
        "read" => ("Read", path()),
        "edit" => ("Edit", path()),
        "write" => ("Write", path()),
        other => (other, input.clone()),
    };
    json!({"type": "tool_use", "id": p.data.get("callID").cloned().unwrap_or(Value::Null),
           "name": name, "input": input})
}

/// A finished tool part's result as a Claude `tool_result` block: its output, or its
/// error marked as one. A call still pending or running has none.
fn tool_result(p: &Part) -> Option<Value> {
    let state = p.data.get("state")?;
    let (content, error) = match state.get("status").and_then(Value::as_str)? {
        "completed" => (state.get("output"), false),
        "error" => (state.get("error"), true),
        _ => return None,
    };
    Some(json!({"type": "tool_result",
                "tool_use_id": p.data.get("callID").cloned().unwrap_or(Value::Null),
                "content": content.cloned().unwrap_or(Value::Null), "is_error": error}))
}

/// A step's token usage under Claude's names, as the scan reads a prompt's size from them.
fn usage(tokens: Option<&Value>) -> Value {
    let mut out = Map::new();
    for (oc, claude) in [
        ("/input", "input_tokens"),
        ("/cache/write", "cache_creation_input_tokens"),
        ("/cache/read", "cache_read_input_tokens"),
        ("/output", "output_tokens"),
    ] {
        if let Some(n) = tokens.and_then(|t| t.pointer(oc)) {
            out.insert(claude.to_owned(), n.clone());
        }
    }
    Value::Object(out)
}
