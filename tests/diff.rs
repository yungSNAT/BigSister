//! Regression tests for `bigsister diff <before> <after>`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bigsister-diff-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_jsonl(dir: &Path, rel: &str, lines: &[String]) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let text = lines.join("\n");
    fs::write(path, format!("{text}\n")).expect("write fixture");
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(args)
        .output()
        .expect("run");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn metric_row<'a>(doc: &'a Value, metric: &str) -> &'a Value {
    doc["detectors"]["diff"]["tables"][0]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| r["metric"] == metric)
        .unwrap_or_else(|| panic!("missing metric {metric}"))
}

fn session_lines(ts1: &str, ts2: &str, read2: u64, write2: u64, tag: &str) -> Vec<String> {
    vec![
        format!(
            r#"{{"type":"assistant","uuid":"u1-{tag}","sessionId":"sess-1","timestamp":"{ts1}","message":{{"model":"claude-opus-5-5","id":"m1-{tag}","role":"assistant","content":[{{"type":"text","text":"SENTINEL-DIFF-BODY"}}],"usage":{{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":100000,"cache_read_input_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":100000,"ephemeral_1h_input_tokens":0}}}}}}}}"#
        ),
        format!(
            r#"{{"type":"assistant","uuid":"u2-{tag}","sessionId":"sess-1","timestamp":"{ts2}","message":{{"model":"claude-opus-5-5","id":"m2-{tag}","role":"assistant","content":[{{"type":"text","text":"ok"}}],"usage":{{"input_tokens":1000,"output_tokens":100,"cache_creation_input_tokens":{write2},"cache_read_input_tokens":{read2},"cache_creation":{{"ephemeral_5m_input_tokens":{write2},"ephemeral_1h_input_tokens":0}}}}}}}}"#
        ),
    ]
}

#[test]
fn diff_compares_two_session_folders_in_json_csv_and_text() {
    let root = temp_dir("folders");
    let before = root.join("before");
    let after = root.join("after");
    write_jsonl(
        &before,
        "proj/s.jsonl",
        &session_lines(
            "2026-11-08T01:00:00Z",
            "2026-11-08T01:00:30Z",
            90_000,
            10_000,
            "a",
        ),
    );
    write_jsonl(
        &after,
        "proj/s.jsonl",
        &session_lines(
            "2026-11-08T02:00:00Z",
            "2026-11-08T02:00:30Z",
            10_000,
            70_000,
            "b",
        ),
    );

    let (b, a) = (before.to_str().unwrap(), after.to_str().unwrap());
    let (code, stdout, stderr) = run(&["diff", b, a, "--json"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("SENTINEL-DIFF-BODY"));
    let doc: Value = serde_json::from_str(&stdout).expect("valid json");
    assert_eq!(
        (
            metric_row(&doc, "turns")["before"].clone(),
            metric_row(&doc, "turns")["delta"].clone()
        ),
        (json!(2), json!(0))
    );
    assert!(metric_row(&doc, "input_tokens")["delta"].as_i64().unwrap() < 0);
    assert!(metric_row(&doc, "input_usd_est")["delta"].as_f64().unwrap() > 0.0);
    assert!(metric_row(&doc, "cache_hit_pct")["delta"].as_f64().unwrap() < 0.0);
    assert_eq!(metric_row(&doc, "sub_ttl_cache_busts")["after"], 1);
    assert!(
        doc["detectors"]["diff"]["summary"]["usd_token_divergence_pp"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    assert_diff_formats(&root, b, a);
}

fn assert_diff_formats(root: &Path, b: &str, a: &str) {
    let (code, text, stderr) = run(&["diff", b, a, "--usd-per-mtok", "5"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(text.contains("== diff: ") && text.contains("cache_hit_pct"));
    let (code, csv, stderr) = run(&["diff", b, a, "--csv"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(csv.contains("diff,cell,0,"));
    let price_file = root.join("prices.json");
    fs::write(
        &price_file,
        r#"{"claude-opus-5-5":{"input_cost_per_token":5e-6}}"#,
    )
    .unwrap();
    let (code, out_p, stderr) = run(&[
        "diff",
        b,
        a,
        "--prices",
        price_file.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc_p: Value = serde_json::from_str(&out_p).unwrap();
    assert_eq!(
        doc_p["detectors"]["diff"]["summary"]["usd_basis"],
        "prices_file"
    );
}

#[test]
fn diff_compares_time_windows_sample_and_agent_subfolders() {
    let root = temp_dir("windows");
    let mut lines = session_lines(
        "2026-11-08T01:00:00Z",
        "2026-11-08T01:00:30Z",
        90_000,
        10_000,
        "a",
    );
    let later = session_lines(
        "2026-11-09T01:00:00Z",
        "2026-11-09T01:00:30Z",
        50_000,
        50_000,
        "b",
    );
    lines.extend(later);
    write_jsonl(&root, "claude/proj/s.jsonl", &lines);

    let (code, stdout, stderr) = run(&[
        "diff",
        "2026-11-08T00:00:00Z..2026-11-09T00:00:00Z",
        "2026-11-09T00:00:00Z..2026-11-10T00:00:00Z",
        "--agent",
        "claude",
        "--claude-root",
        root.join("claude").to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        (
            metric_row(&doc, "turns")["before"].clone(),
            metric_row(&doc, "sub_ttl_cache_busts")["after"].clone()
        ),
        (json!(2), json!(1))
    );
    assert_diff_sample_and_dirs(&root);
}

fn assert_diff_sample_and_dirs(root: &Path) {
    assert_diff_sample();
    assert_diff_dir_modes(root);
}

fn assert_diff_sample() {
    let (code, sample_out, stderr) = run(&[
        "diff",
        "2026-11-01..2026-11-08",
        "2026-11-08..now",
        "--sample",
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let sdoc: Value = serde_json::from_str(&sample_out).unwrap();
    assert_eq!(
        (
            metric_row(&sdoc, "turns")["before"].clone(),
            metric_row(&sdoc, "cache_hit_pct")["before"].clone()
        ),
        (json!(0), Value::Null)
    );
}

fn assert_diff_dir_modes(root: &Path) {
    let empty = temp_dir("empty-dir");
    assert_eq!(
        run(&[
            "diff",
            root.to_str().unwrap(),
            empty.to_str().unwrap(),
            "--json"
        ])
        .0,
        0
    );
    assert_eq!(
        run(&[
            "diff",
            root.join("claude").to_str().unwrap(),
            empty.to_str().unwrap(),
            "--agent",
            "claude",
            "--json"
        ])
        .0,
        0
    );
    let oc = temp_dir("oc-dir");
    fs::write(oc.join("opencode.db"), b"not-a-db").unwrap();
    assert_eq!(
        run(&[
            "diff",
            oc.to_str().unwrap(),
            empty.to_str().unwrap(),
            "--json"
        ])
        .0,
        0
    );
}

#[test]
fn diff_reports_clear_errors_for_bad_targets() {
    let missing = temp_dir("missing").join("nope");
    let dir = temp_dir("ok");
    let (code, _, stderr) = run(&["diff", missing.to_str().unwrap(), dir.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("diff folder not found"), "{stderr}");

    let (code, _, stderr) = run(&["diff", "2026-11-10..2026-11-01", "2026-11-01..2026-11-10"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("start must be before end"), "{stderr}");

    let (code, _, stderr) = run(&["diff", "bad..now", "2026-11-01..now"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("expected START..END"), "{stderr}");

    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(["diff", "9999-12-31..now", "2026-11-01..now"])
        .env("TZ", "UTC")
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr.contains("out of range"), "{stderr}");

    // Relative directory path containing `..` is treated as a folder, not a time window.
    let rel_missing = format!("{}/../definitely-not-a-dir", dir.display());
    let (code, _, stderr) = run(&["diff", &rel_missing, dir.to_str().unwrap()]);
    assert_eq!(code, 1);
    assert!(stderr.contains("diff folder not found"), "{stderr}");
}

#[test]
fn diff_deduplicates_skipped_roots_across_windows() {
    // Window diff with missing default agent folders deduplicates skipped_roots cleanly.
    let fake_home = temp_dir("fake-home");
    let claude = temp_dir("win-claude");
    write_jsonl(
        &claude,
        "proj/s.jsonl",
        &session_lines(
            "2026-11-08T01:00:00Z",
            "2026-11-08T01:00:30Z",
            90_000,
            10_000,
            "h",
        ),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args([
            "diff",
            "2026-11-08T00:00:00Z..2026-11-08T01:00:15Z",
            "2026-11-08T01:00:15Z..2026-11-09T00:00:00Z",
            "--claude-root",
            claude.to_str().unwrap(),
            "--json",
        ])
        .env("HOME", &fake_home)
        .env("USERPROFILE", &fake_home)
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("valid json");
    let skipped = doc["skipped_roots"].as_array().expect("skipped_roots");
    assert!(!skipped.is_empty());
}

#[test]
fn diff_handles_dotdot_dirs_and_rejects_sample_with_folders() {
    let parent = temp_dir("dotdot");
    let child = parent.join("child");
    let dotted = parent.join("a..b");
    fs::create_dir_all(&child).unwrap();
    fs::create_dir_all(&dotted).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(["diff", "..", ".", "--json"])
        .current_dir(&child)
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out_dotted = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(["diff", "a..b", "child", "--json"])
        .current_dir(&parent)
        .output()
        .expect("run");
    assert_eq!(
        out_dotted.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out_dotted.stderr)
    );

    let (code, _, stderr) = run(&[
        "diff",
        parent.to_str().unwrap(),
        child.to_str().unwrap(),
        "--sample",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("--sample cannot be used with session folders"),
        "{stderr}"
    );
}
