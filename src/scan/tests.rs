use super::feed::*;
use super::files::*;
use super::prep::*;
use super::*;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use serde_json::Value;

use crate::gemini::Gemini;
use crate::prices::Prices;
use crate::report::{Opts, Section};
use crate::time::parse_ts;

/// Counts the records it is given, and reports the count.
#[derive(Default)]
struct Count(u64);

impl Detector for Count {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, _s: &Session, _r: &Record<'_>) {
        self.0 += 1;
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: vec![("records".to_owned(), self.0.into())],
            ..Section::default()
        }
    }
}

/// The records `Count`, the first of `dets`, reports.
fn counted(dets: &[Box<dyn Detector>]) -> Value {
    let opts = Opts {
        limit: 0,
        paths: false,
        by_branch: false,
        by_day: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    dets[0].section(&opts).summary.swap_remove(0).1
}

#[test]
fn unselected_agents_are_absent_from_disk_and_sample_reports() {
    let prices = Prices::bundled();
    let since = Some(Timestamp::MIN);
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
    let root = PathBuf::from("unselected-gemini-root");
    let roots = [(Agent::Gemini, root)];
    assert!(scan(&roots, since, &prices, &mut dets).is_empty());

    let files = [Memory {
        agent: Agent::Gemini,
        path: "session.jsonl",
        text: "{}",
    }];
    assert!(scan_memory(&files, "sample", since, &prices, &mut dets).is_empty());
    assert_eq!(counted(&dets), 0);
}

#[test]
fn antigravity_lists_only_conversation_logs_in_the_window() {
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-agy-{}", std::process::id()));
    let logs = dir.join("b").join(".system_generated").join("logs");
    fs::create_dir_all(dir.join("a")).expect("a conversation with no log");
    fs::create_dir_all(&logs).expect("a conversation with a log");
    fs::write(logs.join("transcript_full.jsonl"), b"{}\n").expect("write log");
    let mut st = RootStats::new(Agent::Antigravity, dir.clone());
    let later = SystemTime::now() + Duration::from_secs(3600);
    assert!(antigravity_logs(&dir, later, &mut st).is_empty());
    let found = antigravity_logs(&dir, SystemTime::UNIX_EPOCH, &mut st);
    assert_eq!(found, [logs.join("transcript_full.jsonl")]);
    assert!(antigravity_logs(&dir.join("missing"), later, &mut st).is_empty());
    assert_eq!(st.unreadable, 1);
    fs::remove_dir_all(&dir).expect("remove test directory");
}

#[test]
fn a_gemini_transcript_that_changes_between_passes_is_reported() {
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-reopen-{}", std::process::id()));
    fs::create_dir(&dir).expect("create test directory");
    let path = dir.join("transcript.jsonl.zst");
    let first_pass = || Box::new(io::Cursor::new(b"{}\n")) as Box<dyn BufRead>;
    fs::write(&path, b"replacement").expect("write changed transcript");
    let changed = indexed(first_pass(), &path, Gemini::index)
        .err()
        .expect("reject the replacement");
    fs::remove_file(&path).expect("remove transcript");
    let removed = indexed(first_pass(), &path, Gemini::index)
        .err()
        .expect("report a missing transcript");
    fs::remove_dir(&dir).expect("remove test directory");
    assert_eq!(changed, "it changed whilst it was read");
    assert!(removed.starts_with("a file cannot be opened:"), "{removed}");
}

#[test]
fn a_claude_record_written_again_with_its_uuid_is_read_once() {
    let lines = [
        r#"{"type":"assistant","uuid":"u1","message":{}}"#,
        r#"{"type":"user","uuid":"u2","message":{}}"#,
        r#"{"type":"assistant","uuid":"u1","slug":"s","message":{}}"#,
        r#"{"type":"summary"}"#,
        r#"{"type":"summary"}"#,
    ]
    .join("\n");
    // Claude reads u1 once; Codex records carry no such uuid, so all count.
    for (agent, want) in [(Agent::Claude, 4), (Agent::Codex, 5)] {
        let reader = Box::new(io::Cursor::new(lines.clone().into_bytes()));
        assert_eq!(scan_one(agent, reader).0, want, "{agent:?}");
    }
}

/// Scans one transcript with `Count`, and gives the records it was given and the
/// lines skipped.
fn scan_one(agent: Agent, reader: Box<dyn BufRead>) -> (u64, u64) {
    let mut st = RootStats::new(agent, PathBuf::new());
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
    let mut refs: Vec<&mut (dyn Detector + 'static)> = dets.iter_mut().map(Box::as_mut).collect();
    scan_file(
        reader,
        None,
        Path::new("s.jsonl"),
        &Window {
            until: None,
            since: Some(Timestamp::MIN),
            prices: &Prices::bundled(),
        },
        &mut refs,
        &[0],
        &mut st,
        &mut Earlier::default(),
    )
    .expect("read");
    let n = counted(&dets).as_u64().expect("a count");
    (n, st.bad_lines)
}

#[test]
fn blank_lines_are_passed_over_and_a_record_that_does_not_parse_is_skipped() {
    let lines = [
        r#"{"type":"user","uuid":"u1","message":{}}"#,
        "",
        r#"{"type":"user","uuid":"u2" "message":{}}"#,
        "   ",
        r#"{"type":"summary"}"#,
    ]
    .join("\n");
    let reader = Box::new(io::Cursor::new(lines.into_bytes()));
    assert_eq!(scan_one(Agent::Claude, reader), (2, 1));
}

#[test]
fn blank_lines_in_a_pi_session_are_passed_over() {
    let lines = [
            r#"{"type":"session","id":"s","timestamp":"2026-11-09T06:00:00.000Z"}"#,
            "",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-09T06:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
        ]
        .join("\n");
    let reader = Box::new(io::Cursor::new(lines.into_bytes()));
    let (records, skipped) = scan_one(Agent::Pi, reader);
    assert_eq!(skipped, 0);
    assert!(records > 0);
}

#[test]
fn a_file_s_first_time_is_its_first_record_with_a_valid_timestamp() {
    let lines = [
        "not json",
        r#"{"type":"summary"}"#,
        r#"{"timestamp":"yesterday"}"#,
        r#"{"timestamp":"2026-11-08T01:00:00Z"}"#,
        r#"{"timestamp":"2026-11-08T02:00:00Z"}"#,
    ]
    .join("\n");
    let reader = Box::new(io::Cursor::new(lines.into_bytes()));
    assert_eq!(first_time_in(reader), parse_ts("2026-11-08T01:00:00Z"));
    assert_eq!(
        first_time_in(Box::new(io::Cursor::new(b"{}\n".to_vec()))),
        None
    );
}

#[test]
fn a_transcript_of_one_over_long_line_counts_it_as_skipped() {
    let long = io::repeat(b'a').take(MAX_LINE + 10);
    assert_eq!(
        scan_one(Agent::Claude, Box::new(BufReader::new(long))),
        (0, 1)
    );
}

#[test]
fn only_jsonl_and_compressed_jsonl_files_are_listed() {
    // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
    let dir = std::env::temp_dir().join(format!("bigsister-list-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("sub")).expect("temp folder");
    for name in [
        "a.jsonl",
        "sub/b.jsonl.zst",
        "notes.txt",
        "c.json",
        "d.zst",
        "jsonl",
    ] {
        fs::write(dir.join(name), "{}\n").expect("stub");
    }
    let mut st = RootStats::new(Agent::Claude, dir.clone());
    let mut got = list_files(&dir, SystemTime::UNIX_EPOCH, false, &mut st);
    got.sort();
    let otlp = list_files(&dir, SystemTime::UNIX_EPOCH, true, &mut st);
    fs::remove_dir_all(&dir).expect("clean up");
    assert_eq!(
        got,
        [dir.join("a.jsonl"), dir.join("sub").join("b.jsonl.zst")]
    );
    assert_eq!(
        otlp,
        [
            dir.join("a.jsonl"),
            dir.join("c.json"),
            dir.join("sub").join("b.jsonl.zst")
        ]
    );
    assert_eq!(st.unreadable, 0);
}

#[test]
fn a_json_file_after_a_byte_order_mark_reads_as_one_compact_line() {
    let text = b"\xEF\xBB\xBF{\n  \"resourceSpans\": [\n    {}\n  ]\n}\n";
    let mut lines = Lines::new(one_line(Box::new(&text[..])));
    let first = lines.next_line().expect("read").map(Cow::into_owned);
    assert_eq!(first.as_deref(), Some(r#"{"resourceSpans":[{}]}"#));
    assert!(lines.next_line().expect("read").is_none());
}

#[test]
fn raw_timestamps_for_both_agents() {
    let codex = r#"{"timestamp":"2026-11-08T01:00:00Z","type":"x","payload":{"timestamp":"2020-01-01T00:00:00Z"}}"#;
    assert_eq!(raw_timestamp(codex), parse_ts("2026-11-08T01:00:00Z"));
    let claude = r#"{"type":"user","message":{"content":"a \"timestamp\":\"x\""},"timestamp":"2026-11-08T02:00:00Z"}"#;
    assert_eq!(raw_timestamp(claude), parse_ts("2026-11-08T02:00:00Z"));
    assert_eq!(raw_timestamp(r#"{"type":"summary"}"#), None);
}

#[test]
fn a_line_of_exactly_the_cap_is_read() {
    let record = format!("{{\"a\":\"{}\"}}", "a".repeat(MAX_LINE as usize - 8));
    assert_eq!(record.len() as u64, MAX_LINE);
    // At the end of the file, and before a newline and another line.
    for rest in [&b""[..], b"\n{}"] {
        let bytes = [record.as_bytes(), rest].concat();
        let mut lines = Lines::new(Box::new(io::Cursor::new(bytes)));
        let first = lines.next_line().expect("read").map(|l| l.len() as u64);
        assert_eq!((first, lines.over_long), (Some(MAX_LINE), 0), "{rest:?}");
        let next = lines.next_line().expect("read").map(Cow::into_owned);
        assert_eq!(next.as_deref(), (!rest.is_empty()).then_some("{}"));
    }
}

#[test]
fn a_line_over_the_cap_is_skipped_and_counted() {
    let long = io::repeat(b'a').take(MAX_LINE + 10);
    let rest: &[u8] = b"\n{\"a\":1}\n";
    let mut lines = Lines::new(Box::new(BufReader::new(long.chain(rest))));
    let next = lines.next_line().expect("read").map(Cow::into_owned);
    assert_eq!(next.as_deref(), Some("{\"a\":1}\n"));
    assert_eq!(lines.over_long, 1);
    assert!(lines.next_line().expect("read").is_none());
}

mod parallel;
mod traces;
