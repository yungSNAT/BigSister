//! Runs `shaphan` over OpenCode databases the tests build, in OpenCode's layout.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, params};
use serde_json::{Value, json};

const SINCE: &str = "2026-11-01T00:00:00Z";
/// 9 November 2026 at 05:00 UTC, in milliseconds.
const T0: i64 = 1_794_200_400_000;

/// Runs `shaphan` with an empty home folder, so that no folder of the machine running
/// the tests is read.
fn run(args: &[&str]) -> (i32, String, String) {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty-opencode");
    std::fs::create_dir_all(&home).expect("home folder");
    let out = Command::new(env!("CARGO_BIN_EXE_shaphan"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("run shaphan");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// OpenCode's tables as its database holds them (OpenCode 1.18), reduced to the columns
/// shaphan reads and those holding secrets, which carry sentinels.
const SCHEMA: &str = "
CREATE TABLE session (id text PRIMARY KEY, project_id text NOT NULL, parent_id text,
  slug text NOT NULL, directory text NOT NULL, title text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL);
CREATE TABLE message (id text PRIMARY KEY, session_id text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
CREATE TABLE part (id text PRIMARY KEY, message_id text NOT NULL, session_id text NOT NULL,
  time_created integer NOT NULL, time_updated integer NOT NULL, data text NOT NULL);
CREATE TABLE account (id text PRIMARY KEY, email text NOT NULL, url text NOT NULL,
  access_token text NOT NULL, refresh_token text NOT NULL);
CREATE TABLE credential (id text PRIMARY KEY, label text NOT NULL, value text NOT NULL);
CREATE TABLE session_share (session_id text PRIMARY KEY, secret text NOT NULL, url text NOT NULL);
CREATE TABLE control_account (email text NOT NULL, url text NOT NULL, access_token text NOT NULL);
INSERT INTO control_account VALUES ('sentinel-control@example.com', 'https://sentinel.example',
  'sentinel-control-token');
INSERT INTO account VALUES ('a', 'sentinel-email@example.com', 'https://sentinel.example',
  'sentinel-access-token', 'sentinel-refresh-token');
INSERT INTO credential VALUES ('c', 'sentinel-label', 'sentinel-credential');
INSERT INTO session_share VALUES ('s1', 'sentinel-share-secret', 'https://sentinel.example/s');
";

/// A session's messages: each a role and its parts, in order.
type Messages = Vec<(&'static str, Vec<Value>)>;

fn add_session(conn: &Connection, id: &str, parent: Option<&str>, messages: &Messages) {
    conn.execute(
        "INSERT INTO session VALUES (?1, 'p', ?2, 'sentinel-slug', '/sentinel/dir', \
         'sentinel title', ?3, ?3)",
        params![id, parent, T0],
    )
    .expect("session");
    let mut t = T0;
    for (m, (role, parts)) in messages.iter().enumerate() {
        t += 1000;
        let data = if *role == "assistant" {
            json!({"role": "assistant", "modelID": "claude-opus-5-5", "providerID": "anthropic",
                   "tokens": {"input": 1, "output": 1, "reasoning": 0, "cache": {"read": 0, "write": 0}}})
        } else {
            json!({"role": "user", "agent": "build", "model": {"providerID": "anthropic",
                   "modelID": "claude-opus-5-5"}})
        };
        let mid = format!("{id}-m{m:02}");
        conn.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
            params![mid, id, t, data.to_string()],
        )
        .expect("message");
        for (p, part) in parts.iter().enumerate() {
            t += 10;
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![format!("{mid}-p{p:02}"), mid, id, t, part.to_string()],
            )
            .expect("part");
        }
    }
}

fn tool(call: &str, name: &str, input: Value, output: Option<(&str, bool)>) -> Value {
    let state = match output {
        Some((text, false)) => json!({"status": "completed", "input": input, "output": text,
            "title": "sentinel title", "metadata": {"output": "sentinel-metadata"},
            "time": {"start": T0, "end": T0}}),
        Some((text, true)) => json!({"status": "error", "input": input, "error": text,
            "time": {"start": T0, "end": T0}}),
        None => json!({"status": "pending", "input": input, "raw": "sentinel-raw"}),
    };
    json!({"type": "tool", "callID": call, "tool": name, "state": state})
}

fn start() -> Value {
    json!({"type": "step-start"})
}

fn finish() -> Value {
    json!({"type": "step-finish", "reason": "tool-calls", "cost": 0,
           "tokens": {"input": 1000, "output": 10, "reasoning": 0, "cache": {"read": 0, "write": 0}}})
}

/// The main session: `git status` twice (a poll of 40,000 characters), a read of
/// `a.rs`, a compaction, the read again (a re-read), `false` 3 times (a cascade, retry
/// churn and 2 polls after failure) and a web fetch still pending.
fn main_session() -> Messages {
    let status = || json!({"command": "git status", "description": "sentinel-description"});
    let out = format!("sentinel-output {}", "o".repeat(39_984));
    let fail = || json!({"command": "false"});
    let read = || json!({"filePath": "/w/a.rs"});
    let text = json!({"type": "text", "text": "sentinel-message-text"});
    vec![
        ("user", vec![text]),
        (
            "assistant",
            vec![
                start(),
                tool("c1", "bash", status(), Some((&out, false))),
                finish(),
                start(),
                tool("c2", "bash", status(), Some((&out, false))),
                tool("c3", "read", read(), Some((&"r".repeat(100), false))),
                finish(),
            ],
        ),
        ("user", vec![json!({"type": "compaction", "auto": true})]),
        (
            "assistant",
            vec![
                start(),
                tool("c4", "read", read(), Some((&"r".repeat(100), false))),
                tool("c5", "bash", fail(), Some(("boom", true))),
                finish(),
                start(),
                tool("c6", "bash", fail(), Some(("boom", true))),
                finish(),
                start(),
                tool("c7", "bash", fail(), Some(("boom", true))),
                finish(),
                start(),
                tool(
                    "c8",
                    "webfetch",
                    json!({"url": "https://sentinel.example"}),
                    None,
                ),
                json!({"type": "reasoning", "text": "sentinel-reasoning"}),
            ],
        ),
    ]
}

/// The main session, then a subagent's session running `ls`.
fn sessions(conn: &Connection) {
    add_session(conn, "ses_a", None, &main_session());
    let sub: Messages = vec![(
        "assistant",
        vec![
            start(),
            tool("d1", "bash", json!({"command": "ls"}), Some(("x", false))),
            finish(),
        ],
    )];
    add_session(conn, "ses_b", Some("ses_a"), &sub);
}

/// A folder holding an `opencode.db` with the sessions, and the open connection that
/// wrote it in WAL mode, as OpenCode keeps its database whilst it runs.
fn database(name: &str) -> (PathBuf, Connection) {
    let (dir, conn) = empty(name);
    sessions(&conn);
    (dir, conn)
}

/// A folder holding an `opencode.db` with OpenCode's tables and no session, and the open
/// connection that wrote it in WAL mode.
fn empty(name: &str) -> (PathBuf, Connection) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    let conn = Connection::open(dir.join("opencode.db")).expect("database");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("WAL");
    conn.execute_batch(SCHEMA).expect("schema");
    (dir, conn)
}

fn report(dir: &Path, extra: &[&str]) -> (String, String) {
    let mut args = vec![
        "all",
        "--agent",
        "opencode",
        "--opencode-root",
        dir.to_str().expect("UTF-8 path"),
        "--since",
        SINCE,
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "{stderr}");
    (stdout, stderr)
}

fn row<'a>(doc: &'a Value, section: &str, key: &[(&str, &str)]) -> &'a Value {
    doc["detectors"][section]["tables"][0]["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| key.iter().all(|(k, v)| r[*k] == json!(v)))
        })
        .unwrap_or_else(|| panic!("no {section} row {key:?} in {doc}"))
}

/// The JSON report over a database built in folder `name`, read whilst the connection
/// that wrote it is still open, as OpenCode's is whilst it runs.
fn counts(name: &str) -> Value {
    let (dir, _writer) = database(name);
    let (stdout, _) = report(&dir, &["--json"]);
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn opencode_sessions_feed_polls() {
    let doc = counts("opencode-polls");
    assert_eq!(doc["roots"][0]["files"], json!(2), "{}", doc["roots"]);
    assert_eq!(doc["roots"][0]["unreadable"], json!(0), "{}", doc["roots"]);
    let polls = row(&doc, "polls", &[("agent", "opencode"), ("scope", "main")]);
    assert_eq!(
        [
            &polls["commands"],
            &polls["polls"],
            &polls["polls_after_failure"]
        ],
        [&json!(5), &json!(3), &json!(2)],
        "{polls}"
    );
    // The poll's 40,000 characters, read by the next step's Claude Opus 5.5 reply at
    // US$4 per million: 10,000 tokens est.
    assert_eq!(
        [&polls["poll_usd_est"], &polls["unpriced_tokens_est"]],
        [&json!(0.04), &json!(0)]
    );
    let sub = row(
        &doc,
        "polls",
        &[("agent", "opencode"), ("scope", "subagent")],
    );
    assert_eq!(sub["commands"], json!(1));
}

#[test]
fn opencode_sessions_feed_rereads_and_tools() {
    let doc = counts("opencode-rereads-tools");
    let rereads = row(&doc, "rereads", &[("agent", "opencode"), ("scope", "main")]);
    assert_eq!(
        [
            &rereads["compactions"],
            &rereads["reads"],
            &rereads["rereads"],
            &rereads["reread_chars"]
        ],
        [&json!(1), &json!(2), &json!(1), &json!(100)],
        "{rereads}"
    );
    let summary = &doc["detectors"]["tools"]["summary"];
    assert_eq!(
        [
            &summary["cascades"],
            &summary["calls_in_cascades"],
            &summary["retry_churn_calls"]
        ],
        [&json!(1), &json!(3), &json!(1)],
        "{summary}"
    );
    let bash = row(&doc, "tools", &[("agent", "opencode"), ("tool", "Bash")]);
    assert_eq!(
        [&bash["calls"], &bash["errors"]],
        [&json!(6), &json!(3)],
        "{bash}"
    );
    let fetch = row(&doc, "tools", &[("tool", "webfetch")]);
    assert_eq!(
        [&fetch["calls"], &fetch["result_chars"]],
        [&json!(1), &json!(0)]
    );
}

/// The files in `dir`, by name, with their bytes and modification times.
fn files(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .expect("folder")
        .map(|e| e.expect("entry").path())
        .map(|p| {
            let modified = std::fs::metadata(&p).and_then(|m| m.modified());
            (
                p.display().to_string(),
                std::fs::read(&p).expect("read"),
                modified.expect("time"),
            )
        })
        .collect();
    out.sort();
    out
}

/// With OpenCode stopped, no write-ahead log sits beside the database, and reading it
/// must create none.
#[test]
fn no_opencode_text_or_secret_reaches_the_output_and_nothing_is_written() {
    let (dir, writer) = database("opencode-private");
    drop(writer);
    let before = files(&dir);
    for extra in [&[][..], &["--json"], &["--csv"], &["--paths"]] {
        let (stdout, stderr) = report(&dir, extra);
        assert!(
            !stdout.to_lowercase().contains("sentinel"),
            "leaked with {extra:?}:\n{stdout}"
        );
        assert!(!stderr.to_lowercase().contains("sentinel"), "{stderr}");
    }
    assert!(files(&dir) == before, "the database folder changed");
}

#[test]
fn an_unreadable_database_counts_as_one_unreadable_file() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("opencode-bad");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    std::fs::write(dir.join("opencode.db"), "not a database").expect("file");
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(1), &json!(1)],
        "{}",
        doc["roots"]
    );
}

/// A write-ahead log whose index is missing could hold sessions the database lacks, so
/// the database is not read without it, and no index is created.
#[test]
fn a_log_without_its_index_counts_as_unreadable_and_is_left_alone() {
    let (dir, writer) = database("opencode-no-shm");
    drop(writer);
    std::fs::write(dir.join("opencode.db-wal"), "").expect("log");
    let before = files(&dir);
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["unreadable"], json!(1), "{}", doc["roots"]);
    assert!(files(&dir) == before, "the database folder changed");
}

/// A message or part whose data is not JSON, a part whose message cannot be read and a
/// tool part with no state each count as a skipped line; a part of a type shaphan does
/// not know is passed over; none of their text prints.
#[test]
fn malformed_rows_count_as_skipped_lines_and_print_nothing() {
    let (dir, writer) = database("opencode-malformed");
    writer
        .execute_batch(&format!(
            "INSERT INTO session VALUES ('ses_c', 'p', NULL, 's', '/d', 't', {T0}, {T0});
             INSERT INTO message VALUES ('ses_c-m0', 'ses_c', {T0}, {T0}, 'sentinel-message {{');
             INSERT INTO part VALUES ('ses_c-m0-p0', 'ses_c-m0', 'ses_c', {T0}, {T0}, '{{}}');
             INSERT INTO message VALUES ('ses_c-m1', 'ses_c', {T1}, {T1},
               '{{\"role\":\"assistant\",\"modelID\":\"claude-opus-5-5\"}}');
             INSERT INTO part VALUES ('ses_c-m1-p0', 'ses_c-m1', 'ses_c', {T1}, {T1},
               '{{\"type\":\"step-start\"}}');
             INSERT INTO part VALUES ('ses_c-m1-p1', 'ses_c-m1', 'ses_c', {T1}, {T1},
               'sentinel-part [');
             INSERT INTO part VALUES ('ses_c-m1-p2', 'ses_c-m1', 'ses_c', {T1}, {T1},
               '{{\"type\":\"sentinel-future\",\"text\":\"sentinel-future-text\"}}');
             INSERT INTO part VALUES ('ses_c-m1-p3', 'ses_c-m1', 'ses_c', {T1}, {T1},
               '{{\"type\":\"tool\",\"tool\":\"bash\"}}');
             INSERT INTO part VALUES ('ses_c-m1-p4', 'ses_c-m1', 'ses_c', {T1}, {T1},
               '{{\"type\":\"step-finish\"}}');",
            T1 = T0 + 1
        ))
        .expect("rows");
    let (stdout, stderr) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [
            &doc["roots"][0]["files"],
            &doc["roots"][0]["unreadable"],
            &doc["roots"][0]["lines_skipped"]
        ],
        [&json!(3), &json!(0), &json!(4)],
        "{}",
        doc["roots"]
    );
    let polls = row(&doc, "polls", &[("agent", "opencode"), ("scope", "main")]);
    assert_eq!(polls["commands"], json!(5), "{polls}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    assert!(!stderr.to_lowercase().contains("sentinel"), "{stderr}");
}

/// A database that cannot be read whole gives no partial report: a missing `part` table
/// counts it as one unreadable file and none of its sessions count.
#[test]
fn a_database_missing_a_table_counts_as_unreadable_with_no_sessions() {
    let (dir, writer) = database("opencode-no-part");
    writer.execute_batch("DROP TABLE part").expect("drop");
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(1), &json!(1)],
        "{}",
        doc["roots"]
    );
    assert_eq!(
        doc["detectors"]["polls"]["tables"][0]["rows"],
        json!([]),
        "{}",
        doc["detectors"]["polls"]
    );
}

/// Whilst OpenCode runs, the database, its log and the log's index are read and none of
/// them is written, not even the index's time.
#[test]
fn a_live_database_is_read_without_changing_its_log_or_index() {
    let (dir, _writer) = database("opencode-live");
    let before = files(&dir);
    assert_eq!(
        before.len(),
        3,
        "{:?}",
        before.iter().map(|f| &f.0).collect::<Vec<_>>()
    );
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], json!(2), "{}", doc["roots"]);
    assert!(files(&dir) == before, "the database folder changed");
}

/// `%` and `#` in the folder's name are escaped in the URI SQLite opens, so the database
/// read is the one in that folder.
#[test]
fn a_folder_name_with_uri_characters_is_read() {
    let (dir, writer) = database("opencode-a%25#b");
    drop(writer);
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(2), &json!(0)],
        "{}",
        doc["roots"]
    );
}

/// On Unix, `\` is a character of a folder's name: the database in `a\b` is read, not
/// the one in `a/b`, which holds no session.
#[cfg(unix)]
#[test]
fn a_backslash_in_a_unix_folder_name_is_kept() {
    let (decoy, decoy_writer) = empty("opencode-bs/x");
    drop(decoy_writer);
    let (dir, writer) = database("opencode-bs\\x");
    drop(writer);
    assert!(decoy.join("opencode.db").exists());
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], json!(2), "{}", doc["roots"]);
}

/// A database whose presence cannot be checked counts as one unreadable file, not as
/// no database.
#[cfg(unix)]
#[test]
fn a_database_that_cannot_be_checked_counts_as_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, writer) = database("opencode-denied");
    drop(writer);
    let set = |mode| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode));
    set(0o000).expect("deny");
    let denied = std::fs::metadata(dir.join("opencode.db")).is_err();
    let root = dir.to_str().expect("UTF-8 path");
    let out = run(&[
        "all",
        "--agent",
        "opencode",
        "--opencode-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    set(0o755).expect("allow");
    if !denied {
        // Running as root, which no permission denies.
        return;
    }
    let doc: Value = serde_json::from_str(&out.1).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(1), &json!(1)],
        "{}",
        doc["roots"]
    );
}

/// A report over one main session with `messages` in a database of its own.
fn one_session(name: &str, messages: &Messages) -> (String, Value) {
    let (dir, writer) = empty(name);
    add_session(&writer, "ses_x", None, messages);
    drop(writer);
    let (stdout, _) = report(&dir, &["--json"]);
    let doc = serde_json::from_str(&stdout).expect("valid JSON");
    (stdout, doc)
}

/// Reads of one file at 3 offsets, each failing, are 3 different calls: 3 errors and
/// no retry churn.
#[test]
fn reads_differing_only_in_offset_are_different_calls() {
    let read = |call: &str, offset: u64| {
        tool(
            call,
            "read",
            json!({"filePath": "/w/b.rs", "offset": offset}),
            Some(("no", true)),
        )
    };
    let messages: Messages = vec![(
        "assistant",
        vec![
            start(),
            read("r1", 1),
            finish(),
            start(),
            read("r2", 2),
            finish(),
            start(),
            read("r3", 3),
            finish(),
        ],
    )];
    let (_, doc) = one_session("opencode-offsets", &messages);
    let reads = row(&doc, "tools", &[("agent", "opencode"), ("tool", "Read")]);
    assert_eq!(
        [&reads["calls"], &reads["errors"]],
        [&json!(3), &json!(3)],
        "{reads}"
    );
    let summary = &doc["detectors"]["tools"]["summary"];
    assert_eq!(summary["retry_churn_calls"], json!(0), "{summary}");
}

/// Files a call attached count as images and documents; their addresses never print.
#[test]
fn attached_files_count_as_images_and_documents() {
    let mut read = tool(
        "a1",
        "read",
        json!({"filePath": "/w/c.png"}),
        Some(("ok", false)),
    );
    read["state"]["attachments"] = json!([
        {"type": "file", "mime": "image/png", "url": "data:image/png;base64,sentinel"},
        {"type": "file", "mime": "application/pdf", "url": "https://sentinel.example/d.pdf"}
    ]);
    let messages: Messages = vec![("assistant", vec![start(), read, finish()])];
    let (stdout, doc) = one_session("opencode-attachments", &messages);
    let reads = row(&doc, "tools", &[("agent", "opencode"), ("tool", "Read")]);
    assert_eq!(
        [
            &reads["images"],
            &reads["documents"],
            &reads["result_chars"]
        ],
        [&json!(1), &json!(1), &json!(2)],
        "{reads}"
    );
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
}

/// A row that cannot be read after a valid session makes the whole database unreadable:
/// the sessions already read count nowhere.
#[test]
fn a_late_unreadable_row_leaves_no_partial_report() {
    let (dir, writer) = database("opencode-late-blob");
    writer
        .execute(
            "INSERT INTO session VALUES ('ses_z', 'p', NULL, 's', '/d', 't', ?1, ?1)",
            params![T0 + 10],
        )
        .expect("session");
    writer
        .execute(
            "INSERT INTO message VALUES ('ses_z-m0', 'ses_z', ?1, ?1, ?2)",
            params![T0 + 10, vec![0u8, 159, 146, 150]],
        )
        .expect("message");
    drop(writer);
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(1), &json!(1)],
        "{}",
        doc["roots"]
    );
    assert_eq!(doc["detectors"]["polls"]["tables"][0]["rows"], json!([]));
}
