//! OpenCode's sessions, read from its SQLite database and rewritten into the Claude Code
//! records the detectors read.
//!
//! OpenCode (<https://github.com/anomalyco/opencode>) keeps sessions in `opencode.db` in
//! its data folder: a `session` table, a `message` table and a `part` table whose `data`
//! columns hold JSON (`packages/schema/src/v1/session.ts`). The same database holds
//! account tokens and credentials, so only those 3 tables are queried, with fixed SQL,
//! and SQLite refuses a read of any other. The file is copied into memory with ordinary
//! reads and checked, and the copy is queried, so that no lock is taken and no file
//! beside it is opened, written or created.
//!
//! An assistant message holds one or more steps, each a model call: a `step-start` part,
//! its `tool` parts and a `step-finish` part with the call's token usage. Each step
//! becomes a Claude `assistant` record holding a `tool_use` block for each of its tool
//! calls, followed by a Claude `user` record for each call that finished, holding one
//! `tool_result`; a `compaction` part becomes a `compact_boundary`. Every message also
//! becomes a record holding only its time, which the detectors ignore, and other parts
//! become nothing. A message or part whose data is not a JSON object, or a tool part
//! without its state, counts as a skipped line.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use jiff::Timestamp;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, MAIN_DB, params};
use serde_json::{Map, Value, json};

/// One OpenCode session, as Claude Code records.
pub struct Session {
    /// Whether another session started it, as a subagent's is.
    pub subagent: bool,
    /// One record a line.
    pub lines: Vec<String>,
}

/// The tables `bigsister` reads. SQLite refuses a read of any other, among them those
/// holding OpenCode's account tokens and credentials.
const TABLES: [&str; 3] = ["session", "message", "part"];

/// The sessions in database `db` updated at or after `since`, oldest first.
///
/// # Errors
/// When the database cannot be read, changed whilst it was copied, has a write-ahead log
/// or a journal holding records, fails SQLite's quick check, or lacks the tables or
/// columns used. The reason, with what to do where the user can act, holds no table
/// content: an error other than SQLite's own message is named only as such, since a
/// conversion error can quote a row's value.
pub fn read(db: &Path, since: Option<Timestamp>) -> Result<Vec<Session>, String> {
    let conn = open(db)?;
    sessions(&conn, since).map_err(|e| match e {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message,
        _ => "OpenCode's tables cannot be read as OpenCode writes them".to_owned(),
    })
}

fn sessions(conn: &Connection, since: Option<Timestamp>) -> rusqlite::Result<Vec<Session>> {
    // With no `since`, only a session updated past the last instant a timestamp holds.
    let since = since.map_or(
        Timestamp::MAX.as_millisecond() + 1,
        Timestamp::as_millisecond,
    );
    let sessions: Vec<(String, bool)> = conn
        .prepare(
            "SELECT id, parent_id IS NOT NULL FROM session WHERE time_updated >= ?1 \
             ORDER BY time_created, id",
        )?
        .query_map(params![since], |r| Ok((r.get(0)?, r.get(1)?)))?
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

/// What the database file and the files beside it were before it was copied.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    /// The write-ahead log's length, if there is one.
    wal: Option<u64>,
    /// The rollback journal's length, if there is one.
    journal: Option<u64>,
    shm: bool,
}

impl Stamp {
    fn of(db: &Path) -> Option<Self> {
        let m = std::fs::metadata(db).ok()?;
        let beside = |suffix: &str| {
            let mut name = db.as_os_str().to_owned();
            name.push(suffix);
            std::fs::metadata(Path::new(&name))
        };
        Some(Self {
            len: m.len(),
            modified: m.modified().ok()?,
            wal: beside("-wal").ok().map(|w| w.len()),
            journal: beside("-journal").ok().map(|j| j.len()),
            shm: beside("-shm").is_ok(),
        })
    }
}

/// Copies database `db` into memory and opens the copy read-only, with reads of tables
/// other than `TABLES` refused.
///
/// No lock is taken and no file beside the database is opened, so another program,
/// OpenCode included, opens, writes and checkpoints as it would were `bigsister` not
/// running. SQLite's own read-only reader is not used, because SQLite 3.54.1 creates the
/// write-ahead log (`-wal`) when it is missing, and on Windows the log's index (`-shm`)
/// too (`sqlite3WalOpen` and `winHandleOpen` in `sqlite3.c`). Instead the file is read
/// twice with ordinary reads, sharing it fully, and the copy counts only if both reads
/// match, the file's size and time, the log's and the journal's lengths and whether the
/// index exists are unchanged, and SQLite's quick check passes on the copy. The queries
/// then read the copy, which nothing else changes. A log holding records, which only a
/// connection sharing its index can read, fails, so committed records still in the log
/// are not read. Once OpenCode has written since it started, its log holds records until
/// it is emptied, as on a clean stop, so the database is read only then; after OpenCode
/// is killed, only once it has next started and stopped cleanly. A journal holding a
/// write under way or cut short fails too. The second read and the second stamp each
/// refuse with their own reason.
///
/// shortcut: consistency is detected, not enforced. For OpenCode's database, which uses
/// the log, a copy holding pages from 2 states passes every check only if a checkpoint
/// rewrites pages in place, stalls across both reads and empties or removes the log
/// before the second stamp without moving the file's time: within the file clock's
/// resolution, or on Windows, which need not move the time whilst a writer holds the
/// file open. OpenCode asks only for `PASSIVE` checkpoints and sets no
/// `journal_size_limit` (its `packages/core/src/database/database.ts` at v1.19.6), and
/// SQLite's automatic checkpoints leave the log's length alone, so the one checkpoint
/// that does this is SQLite's last on a clean close, which then deletes the log. A writer with no journal on disk (`journal_mode` `OFF` or `MEMORY`) that
/// stalls across both reads passes too. The upgrade is SQLite's own read-only reader
/// once it can open without creating a file. The whole database is also held in memory,
/// twice whilst the copy is opened (OpenCode's on this machine is 1.1 MB, November
/// 2026); copying and checking it a page at a time would be the upgrade there.
fn open(db: &Path) -> Result<Connection, String> {
    open_with(db, || {})
}

/// `open`, running `during` between the 2 reads of the file, as a test's other program
/// does.
fn open_with(db: &Path, during: impl FnOnce()) -> Result<Connection, String> {
    let stamp = Stamp::of(db).ok_or("the database's size and time cannot be read")?;
    if stamp.wal.is_some_and(|len| len > 0) {
        return Err("OpenCode's write-ahead log holds records, as it does from OpenCode's first write until the log is emptied, as on a clean stop: quit OpenCode (if it was killed, start and quit it again) and run again".to_owned());
    }
    if stamp.journal.is_some_and(|len| len > 0) {
        return Err("the database's rollback journal holds records, so a write is under way or was cut short: quit OpenCode and run again (a journal kept after every write, as in PERSIST mode, always refuses the database)".to_owned());
    }
    let mut copy = std::fs::read(db).map_err(|e| e.to_string())?;
    during();
    if !holds(db, &copy) {
        return Err(
            "the database changed whilst it was copied: quit OpenCode and run again".to_owned(),
        );
    }
    if Stamp::of(db) != Some(stamp) {
        return Err("the database's time or a file beside it changed whilst it was copied: quit OpenCode and run again".to_owned());
    }
    // Bytes 18 and 19 of the header say whether the database uses a write-ahead log
    // (2) or not (1). A copy in memory cannot use one and this log is empty, so the
    // copy says not.
    if let Some(versions) = copy.get_mut(18..20) {
        versions.fill(1);
    }
    let mut conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    let len = copy.len();
    conn.deserialize_read_exact(MAIN_DB, copy.as_slice(), len, true)
        .map_err(|e| e.to_string())?;
    let check: String = conn
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if check != "ok" {
        return Err(
            "the copy fails SQLite's quick check, so the database may be damaged".to_owned(),
        );
    }
    conn.authorizer(Some(only_tables))
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Whether file `db` holds exactly `copy`, read again a piece at a time.
fn holds(db: &Path, copy: &[u8]) -> bool {
    let Ok(mut file) = std::fs::File::open(db) else {
        return false;
    };
    let mut piece = vec![0; 1 << 16];
    let mut at = 0;
    loop {
        match file.read(&mut piece) {
            Ok(0) => return at == copy.len(),
            Ok(n) => {
                if copy.get(at..at + n) != Some(&piece[..n]) {
                    return false;
                }
                at += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
}

/// Allows a query to read only `TABLES`.
fn only_tables(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read { table_name, .. } if TABLES.contains(&table_name) => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

/// A part of a message: its id, time and parsed `data`, which is null when it is not a
/// JSON object or is a tool call without its state.
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
            data: serde_json::from_str::<Value>(data)
                .ok()
                .filter(|d| d.is_object() && (d["type"] != "tool" || d["state"].is_object()))
                .unwrap_or(Value::Null),
        }
    }

    fn kind(&self) -> &str {
        self.data.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// An RFC 3339 time from milliseconds since 1970, or null for an impossible one.
fn time(ms: i64) -> Value {
    Timestamp::from_millisecond(ms).map_or(Value::Null, |t| Value::from(format!("{t:.3}")))
}

/// The line standing for a message or part that cannot be read: it does not end in `}`,
/// so the scan counts it as a skipped line, and it holds no row content.
const SKIPPED: &str = "(OpenCode row that cannot be read)";

/// Appends the records of message `id`, created at `at`, with `data` and `parts`.
fn message(id: &str, at: i64, data: &str, parts: &[Part], lines: &mut Vec<String>) {
    let data = serde_json::from_str::<Value>(data).unwrap_or(Value::Null);
    if !data.is_object() {
        // Its role is unknown, so its parts cannot be read either.
        lines.extend(std::iter::repeat_n(SKIPPED.to_owned(), 1 + parts.len()));
        return;
    }
    lines.push(json!({"type": "opencode", "timestamp": time(at)}).to_string());
    let skipped = parts.iter().filter(|p| !p.data.is_object()).count();
    lines.extend(std::iter::repeat_n(SKIPPED.to_owned(), skipped));
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
                // A step OpenCode left without its step-finish has no usage.
                if let Some(first) = start.or(step.first().copied()) {
                    self::step(model, first, None, &step, lines);
                }
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
/// `Read` reads `file_path`, and `edit` and `write` change files. Every other input
/// field is kept, so that calls differing only in them stay different calls.
fn tool_use(p: &Part) -> Value {
    let input = p
        .data
        .pointer("/state/input")
        .cloned()
        .unwrap_or(Value::Null);
    let path = || {
        let mut fields = input.as_object().cloned().unwrap_or_default();
        if let Some(file) = fields.remove("filePath") {
            fields.insert("file_path".to_owned(), file);
        }
        Value::Object(fields)
    };
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
/// error marked as one. A call still pending or running has none. Each file the call
/// attached adds an `image` block, for an `image/` type, or a `document` block, which
/// keep neither its address nor its content.
fn tool_result(p: &Part) -> Option<Value> {
    let state = p.data.get("state")?;
    let (content, error) = match state.get("status").and_then(Value::as_str)? {
        "completed" => (state.get("output"), false),
        "error" => (state.get("error"), true),
        _ => return None,
    };
    let mut content = content.cloned().unwrap_or(Value::Null);
    if let Some(files) = state.get("attachments").and_then(Value::as_array)
        && !files.is_empty()
    {
        let text = json!({"type": "text", "text": content});
        content = std::iter::once(text)
            .chain(files.iter().map(|f| {
                let image = f
                    .get("mime")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.starts_with("image/"));
                json!({"type": if image { "image" } else { "document" }})
            }))
            .collect();
    }
    Some(json!({"type": "tool_result",
                "tool_use_id": p.data.get("callID").cloned().unwrap_or(Value::Null),
                "content": content, "is_error": error}))
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

#[cfg(test)]
#[path = "opencode_tests.rs"]
mod tests;
