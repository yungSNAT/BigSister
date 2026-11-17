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

use chrono::{DateTime, Utc};
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

/// The tables `shaphan` reads. SQLite refuses a read of any other, among them those
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
pub fn read(db: &Path, since: DateTime<Utc>) -> Result<Vec<Session>, String> {
    let conn = open(db)?;
    sessions(&conn, since).map_err(|e| match e {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message,
        _ => "OpenCode's tables cannot be read as OpenCode writes them".to_owned(),
    })
}

fn sessions(conn: &Connection, since: DateTime<Utc>) -> rusqlite::Result<Vec<Session>> {
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
/// OpenCode included, opens, writes and checkpoints as it would were `shaphan` not
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
    DateTime::from_timestamp_millis(ms).map_or(Value::Null, |t| {
        Value::from(t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    })
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
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::process::{Child, ChildStdout, Command, Stdio};
    use std::time::{Duration, Instant};

    fn part(id: &str, data: &Value) -> Part {
        Part::new(id.to_owned(), 0, &data.to_string())
    }

    fn call(id: &str) -> Value {
        json!({"type": "tool", "callID": id, "tool": "bash",
               "state": {"status": "completed", "input": {"command": "ls"}, "output": "x"}})
    }

    /// A step-start before the previous step's step-finish closes that step: both
    /// steps' calls and results are kept.
    #[test]
    fn a_step_left_without_its_finish_keeps_its_calls() {
        let parts = [
            part("p0", &json!({"type": "step-start"})),
            part("p1", &call("a")),
            part("p2", &json!({"type": "step-start"})),
            part("p3", &call("b")),
            part(
                "p4",
                &json!({"type": "step-finish", "tokens": {"input": 5}}),
            ),
        ];
        let mut lines = Vec::new();
        message("m", 0, r#"{"role": "assistant"}"#, &parts, &mut lines);
        let text = lines.join("\n");
        for id in [r#""tool_use_id":"a""#, r#""tool_use_id":"b""#] {
            assert!(text.contains(id), "{id} missing:\n{text}");
        }
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains(r#""type":"assistant""#))
                .count(),
            2,
            "{text}"
        );
    }

    /// Only `TABLES` can be read through the connection `open` gives.
    #[test]
    fn a_table_other_than_sessions_messages_and_parts_is_refused() {
        let dir = std::env::temp_dir().join(format!("shaphan-opencode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("folder");
        let db = dir.join("opencode.db");
        Connection::open(&db)
            .expect("database")
            .execute_batch(
                "CREATE TABLE session (id text); CREATE TABLE message (id text);
                 CREATE TABLE part (id text); CREATE TABLE account (token text);
                 INSERT INTO account VALUES ('secret');",
            )
            .expect("tables");
        let conn = open(&db).expect("open");
        for table in TABLES {
            let rows = conn
                .prepare(&format!("SELECT id FROM {table}"))
                .and_then(|mut q| q.query([]).map(|_| ()));
            assert!(rows.is_ok(), "{table}: {rows:?}");
        }
        for refused in [
            "SELECT token FROM account",
            "SELECT * FROM sqlite_master",
            "SELECT count(*) FROM session",
        ] {
            assert!(conn.prepare(refused).is_err(), "{refused} was allowed");
        }
        drop(conn);
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// OpenCode writing to its database whilst `shaphan` queries it changes nothing the
    /// queries see: they read the copy `open` made.
    #[test]
    fn a_write_after_opening_is_not_seen() {
        let dir = std::env::temp_dir().join(format!("shaphan-opencode-w-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("folder");
        let db = dir.join("opencode.db");
        let writer = Connection::open(&db).expect("database");
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL; CREATE TABLE session (id text);
                 INSERT INTO session VALUES ('before');
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .expect("table");
        let conn = open(&db).expect("open");
        // The write reaches the database file itself.
        writer
            .execute_batch("UPDATE session SET id = 'after'; PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("write");
        let id: String = conn
            .query_row("SELECT id FROM session", [], |r| r.get(0))
            .expect("query");
        assert_eq!(id, "before");
        drop((conn, writer));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// A file that changed after it was copied, in a byte or in length, no longer holds
    /// the copy.
    #[test]
    fn a_file_changed_after_its_copy_does_not_hold_it() {
        let dir = std::env::temp_dir().join(format!("shaphan-opencode-h-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("folder");
        let file = dir.join("f");
        let copy = vec![7u8; 70_000];
        std::fs::write(&file, &copy).expect("file");
        assert!(holds(&file, &copy));
        let mut changed = copy.clone();
        changed[69_999] = 8;
        std::fs::write(&file, &changed).expect("file");
        assert!(!holds(&file, &copy));
        std::fs::write(&file, &copy[..69_999]).expect("file");
        assert!(!holds(&file, &copy));
        changed = copy.clone();
        changed.push(7);
        std::fs::write(&file, &changed).expect("file");
        assert!(!holds(&file, &copy));
        assert!(!holds(&dir.join("missing"), &copy));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// OpenCode's tables, an `account` table, and 20 sessions of generation 0, each
    /// spanning 2 pages.
    const SCHEMA: &str = "
        CREATE TABLE session (id TEXT, parent_id TEXT, time_created INTEGER,
                              time_updated INTEGER);
        CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER,
                           data TEXT);
        CREATE TABLE account (token TEXT);
        INSERT INTO account VALUES ('secret');
        WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20)
        INSERT INTO session SELECT '0-' || hex(randomblob(3000)), NULL, i, i FROM n;";

    /// A new folder `name` holding `opencode.db`, in WAL mode or not, with `SCHEMA`,
    /// closed, so that no file is beside it.
    fn database(name: &str, wal: bool) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("shaphan-oc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("folder");
        let db = dir.join("opencode.db");
        let mode = if wal { "WAL" } else { "DELETE" };
        Connection::open(&db)
            .expect("database")
            .execute_batch(&format!("PRAGMA journal_mode = {mode}; {SCHEMA}"))
            .expect("tables");
        (dir, db)
    }

    /// The file named as `db` with `suffix` added, such as its log.
    fn beside(db: &Path, suffix: &str) -> PathBuf {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    }

    /// The files in `dir`, by name, with their lengths, modification times and bytes. A
    /// file part of which another program has locked, as SQLite on Windows locks the
    /// log's index whilst a connection is open, has no bytes.
    fn files(dir: &Path) -> Vec<(PathBuf, u64, SystemTime, Option<Vec<u8>>)> {
        let mut out: Vec<_> = std::fs::read_dir(dir)
            .expect("folder")
            .map(|e| e.expect("entry").path())
            .map(|p| {
                let m = std::fs::metadata(&p).expect("metadata");
                let bytes = std::fs::read(&p).ok();
                (p, m.len(), m.modified().expect("time"), bytes)
            })
            .collect();
        out.sort();
        out
    }

    /// The generation of each session read through `conn`: its id up to the first `-`.
    fn generations(conn: &Connection) -> BTreeSet<String> {
        let mut query = conn.prepare("SELECT id FROM session").expect("query");
        query
            .query_map([], |r| r.get::<_, String>(0))
            .expect("rows")
            .map(|id| {
                let id = id.expect("id");
                id.split('-').next().unwrap_or_default().to_owned()
            })
            .collect()
    }

    /// What a write by another program gave: whether its connection is read-only, then
    /// the results of a write spanning many pages, a checkpoint and an integrity check.
    fn write(conn: &Connection) -> String {
        let readonly = conn.is_readonly(MAIN_DB);
        let wrote =
            conn.execute_batch("INSERT INTO session (id) VALUES ('w-' || hex(randomblob(10000)));");
        let busy = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            r.get::<_, i64>(0)
        });
        let check = conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0));
        format!("readonly={readonly:?} write={wrote:?} busy={busy:?} integrity={check:?}")
    }

    /// `write`'s report when the other program kept a read-write connection, wrote,
    /// checkpointed the whole log and found its database sound.
    const SOUND: &str = r#"readonly=Ok(false) write=Ok(()) busy=Ok(0) integrity=Ok("ok")"#;

    /// Another program, as OpenCode is to `shaphan`: the test binary running `other` in a
    /// child process, which reports in lines starting `@`.
    struct Other {
        child: Child,
        out: BufReader<ChildStdout>,
    }

    impl Other {
        fn start(db: &Path, mode: &str) -> Self {
            let mut child = Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "opencode::tests::other",
                    "--ignored",
                    "--nocapture",
                ])
                .env("SHAPHAN_TEST_DB", db)
                .env("SHAPHAN_TEST_MODE", mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("child");
            let out = BufReader::new(child.stdout.take().expect("output"));
            Self { child, out }
        }

        /// Its next report.
        fn report(&mut self) -> String {
            let mut line = String::new();
            loop {
                line.clear();
                let read = self.out.read_line(&mut line).expect("line");
                assert_ne!(read, 0, "the other program stopped without reporting");
                if let Some(report) = line.strip_prefix('@') {
                    return report.trim_end().to_owned();
                }
            }
        }

        /// Tells it to go on, and gives its next report.
        fn go(&mut self) -> String {
            let input = self.child.stdin.as_mut().expect("input");
            writeln!(input, "go").expect("go");
            self.report()
        }

        /// Waits for it to end, its connection closed.
        fn end(mut self) {
            drop(self.child.stdin.take());
            self.child.wait().expect("end");
        }
    }

    /// Runs another program in `mode` to its end, and gives its first report.
    fn run(db: &Path, mode: &str) -> String {
        let mut other = Other::start(db, mode);
        let report = other.report();
        other.end();
        report
    }

    /// The other program's side of `Other`, in the mode the test names.
    #[test]
    #[ignore = "run as a child process by other tests"]
    fn other() {
        let (Some(db), Some(mode)) = (
            std::env::var_os("SHAPHAN_TEST_DB"),
            std::env::var_os("SHAPHAN_TEST_MODE"),
        ) else {
            return;
        };
        let say = |report: &str| {
            println!("@{report}");
            std::io::stdout().flush().expect("flush");
        };
        let wait = || {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        };
        let conn = Connection::open(&db).expect("database");
        conn.busy_timeout(Duration::ZERO).expect("no waiting");
        let mode = mode.to_str().expect("mode");
        if let Some(sql) = cut_short(mode) {
            conn.execute_batch(sql).expect("cut short");
            say("left");
            std::process::exit(0);
        }
        match mode {
            // Holds a write open until told to go on, then commits it.
            "hold" => {
                conn.execute_batch(
                    "BEGIN IMMEDIATE;
                     INSERT INTO session (id) VALUES ('w-' || hex(randomblob(10000)));",
                )
                .expect("write");
                say("holding");
                wait();
                say(&format!("{:?}", conn.execute_batch("COMMIT")));
            }
            "write" => say(&write(&conn)),
            // Opens the database, then writes when told to go on.
            "open" => {
                let tables: rusqlite::Result<i64> =
                    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0));
                say(&format!("open {tables:?}"));
                wait();
                say(&write(&conn));
            }
            "churn" => {
                say("churning");
                churn(&conn);
            }
            unknown => panic!("no mode {unknown}"),
        }
    }

    /// What the other program runs, in a mode in which it then stops without closing
    /// the database, leaving the files beside it as they stand.
    fn cut_short(mode: &str) -> Option<&'static str> {
        match mode {
            // A read, leaving the log and its index, as OpenCode does when it is stopped.
            "leave" => Some("SELECT count(*) FROM session;"),
            // A committed change that stays in the log.
            "frames" => Some(
                "PRAGMA wal_autocheckpoint = 0;
                 UPDATE session SET id = '1-' || hex(randomblob(3000));",
            ),
            // A change to more pages than the cache holds, so that some reach the
            // database file, left uncommitted: a hot journal.
            "cut" => Some(
                "PRAGMA cache_size = 1; BEGIN;
                 UPDATE session SET id = '1-' || hex(randomblob(3000));",
            ),
            _ => None,
        }
    }

    /// Writes a new generation of every session, a transaction and a checkpoint each,
    /// for 2 seconds.
    fn churn(conn: &Connection) {
        let end = Instant::now() + Duration::from_secs(2);
        for k in 1.. {
            if Instant::now() > end {
                break;
            }
            let _ = conn.execute_batch(&format!(
                "UPDATE session SET id = '{k}-' || hex(randomblob(3000));
                 PRAGMA wal_checkpoint(TRUNCATE);"
            ));
        }
    }

    /// Reading the database creates, removes and changes no file beside it: with none
    /// beside it, with the log and its index left by a program that stopped, with the
    /// log alone, with a hot journal, which is refused, and whilst another program holds
    /// a write open.
    #[test]
    fn a_read_creates_or_changes_no_file_beside_the_database() {
        let since = DateTime::from_timestamp(0, 0).expect("1970");
        let mut wrong = Vec::new();
        for case in ["none", "both", "log", "journal", "live"] {
            let (dir, db) = database(&format!("a-{case}"), case != "journal");
            let mut live = None;
            match case {
                "both" | "log" => assert_eq!(run(&db, "leave"), "left"),
                "journal" => assert_eq!(run(&db, "cut"), "left"),
                "live" => {
                    let mut other = Other::start(&db, "hold");
                    assert_eq!(other.report(), "holding");
                    live = Some(other);
                }
                _ => {}
            }
            if case == "log" {
                std::fs::remove_file(beside(&db, "-shm")).expect("no index");
            }
            if case == "journal" {
                let journal = std::fs::metadata(beside(&db, "-journal")).expect("a journal");
                assert!(journal.len() > 0, "an empty journal");
            }
            let before = files(&dir);
            let result = read(&db, since);
            let after = files(&dir);
            if after != before {
                let names: Vec<_> = after.iter().map(|f| (&f.0, f.1)).collect();
                wrong.push(format!("{case}: the folder changed to {names:?}"));
            }
            if result.is_ok() == (case == "journal") {
                wrong.push(format!("{case}: {:?}", result.as_ref().err()));
            }
            if let Some(mut other) = live {
                assert_eq!(other.go(), "Ok(())");
                other.end();
            }
            std::fs::remove_dir_all(&dir).expect("clean up");
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// Another program opens, writes and checkpoints before, during and after a read,
    /// with a read-write connection, and its database stays sound: with no file beside
    /// the database, and with the log and its index left by a program that stopped.
    #[test]
    fn another_program_writes_before_during_and_after_a_read() {
        for left in [false, true] {
            let (dir, db) = database(&format!("b{}", u8::from(left)), true);
            let leave = || {
                if left {
                    assert_eq!(run(&db, "leave"), "left");
                }
            };
            leave();
            assert_eq!(run(&db, "write"), SOUND, "before");
            leave();
            let mut during = String::new();
            let read = open_with(&db, || during = run(&db, "write"));
            assert_eq!(during, SOUND, "during");
            assert!(read.is_err(), "a copy taken across another program's write");
            leave();
            let _ = open(&db);
            assert_eq!(run(&db, "write"), SOUND, "after");
            std::fs::remove_dir_all(&dir).expect("clean up");
        }
    }

    /// The sequence that corrupted another program's database whilst a lock was held on
    /// the log's index: a program stops leaving records in the log, the log is lost, and
    /// another program opens during a read and writes and checkpoints after it. Its
    /// database stays sound.
    #[test]
    fn a_program_opening_during_a_read_over_a_stale_index_keeps_its_database_sound() {
        let (dir, db) = database("stale", true);
        assert_eq!(run(&db, "frames"), "left");
        std::fs::remove_file(beside(&db, "-wal")).expect("lose the log");
        let mut opened = None;
        let _ = open_with(&db, || {
            let mut other = Other::start(&db, "open");
            let report = other.report();
            assert!(report.starts_with("open Ok"), "{report}");
            opened = Some(other);
        });
        let mut other = opened.expect("the other program");
        assert_eq!(other.go(), SOUND);
        other.end();
        let check: String = Connection::open(&db)
            .expect("database")
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("check");
        assert_eq!(check, "ok");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// Whilst another program writes a new generation of every session again and again,
    /// in WAL mode with a checkpoint after each and in rollback mode, each copy read
    /// holds one generation.
    #[test]
    fn under_a_writer_each_copy_read_is_one_committed_state() {
        for wal in [true, false] {
            let (dir, db) = database(&format!("c{}", u8::from(wal)), wal);
            let mut churn = Other::start(&db, "churn");
            assert_eq!(churn.report(), "churning");
            let (mut copies, mut refused) = (0, 0);
            let end = Instant::now() + Duration::from_millis(1500);
            while Instant::now() < end {
                if let Ok(conn) = open(&db) {
                    let seen = generations(&conn);
                    assert_eq!(seen.len(), 1, "WAL {wal}: generations {seen:?} in one copy");
                    copies += 1;
                } else {
                    refused += 1;
                }
            }
            churn.end();
            eprintln!("WAL {wal}: {copies} copies read, {refused} refused");
            std::fs::remove_dir_all(&dir).expect("clean up");
        }
    }

    /// A change between the 2 reads of the file that only one check sees refuses the
    /// copy with that check's reason: a byte changed in place with the file's time put
    /// back, which only the second read sees, and the time alone moved or an empty journal
    /// created, which only the second stamp sees.
    #[test]
    fn a_change_between_the_reads_refuses_the_copy() {
        let mut wrong = Vec::new();
        for (change, reason) in [
            ("byte", "the database changed whilst"),
            ("time", "time or a file beside it changed whilst"),
            ("journal", "time or a file beside it changed whilst"),
        ] {
            let (dir, db) = database(&format!("e-{change}"), false);
            let read = open_with(&db, || {
                let mut file = OpenOptions::new().write(true).open(&db).expect("file");
                let modified = file.metadata().and_then(|m| m.modified()).expect("time");
                match change {
                    "byte" => file
                        .seek(SeekFrom::End(-1))
                        .and_then(|_| file.write_all(b"x"))
                        .and_then(|()| file.set_modified(modified)),
                    "time" => file.set_modified(modified + Duration::from_secs(2)),
                    _ => std::fs::File::create(beside(&db, "-journal")).map(drop),
                }
                .expect("change");
            });
            let got = read.err().unwrap_or_default();
            if !got.contains(reason) {
                wrong.push(format!("{change}: {got:?}"));
            }
            std::fs::remove_dir_all(&dir).expect("clean up");
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
    }

    /// A database whose pages do not fit together is refused whole, even when the
    /// sessions themselves read: here a page of another table is damaged.
    #[test]
    fn a_database_failing_the_quick_check_is_refused() {
        let (dir, db) = database("d", false);
        let (root, size): (i64, i64) = {
            let conn = Connection::open(&db).expect("database");
            let root = conn
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'account'",
                    [],
                    |r| r.get(0),
                )
                .expect("root page");
            let size = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .expect("page size");
            (root, size)
        };
        let mut bytes = std::fs::read(&db).expect("file");
        // The page's type: no page of a table holds 0 there.
        bytes[usize::try_from((root - 1) * size).expect("offset")] = 0;
        std::fs::write(&db, bytes).expect("damage");
        let since = DateTime::from_timestamp(0, 0).expect("1970");
        assert!(read(&db, since).is_err(), "a damaged database was read");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
