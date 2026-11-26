//! Runs `bigsister` over OpenCode databases the tests build, in OpenCode's layout.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, params};
use serde_json::{Value, json};

const SINCE: &str = "2026-11-01T00:00:00Z";
/// 9 November 2026 at 05:00 UTC, in milliseconds.
const T0: i64 = 1_794_200_400_000;

/// Runs `bigsister` with an empty home folder, so that no folder of the machine running
/// the tests is read.
fn run(args: &[&str]) -> (i32, String, String) {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty-opencode");
    std::fs::create_dir_all(&home).expect("home folder");
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .output()
        .expect("run bigsister");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// OpenCode's tables as its database holds them (OpenCode 1.18), reduced to the columns
/// bigsister reads and those holding secrets, which carry sentinels.
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

/// The JSON report over a database built in folder `name`, read once the connection
/// that wrote it has closed, as OpenCode's has once it stops.
fn counts(name: &str) -> Value {
    let (dir, writer) = database(name);
    drop(writer);
    let (stdout, _) = report(&dir, &["--json"]);
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn opencode_sessions_feed_polls() {
    let doc = counts("opencode-polls");
    assert_eq!(doc["roots"][0]["files"], json!(2), "{}", doc["roots"]);
    assert_eq!(doc["roots"][0]["unreadable"], json!(0), "{}", doc["roots"]);
    assert_eq!(doc["roots"][0]["unreadable_reasons"], json!([]));
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

mod opencode {
    mod edges;
}
