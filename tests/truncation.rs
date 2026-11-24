//! Regression tests for truncation-triggered command re-runs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bigsister-ct-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_jsonl(dir: &Path, rel: &str, lines: &[String]) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    let body = lines.join("\n");
    fs::write(path, format!("{body}\n")).expect("write fixture");
}

fn run_json(cmd: &str, extra: &[&str]) -> Value {
    let mut args = vec![cmd, "--since", "2026-11-01", "--json"];
    args.extend(extra);
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(&args)
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn table_rows<'a>(doc: &'a Value, section: &str, title: &str) -> &'a [Value] {
    doc["detectors"][section]["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .find(|t| t["title"] == title)
        .and_then(|t| t["rows"].as_array())
        .map(Vec::as_slice)
        .expect("table by title")
}

fn find_row<'a>(rows: &'a [Value], pairs: &[(&str, Value)]) -> &'a Value {
    rows.iter()
        .find(|r| pairs.iter().all(|(k, v)| r.get(*k) == Some(v)))
        .unwrap_or_else(|| panic!("no row matching {pairs:?}"))
}

fn reply(
    ts: &str,
    id: &str,
    model: &str,
    input: u64,
    write: u64,
    read: u64,
    content: Value,
) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"u-{id}","timestamp":"{ts}","message":{{"model":"{model}","id":"{id}","role":"assistant","content":{content},"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{write},"cache_read_input_tokens":{read}}}}}}}"#
    )
}

fn read_use(id: &str, path: &str) -> Value {
    json!([{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": path}}])
}

fn bash_use(id: &str, cmd: &str) -> Value {
    json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": cmd}}])
}

fn tool_res(ts: &str, id: &str, content: Value) -> String {
    json!({
        "type": "user", "timestamp": ts,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": content, "is_error": false}]}
    }).to_string()
}

fn truncation_pre_window() -> Vec<String> {
    vec![
        reply(
            "2026-10-01T00:00:00Z",
            "pre1",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("tp1", "cargo check"),
        ),
        tool_res("2026-10-01T00:00:02Z", "tp1", json!("[output truncated]")),
        reply(
            "2026-10-01T00:00:10Z",
            "pre2",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("tp2", "cargo check"),
        ),
        tool_res("2026-10-01T00:00:12Z", "tp2", json!("ok")),
    ]
}

fn truncation_claude_a() -> Vec<String> {
    let big = format!(
        "{}SENTINEL-TRUNC-MID{}[output truncated: 240 lines omitted]",
        "a".repeat(5000),
        "b".repeat(5000)
    );
    let mut v = truncation_pre_window();
    v.extend([
        reply(
            "2026-11-08T01:00:00Z",
            "p1",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t1", "cargo test SENTINEL-CMD"),
        ),
        tool_res("2026-11-08T01:00:02Z", "t1", json!(big)),
        reply(
            "2026-11-08T01:00:10Z",
            "p2",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t2", "cargo test SENTINEL-CMD"),
        ),
        tool_res("2026-11-08T01:00:12Z", "t2", json!("all 5 tests passed")),
        reply(
            "2026-11-08T01:00:20Z",
            "p3",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t3", "rustc --version"),
        ),
        tool_res(
            "2026-11-08T01:00:22Z",
            "t3",
            json!("run recall 42 for full output"),
        ),
    ]);
    v
}

fn truncation_claude_b() -> Vec<String> {
    let persisted = json!({
        "type": "user",
        "timestamp": "2026-11-08T01:00:27Z",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t4", "content": [{"type": "text", "text": "ok"}], "is_error": false, "persistedOutputPath": "/tmp/out.log"}]}
    }).to_string();
    vec![
        reply(
            "2026-11-08T01:00:25Z",
            "p4",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t4", "git diff"),
        ),
        persisted,
        reply(
            "2026-11-08T01:00:30Z",
            "p5",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t5", "git diff"),
        ),
        tool_res("2026-11-08T01:00:31Z", "t5", Value::Null),
        reply(
            "2026-11-08T01:00:30Z",
            "p5",
            "claude-opus-5-5",
            10,
            0,
            0,
            read_use("rp5", "/work/a.rs"),
        )
        .replacen("u-p5", "u-p5b", 1),
        reply(
            "2026-11-08T01:00:35Z",
            "p6",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("t6", "rustc --version"),
        ),
        tool_res("2026-11-08T01:00:36Z", "t6", json!("clean")),
    ]
}

fn truncation_codex_lines() -> Vec<String> {
    [
        json!({"type": "response_item", "timestamp": "2026-11-08T01:00:00Z", "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "c1", "input": "tools.exec_command({cmd: \"git status\"})"}}),
        json!({"type": "response_item", "timestamp": "2026-11-08T01:00:05Z", "payload": {"type": "custom_tool_call_output", "call_id": "c1", "output": r#"{"exit_code":0,"output":"output too long; <persisted-output>"}"#}}),
        json!({"type": "response_item", "timestamp": "2026-11-08T01:00:10Z", "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "c2", "input": "tools.exec_command({cmd: \"git status\"})"}}),
        json!({"type": "response_item", "timestamp": "2026-11-08T01:00:15Z", "payload": {"type": "custom_tool_call_output", "call_id": "c2", "output": r#"{"exit_code":0,"output":"1 passed"}"#}}),
    ]
    .into_iter()
    .map(|v| v.to_string())
    .collect()
}

#[test]
fn truncation_reruns_flag_repeats_within_two_turns_only() {
    // Patrick Jane always notices when the same command repeats within 2 turns of a cut output.
    let dir = temp_dir("truncation");
    let mut claude = truncation_claude_a();
    claude.extend(truncation_claude_b());
    write_jsonl(&dir, "claude/proj/s1.jsonl", &claude);
    write_jsonl(&dir, "codex/2026/rollout.jsonl", &truncation_codex_lines());
    let doc = run_json(
        "polls",
        &[
            "--claude-root",
            dir.join("claude").to_str().unwrap(),
            "--codex-root",
            dir.join("codex").to_str().unwrap(),
            "--usd-per-mtok",
            "10",
        ],
    );
    assert_truncation_rows(&doc);
}

fn assert_truncation_rows(doc: &Value) {
    assert!(!doc.to_string().contains("SENTINEL-CMD"));
    assert!(!doc.to_string().contains("SENTINEL-TRUNC-MID"));
    let s = &doc["detectors"]["polls"]["summary"];
    assert_eq!(s["truncated_outputs"], 4);
    assert_eq!(s["truncation_reruns"], 3);
    let rows = table_rows(doc, "polls", "Truncation-triggered command re-runs");
    let cargo = find_row(
        rows,
        &[("agent", json!("claude")), ("verb", json!("cargo"))],
    );
    assert_eq!(cargo["truncated_outputs"], 1);
    assert_eq!(cargo["truncation_reruns"], 1);
    assert_eq!(cargo["rerun_chars"], 18);
    assert_eq!(
        find_row(rows, &[("agent", json!("claude")), ("verb", json!("git"))])["truncation_reruns"],
        1
    );
    assert_eq!(
        find_row(
            rows,
            &[("agent", json!("claude")), ("verb", json!("rustc"))]
        )["truncation_reruns"],
        0
    );
    assert_eq!(
        find_row(rows, &[("agent", json!("codex")), ("verb", json!("git"))])["truncation_reruns"],
        1
    );
}

#[test]
fn plain_assistant_turns_advance_truncation_turn_gap() {
    let dir = temp_dir("plain-turns");
    let txt = json!([{"type": "text", "text": "thinking through next step"}]);
    let mut lines = vec![
        reply(
            "2026-11-08T01:00:00Z",
            "pt1",
            "claude-opus-5-5",
            10,
            0,
            0,
            bash_use("tb1", "cargo test"),
        ),
        tool_res("2026-11-08T01:00:02Z", "tb1", json!("[output truncated]")),
        reply(
            "2026-11-08T01:00:05Z",
            "pt-syn",
            "<synthetic>",
            0,
            0,
            0,
            txt.clone(),
        ),
    ];
    for (id, ts) in [
        ("pt2", "2026-11-08T01:00:10Z"),
        ("pt3", "2026-11-08T01:00:15Z"),
        ("pt4", "2026-11-08T01:00:20Z"),
    ] {
        lines.push(reply(ts, id, "claude-opus-5-5", 10, 0, 0, txt.clone()));
    }
    lines.push(reply(
        "2026-11-08T01:00:25Z",
        "pt5",
        "claude-opus-5-5",
        10,
        0,
        0,
        bash_use("tb2", "cargo test"),
    ));
    lines.push(tool_res("2026-11-08T01:00:27Z", "tb2", json!("ok")));
    write_jsonl(&dir, "proj/s.jsonl", &lines);
    let doc = run_json("polls", &["--claude-root", dir.to_str().unwrap()]);
    let s = &doc["detectors"]["polls"]["summary"];
    assert_eq!(s["truncated_outputs"], 1);
    assert_eq!(s["truncation_reruns"], 0);
}
