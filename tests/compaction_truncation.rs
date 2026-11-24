//! Regression tests for compaction health metrics and truncation-triggered command re-runs.

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

fn boundary(ts: &str, trigger: &str, pre: u64, post: Option<u64>) -> String {
    let mut meta = json!({"trigger": trigger, "preTokens": pre});
    if let Some(p) = post {
        meta["postTokens"] = json!(p);
    }
    json!({"type": "system", "subtype": "compact_boundary", "timestamp": ts, "compactMetadata": meta}).to_string()
}

fn edit_use(id: &str, path: &str) -> Value {
    json!([{"type": "tool_use", "id": id, "name": "Edit", "input": {"file_path": path}}])
}

fn read_use(id: &str, path: &str) -> Value {
    json!([{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": path}}])
}

fn claude_compaction_lines() -> Vec<String> {
    let mut v = vec![
        boundary("2026-10-01T00:00:00Z", "auto", 100_000, Some(95_000)),
        reply(
            "2026-11-08T01:00:00Z",
            "c0",
            "claude-opus-5-5",
            1000,
            0,
            0,
            edit_use("e0", "/work/cbi/jane_notes.rs"),
        ),
        reply(
            "2026-11-08T01:00:01Z",
            "c0b",
            "claude-opus-5-5",
            1000,
            0,
            0,
            edit_use("e0b", ""),
        ),
        reply(
            "2026-11-08T01:00:02Z",
            "c0c",
            "claude-opus-5-5",
            1000,
            0,
            0,
            edit_use("e0c", "/work/cbi/direct_edit.rs"),
        ),
        boundary("2026-11-08T01:01:00Z", "auto", 100_000, None),
        reply(
            "2026-11-08T01:01:01Z",
            "syn",
            "<synthetic>",
            1,
            0,
            0,
            json!([]),
        ),
    ];
    v.extend(claude_compaction_part_b());
    v
}

fn claude_compaction_part_b() -> Vec<String> {
    let mut v = vec![
        reply(
            "2026-11-08T01:01:05Z",
            "c1",
            "claude-opus-5-5",
            4_000,
            10_000,
            80_000,
            read_use("r1", "/work/cbi/jane_notes.rs"),
        ),
        reply(
            "2026-11-08T01:01:05Z",
            "c1",
            "claude-opus-5-5",
            4_000,
            10_000,
            80_000,
            read_use("r2", "/work/cbi/jane_notes.rs"),
        )
        .replacen("u-c1", "u-c1b", 1),
        reply(
            "2026-11-08T01:01:10Z",
            "c2",
            "claude-opus-5-5",
            500,
            0,
            90_000,
            edit_use("e1", "/work/cbi/jane_notes.rs"),
        ),
    ];
    v.extend(claude_compaction_part_c());
    v
}

fn claude_compaction_part_c() -> Vec<String> {
    vec![
        reply(
            "2026-11-08T01:01:15Z",
            "c3",
            "claude-opus-5-5",
            500,
            0,
            90_000,
            edit_use("e2", "/work/cbi/jane_notes.rs"),
        ),
        reply(
            "2026-11-08T01:01:16Z",
            "c4",
            "claude-opus-5-5",
            500,
            0,
            90_000,
            edit_use("e3", "/work/cbi/direct_edit.rs"),
        ),
        boundary("2026-11-08T01:02:00Z", "manual", 835_000, Some(10_000)),
    ]
}

fn codex_compaction_lines() -> Vec<String> {
    let call = |ts: &str, name: &str, id: &str, args: &str| {
        json!({"type": "response_item", "timestamp": ts, "payload": {"type": "function_call", "name": name, "call_id": id, "arguments": args}}).to_string()
    };
    vec![
        call(
            "2026-11-08T01:00:00Z",
            "apply_patch",
            "p1",
            r#"{"input":"*** Update File: /work/app.rs\n*** Add File: /work/new.rs\n"}"#,
        ),
        call(
            "2026-11-08T01:00:05Z",
            "apply_patch",
            "p2",
            "*** Delete File: /work/old.rs\n",
        ),
        call(
            "2026-11-08T01:00:06Z",
            "apply_patch",
            "p2b",
            r#"{"patch":"*** Move to: /work/moved.rs\n"}"#,
        ),
        json!({"type": "compacted", "timestamp": "2026-11-08T01:01:00Z"}).to_string(),
        json!({"type": "response_item", "timestamp": "2026-11-08T01:01:05Z", "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "x1", "input": "tools.exec_command({cmd: \"cat /work/app.rs\"})"}}).to_string(),
        call(
            "2026-11-08T01:01:10Z",
            "apply_patch",
            "p3",
            "*** Update File: /work/app.rs\n",
        ),
    ]
}

#[test]
fn compaction_health_detects_extreme_loss_thrashing_and_reverts() {
    let dir = temp_dir("compaction");
    write_jsonl(&dir, "claude/proj/s1.jsonl", &claude_compaction_lines());
    write_jsonl(&dir, "codex/2026/rollout.jsonl", &codex_compaction_lines());
    let doc = run_json(
        "rereads",
        &[
            "--claude-root",
            dir.join("claude").to_str().unwrap(),
            "--codex-root",
            dir.join("codex").to_str().unwrap(),
        ],
    );
    assert_compaction_health(&doc);
}

fn assert_compaction_health(doc: &Value) {
    let s = &doc["detectors"]["rereads"]["summary"];
    assert_eq!(s["compactions_measured"], 2);
    assert_eq!(s["extreme_loss_compactions"], 1);
    assert_eq!(s["compaction_thrashing_passes"], 1);
    assert_eq!(s["post_compaction_revert_cycles"], 2);
    let rows = table_rows(doc, "rereads", "Compaction health");
    let claude = find_row(
        rows,
        &[("agent", json!("claude")), ("scope", json!("main"))],
    );
    assert_eq!(claude["measured"], 2);
    assert_eq!(claude["min_post_pre_ratio"], 0.012);
    let codex = find_row(rows, &[("agent", json!("codex")), ("scope", json!("main"))]);
    assert_eq!(codex["measured"], 0);
    assert_eq!(codex["median_post_pre_ratio"], Value::Null);
    assert_eq!(codex["revert_cycles"], 1);
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
