//! Compaction health metrics tests.

use serde_json::{Value, json};

fn reply(
    ts: &str,
    id: &str,
    model: &str,
    input: u64,
    write: u64,
    read: u64,
    content: Value,
) -> String {
    super::reply(ts, id, model, input, write, read, &content)
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
    let dir = super::temp_dir("ct", "compaction");
    super::write_jsonl(&dir, "claude/proj/s1.jsonl", &claude_compaction_lines());
    super::write_jsonl(&dir, "codex/2026/rollout.jsonl", &codex_compaction_lines());
    let doc = super::run_json(
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
    let rows = super::table_rows(doc, "rereads", "Compaction health");
    let claude = super::find_row(
        rows,
        &[("agent", json!("claude")), ("scope", json!("main"))],
    );
    assert_eq!(claude["measured"], 2);
    assert_eq!(claude["min_post_pre_ratio"], 0.012);
    let codex = super::find_row(rows, &[("agent", json!("codex")), ("scope", json!("main"))]);
    assert_eq!(codex["measured"], 0);
    assert_eq!(codex["median_post_pre_ratio"], Value::Null);
    assert_eq!(codex["revert_cycles"], 1);
}

#[test]
fn unmeasured_compaction_resets_thrashing_streak() {
    let dir = super::temp_dir("ct", "unmeasured-streak");
    let no_pre = json!({"type": "system", "subtype": "compact_boundary", "timestamp": "2026-11-08T01:01:00Z"}).to_string();
    let lines = vec![
        boundary("2026-11-08T01:00:00Z", "auto", 100_000, Some(95_000)),
        no_pre,
        boundary("2026-11-08T01:02:00Z", "auto", 100_000, Some(95_000)),
        boundary("2026-11-08T01:03:00Z", "auto", 100_000, None),
        boundary("2026-11-08T01:04:00Z", "auto", 100_000, Some(95_000)),
    ];
    super::write_jsonl(&dir, "proj/s.jsonl", &lines);
    let doc = super::run_json("rereads", &["--claude-root", dir.to_str().unwrap()]);
    let s = &doc["detectors"]["rereads"]["summary"];
    assert_eq!(s["compactions_measured"], 3);
    assert_eq!(s["compaction_thrashing_passes"], 0);
}

fn codex_patch(ts: &str, id: &str, path: &str) -> String {
    let args = format!("*** Update File: {path}\n");
    json!({"type": "response_item", "timestamp": ts, "payload": {"type": "function_call", "name": "apply_patch", "call_id": id, "arguments": args}}).to_string()
}

fn codex_exec_cmd(ts: &str, id: &str, cmd: &str) -> String {
    let input = format!("tools.exec_command({{cmd: \"{cmd}\"}})");
    json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call", "name": "exec", "call_id": id, "input": input}}).to_string()
}

#[test]
fn codex_post_compaction_window_counts_turn_boundaries_not_tool_calls() {
    let dir = super::temp_dir("ct", "codex-turns");
    assert_codex_multi_call_turn(&dir);
    assert_codex_spaced_text_turns(&dir);
}

fn assert_codex_multi_call_turn(dir: &std::path::Path) {
    // 6 tool calls in 1 post-compaction turn must still detect churn and revert.
    let mut lines = vec![
        codex_patch("2026-11-08T01:00:00Z", "p1", "/work/a.rs"),
        json!({"type": "compacted", "timestamp": "2026-11-08T01:01:00Z"}).to_string(),
        json!({"type": "event_msg", "timestamp": "2026-11-08T01:01:01Z", "payload": {"type": "task_started", "turn_id": "t1"}}).to_string(),
        json!({"type": "turn_context", "timestamp": "2026-11-08T01:01:02Z", "payload": {"turn_id": "t1", "model": "gpt-6.1-sol"}}).to_string(),
    ];
    for i in 1..=5 {
        lines.push(codex_exec_cmd(
            "2026-11-08T01:01:03Z",
            &format!("n{i}"),
            "git status",
        ));
    }
    lines.push(codex_exec_cmd(
        "2026-11-08T01:01:04Z",
        "r1",
        "cat /work/a.rs",
    ));
    lines.push(codex_patch("2026-11-08T01:01:05Z", "p2", "/work/a.rs"));
    super::write_jsonl(dir, "multi/2026/rollout.jsonl", &lines);
    let doc = super::run_json(
        "rereads",
        &["--codex-root", dir.join("multi").to_str().unwrap()],
    );
    assert_eq!(
        doc["detectors"]["rereads"]["summary"]["post_compaction_revert_cycles"],
        1
    );
}

fn assert_codex_spaced_text_turns(dir: &std::path::Path) {
    // 6 intervening text-only Codex turns push the later read/edit past the 5-turn window.
    let mut lines = vec![
        codex_patch("2026-11-08T02:00:00Z", "p1", "/work/b.rs"),
        json!({"type": "compacted", "timestamp": "2026-11-08T02:01:00Z"}).to_string(),
    ];
    for i in 1..=6 {
        lines.push(json!({"type": "response_item", "timestamp": "2026-11-08T02:01:05Z", "payload": {"type": "message", "role": "assistant", "id": format!("m{i}")}}).to_string());
    }
    lines.push(codex_exec_cmd(
        "2026-11-08T02:02:00Z",
        "r1",
        "cat /work/b.rs",
    ));
    lines.push(codex_patch("2026-11-08T02:02:05Z", "p2", "/work/b.rs"));
    super::write_jsonl(dir, "spaced/2026/rollout.jsonl", &lines);
    let doc = super::run_json(
        "rereads",
        &["--codex-root", dir.join("spaced").to_str().unwrap()],
    );
    let s = &doc["detectors"]["rereads"]["summary"];
    assert_eq!(
        (
            s["post_compaction_churn_rereads"].clone(),
            s["post_compaction_revert_cycles"].clone()
        ),
        (json!(0), json!(0))
    );
}
