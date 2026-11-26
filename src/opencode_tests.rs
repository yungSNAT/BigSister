use super::*;
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// A row's milliseconds become an RFC 3339 time with 3 digits and `Z`.
#[test]
fn times_print_milliseconds_in_utc() {
    assert_eq!(time(0), json!("1970-01-01T00:00:00.000Z"));
    assert_eq!(time(1_762_560_000_120), json!("2025-11-08T00:00:00.120Z"));
    assert_eq!(time(-1), json!("1969-12-31T23:59:59.999Z"));
    assert_eq!(time(253_402_128_000_000), json!("9999-12-30T00:00:00.000Z"));
    assert_eq!(time(i64::MAX), Value::Null);
    assert_eq!(time(i64::MIN), Value::Null);
}

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
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-opencode-{}", std::process::id()));
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

/// OpenCode writing to its database whilst `bigsister` queries it changes nothing the
/// queries see: they read the copy `open` made.
#[test]
fn a_write_after_opening_is_not_seen() {
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-opencode-w-{}", std::process::id()));
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
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-opencode-h-{}", std::process::id()));
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
    // A folder opens on Unix, but reading it fails: it holds not even an empty copy.
    assert!(!holds(&dir, &[]));
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
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-oc-{name}-{}", std::process::id()));
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

/// Another program, as OpenCode is to `bigsister`: the test binary running `other` in a
/// child process, which reports in lines starting `@`.
struct Other {
    child: Child,
    out: BufReader<ChildStdout>,
}

impl Other {
    fn start(db: &Path, mode: &str) -> Self {
        // nosemgrep: rust.lang.security.current-exe.current-exe -- the test reruns its own binary as a second process; no trust decision rests on the path
        let mut child = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "opencode::tests::other",
                "--ignored",
                "--nocapture",
            ])
            .env("BIGSISTER_TEST_DB", db)
            .env("BIGSISTER_TEST_MODE", mode)
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
    let vars = (
        std::env::var_os("BIGSISTER_TEST_DB"),
        std::env::var_os("BIGSISTER_TEST_MODE"),
    );
    // Run directly, without its variables, it has nothing to do.
    let (Some(db), Some(mode)) = vars else { return };
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
        other => {
            assert_eq!(other, "churn", "no mode {other}");
            say("churning");
            churn(&conn);
        }
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
    let since = Some(Timestamp::UNIX_EPOCH);
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
        // Messages are built every time, so no line runs only when a case fails.
        let names: Vec<_> = after.iter().map(|f| (&f.0, f.1)).collect();
        let changed = format!("{case}: the folder changed to {names:?}");
        wrong.extend((after != before).then_some(changed));
        let failed = format!("{case}: {:?}", result.as_ref().err());
        wrong.extend((result.is_ok() == (case == "journal")).then_some(failed));
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
        let missing = format!("{change}: {got:?}");
        wrong.extend((!got.contains(reason)).then_some(missing));
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
    let since = Some(Timestamp::UNIX_EPOCH);
    assert!(read(&db, since).is_err(), "a damaged database was read");
    std::fs::remove_dir_all(&dir).expect("clean up");
}
