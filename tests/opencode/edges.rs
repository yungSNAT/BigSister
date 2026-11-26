//! OpenCode databases that are unreadable, empty, malformed or oddly placed.

use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use rusqlite::{Connection, params};
use serde_json::{Value, json};

use crate::{Messages, T0, add_session, database, empty, finish, report, row, start, tool};
#[cfg(unix)]
use crate::{SINCE, run};

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

/// Once OpenCode has stopped, an empty write-ahead log and its index can remain beside
/// the database: the database is read and neither file is touched.
#[test]
fn an_empty_log_and_its_index_are_left_alone() {
    let (dir, writer) = database("opencode-empty-log");
    drop(writer);
    std::fs::write(dir.join("opencode.db-wal"), "").expect("log");
    std::fs::write(dir.join("opencode.db-shm"), [7u8; 32]).expect("index");
    let before = files(&dir);
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(2), &json!(0)],
        "{}",
        doc["roots"]
    );
    assert!(files(&dir) == before, "the database folder changed");
}

/// A message or part whose data is not JSON, a part whose message cannot be read and a
/// tool part with no state each count as a skipped line; a part of a type bigsister does
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
    drop(writer);
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
    drop(writer);
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

/// Whilst OpenCode runs, its newest records sit in the write-ahead log, which SQLite
/// cannot read without sharing the log's index: the database counts as one unreadable
/// file, the report gives the reason and what to do, and the database, the log and the
/// index are untouched, times included.
#[test]
fn a_log_holding_records_counts_as_unreadable_and_is_left_alone() {
    let (dir, writer) = database("opencode-live");
    // The sessions reach the database file; a later write stays in the log.
    writer
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); CREATE TABLE later (x INTEGER);")
        .expect("checkpoint and write");
    let before = files(&dir);
    let wal = before
        .iter()
        .find(|f| f.0.ends_with("-wal"))
        .expect("a log");
    assert!(!wal.1.is_empty(), "the log holds no record");
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(1), &json!(1)],
        "{}",
        doc["roots"]
    );
    let reasons = &doc["roots"][0]["unreadable_reasons"];
    assert_eq!(reasons[0]["count"], json!(1), "{reasons}");
    let reason = reasons[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("OpenCode's write-ahead log holds records")
            && reason.contains("quit OpenCode"),
        "{reason:?}"
    );
    let (text, _) = report(&dir, &[]);
    assert!(
        text.contains(&format!("\n  unreadable: {reason}\n")),
        "{text}"
    );
    assert!(files(&dir) == before, "the database folder changed");
}

/// A folder whose name holds `%` and `#` is read: the database read is the one in it.
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
    // Listable but not searchable: the folder opens, its files cannot be looked up.
    set(0o600).expect("deny");
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
    let doc: Value = serde_json::from_str(&out.1).unwrap_or_else(|e| panic!("{e}: {out:?}"));
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
/// the sessions already read count nowhere, and the reason names the row only as one.
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
    assert_eq!(
        doc["roots"][0]["unreadable_reasons"],
        json!([{"reason": "OpenCode's tables cannot be read as OpenCode writes them", "count": 1}])
    );
    assert_eq!(doc["detectors"]["polls"]["tables"][0]["rows"], json!([]));
}

/// The root's counts as JSON, with `--since` read in UTC.
#[cfg(unix)]
fn utc_roots(dir: &Path, since: &str) -> Value {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty-opencode");
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(["all", "--agent", "opencode", "--opencode-root"])
        .arg(dir)
        .args(["--since", since, "--json"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TZ", "UTC")
        .output()
        .expect("run bigsister");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    doc["roots"][0].clone()
}

/// The last date there is, at midnight in UTC, lies past every instant a session can be
/// updated at, so a session updated at the last of them is not read.
#[cfg(unix)]
#[test]
fn a_window_past_the_last_instant_reads_no_session() {
    let (dir, writer) = empty("opencode-past-last-instant");
    // 9999-12-30T22:00:00.999Z, the last millisecond a timestamp holds.
    writer
        .execute(
            "INSERT INTO session VALUES ('ses_z', 'p', NULL, 's', '/d', 't', ?1, ?1)",
            params![253_402_207_200_999_i64],
        )
        .expect("session");
    drop(writer);
    let root = utc_roots(&dir, "9999-12-31");
    assert_eq!(root["files"], json!(0), "{root}");
}

/// A window past the last instant still counts a database that cannot be read, as the
/// day before does.
#[cfg(unix)]
#[test]
fn a_window_past_the_last_instant_still_counts_an_unreadable_database() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("opencode-past-last-text");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    std::fs::write(dir.join("opencode.db"), "not a database").expect("text file");
    let day_before = utc_roots(&dir, "9999-12-30");
    assert_eq!(
        [&day_before["files"], &day_before["unreadable"]],
        [&json!(1), &json!(1)],
        "{day_before}"
    );
    assert_eq!(utc_roots(&dir, "9999-12-31"), day_before);
}

/// A folder without `opencode.db`, as before OpenCode first runs, has no sessions and
/// nothing unreadable.
#[test]
fn a_folder_without_a_database_reads_as_empty() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("opencode-no-db");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    let (stdout, _) = report(&dir, &["--json"]);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        [&doc["roots"][0]["files"], &doc["roots"][0]["unreadable"]],
        [&json!(0), &json!(0)],
        "{}",
        doc["roots"]
    );
}

/// A database without OpenCode's `session` or `message` table is one unreadable file,
/// with SQLite's reason.
#[test]
fn a_database_missing_a_table_names_it() {
    for (table, schema) in [
        ("session", "CREATE TABLE other (x integer);"),
        (
            "message",
            "CREATE TABLE session (id text PRIMARY KEY, parent_id text,
               time_created integer NOT NULL, time_updated integer NOT NULL);",
        ),
    ] {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("opencode-no-{table}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("folder");
        let conn = Connection::open(dir.join("opencode.db")).expect("database");
        conn.execute_batch(schema).expect("schema");
        drop(conn);
        let (stdout, _) = report(&dir, &["--json"]);
        let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
        let root = &doc["roots"][0];
        assert_eq!(
            (&root["files"], &root["unreadable_reasons"]),
            (
                &json!(1),
                &json!([{"reason": format!("no such table: {table}"), "count": 1}])
            ),
            "{root}"
        );
    }
}
