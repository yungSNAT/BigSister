//! Truncation-triggered command re-run tests.

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
    let dir = super::temp_dir("tr", "truncation");
    let mut claude = truncation_claude_a();
    claude.extend(truncation_claude_b());
    super::write_jsonl(&dir, "claude/proj/s1.jsonl", &claude);
    super::write_jsonl(&dir, "codex/2026/rollout.jsonl", &truncation_codex_lines());
    let doc = super::run_json(
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
    let rows = super::table_rows(doc, "polls", "Truncation-triggered command re-runs");
    let cargo = super::find_row(
        rows,
        &[("agent", json!("claude")), ("verb", json!("cargo"))],
    );
    assert_eq!(cargo["truncated_outputs"], 1);
    assert_eq!(cargo["truncation_reruns"], 1);
    assert_eq!(cargo["rerun_chars"], 18);
    assert_eq!(
        super::find_row(rows, &[("agent", json!("claude")), ("verb", json!("git"))])["truncation_reruns"],
        1
    );
    assert_eq!(
        super::find_row(
            rows,
            &[("agent", json!("claude")), ("verb", json!("rustc"))]
        )["truncation_reruns"],
        0
    );
    assert_eq!(
        super::find_row(rows, &[("agent", json!("codex")), ("verb", json!("git"))])["truncation_reruns"],
        1
    );
}

#[test]
fn plain_assistant_turns_advance_truncation_turn_gap() {
    let dir = super::temp_dir("tr", "plain-turns");
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
    super::write_jsonl(&dir, "proj/s.jsonl", &lines);
    let doc = super::run_json("polls", &["--claude-root", dir.to_str().unwrap()]);
    let s = &doc["detectors"]["polls"]["summary"];
    assert_eq!(s["truncated_outputs"], 1);
    assert_eq!(s["truncation_reruns"], 0);
}

fn codex_exec(ts: &str, id: &str, cmd: &str) -> String {
    json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call", "name": "exec", "call_id": id, "input": format!("text(await tools.exec_command({{cmd: \"{cmd}\"}}));")}}).to_string()
}

fn codex_out(ts: &str, id: &str, code: i64, output: &str) -> String {
    let body = json!({"exit_code": code, "output": output}).to_string();
    json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call_output", "call_id": id, "output": [{"type": "input_text", "text": body}]}}).to_string()
}

fn codex_item(ts: &str, args: &[&str], code: i64, out: &str, ms: i64) -> String {
    json!({"type": "event_msg", "timestamp": ts, "payload": {"type": "item_completed", "started_at_ms": ms, "completed_at_ms": ms + 5, "item": {"type": "CommandExecution", "command": args, "exit_code": code, "aggregated_output": out, "status": "completed"}}}).to_string()
}

#[test]
fn codex_truncation_uses_turn_boundaries_for_multi_call_and_text_turns() {
    let dir = super::temp_dir("tr", "codex-turns");
    // Multi-call turn: 3 extra calls inside turn t1 must not advance the turn gap past 2.
    let multi = vec![
        json!({"type": "event_msg", "timestamp": "2026-11-08T01:00:00Z", "payload": {"type": "task_started", "turn_id": "t1"}}).to_string(),
        json!({"type": "turn_context", "timestamp": "2026-11-08T01:00:00Z", "payload": {"turn_id": "t1", "model": "gpt-6.1-sol"}}).to_string(),
        codex_exec("2026-11-08T01:00:01Z", "c1", "git status"),
        codex_out("2026-11-08T01:00:02Z", "c1", 0, "[output truncated]"),
        codex_exec("2026-11-08T01:00:03Z", "c2", "git diff"),
        codex_exec("2026-11-08T01:00:04Z", "c3", "cargo check"),
        codex_exec("2026-11-08T01:00:05Z", "c4", "rustc --version"),
        json!({"type": "token_usage_record", "timestamp": "2026-11-08T01:00:06Z", "payload": {"turn_id": "t1", "response_id": "r1"}}).to_string(),
        json!({"type": "token_usage_record", "timestamp": "2026-11-08T01:00:06Z", "payload": {"turn_id": "t1", "response_id": "r1"}}).to_string(),
        json!({"type": "turn_context", "timestamp": "2026-11-08T01:00:10Z", "payload": {"turn_id": "t2", "model": "gpt-6.1-sol"}}).to_string(),
        codex_exec("2026-11-08T01:00:11Z", "c5", "git status"),
        codex_out("2026-11-08T01:00:12Z", "c5", 0, "clean"),
    ];
    super::write_jsonl(&dir, "multi/2026/rollout.jsonl", &multi);
    let doc_multi = super::run_json(
        "polls",
        &["--codex-root", dir.join("multi").to_str().unwrap()],
    );
    assert_eq!(
        doc_multi["detectors"]["polls"]["summary"]["truncation_reruns"],
        1
    );
    assert_codex_text_turns(&dir);
}

fn assert_codex_text_turns(dir: &std::path::Path) {
    // 3 intervening text-only Codex turns push the repeat past the 2-turn gap.
    let spaced = vec![
        json!({"type": "turn_context", "timestamp": "2026-11-08T02:00:00Z", "payload": {"turn_id": "u1"}}).to_string(),
        codex_exec("2026-11-08T02:00:01Z", "s1", "git status"),
        codex_out("2026-11-08T02:00:02Z", "s1", 0, "[output truncated]"),
        json!({"type": "response_item", "timestamp": "2026-11-08T02:00:03Z", "payload": {"type": "message", "role": "assistant", "id": "m1"}}).to_string(),
        json!({"type": "response_item", "timestamp": "2026-11-08T02:00:03Z", "payload": {"type": "message", "role": "assistant", "id": "m1"}}).to_string(),
        json!({"type": "response_item", "timestamp": "2026-11-08T02:00:04Z", "payload": {"type": "message", "role": "user", "id": "usr1"}}).to_string(),
        json!({"type": "token_usage_record", "timestamp": "2026-11-08T02:00:05Z", "payload": {"response_id": "u1r1"}}).to_string(),
        json!({"type": "token_usage_record", "timestamp": "2026-11-08T02:00:06Z", "payload": {"response_id": "u1r2"}}).to_string(),
        json!({"type": "event_msg", "timestamp": "2026-11-08T02:00:07Z", "payload": {"type": "agent_message", "id": "am1"}}).to_string(),
        json!({"type": "response_item", "timestamp": "2026-11-08T02:00:08Z", "payload": {"type": "message", "role": "assistant", "id": "m3"}}).to_string(),
        codex_exec("2026-11-08T02:00:10Z", "s2", "git status"),
        codex_out("2026-11-08T02:00:11Z", "s2", 0, "clean"),
    ];
    super::write_jsonl(dir, "spaced/2026/rollout.jsonl", &spaced);
    let doc = super::run_json(
        "polls",
        &["--codex-root", dir.join("spaced").to_str().unwrap()],
    );
    assert_eq!(doc["detectors"]["polls"]["summary"]["truncated_outputs"], 1);
    assert_eq!(doc["detectors"]["polls"]["summary"]["truncation_reruns"], 0);
}

#[test]
fn codex_command_execution_uses_model_visible_truncation_not_archived_stdout() {
    let dir = super::temp_dir("tr", "codex-visible");
    // Archived CommandExecution holds complete non-truncated stdout, while visible output is truncated.
    let ts = "2026-11-08T01:00:00.000Z";
    let ms = 1_794_099_600_000_i64;
    let lines = vec![
        codex_exec(ts, "v1", "cargo test"),
        codex_item(ts, &["cargo", "test"], 0, "full archived stdout\nall lines intact", ms + 1),
        codex_out(ts, "v1", 0, "output too long; <persisted-output>"),
        codex_exec("2026-11-08T01:00:05.000Z", "v2", "cargo test"),
        codex_item("2026-11-08T01:00:05.000Z", &["cargo", "test"], 0, "1 passed", ms + 5001),
        codex_out("2026-11-08T01:00:05.000Z", "v2", 0, "1 passed"),
        // Tail-truncated visible output (shorter suffix of archived stdout, no marker text).
        codex_exec("2026-11-08T01:00:10.000Z", "v3", "git status"),
        codex_item("2026-11-08T01:00:10.000Z", &["git", "status"], 0, "head lines\ntail line", ms + 10_001),
        codex_out("2026-11-08T01:00:10.000Z", "v3", 0, "tail line"),
        codex_exec("2026-11-08T01:00:15.000Z", "v4", "git status"),
        codex_out("2026-11-08T01:00:15.000Z", "v4", 0, "clean"),
        // Non-JSON plain-text truncated output on single tied command.
        codex_exec("2026-11-08T01:00:20.000Z", "v5", "git diff"),
        codex_item("2026-11-08T01:00:20.000Z", &["git", "diff"], 0, "full diff", ms + 20_001),
        json!({"type": "response_item", "timestamp": "2026-11-08T01:00:20.000Z", "payload": {"type": "custom_tool_call_output", "call_id": "v5", "output": "Script completed\n[output truncated]"}}).to_string(),
        codex_exec("2026-11-08T01:00:25.000Z", "v6", "git diff"),
        codex_out("2026-11-08T01:00:25.000Z", "v6", 0, "ok"),
    ];
    super::write_jsonl(&dir, "2026/rollout.jsonl", &lines);
    let doc = super::run_json("polls", &["--codex-root", dir.to_str().unwrap()]);
    let s = &doc["detectors"]["polls"]["summary"];
    assert_eq!(s["truncated_outputs"], 3);
    assert_eq!(s["truncation_reruns"], 3);
}

#[test]
fn codex_mirrored_idless_messages_coalesce_at_two_turn_boundary() {
    let dir = super::temp_dir("tr", "codex-mirror");
    let ev = json!({"type": "event_msg", "timestamp": "2026-11-08T01:00:05Z", "payload": {"type": "agent_message", "message": "ok"}}).to_string();
    let item = json!({"type": "response_item", "timestamp": "2026-11-08T01:00:05Z", "payload": {"type": "message", "role": "assistant"}}).to_string();
    for (name, pairs, expect) in [("within", 2_usize, 1_u64), ("past", 3, 0)] {
        let mut lines = vec![
            json!({"type": "turn_context", "timestamp": "2026-11-08T01:00:00Z", "payload": {"turn_id": "t1"}}).to_string(),
            codex_exec("2026-11-08T01:00:01Z", "m1", "git status"),
            codex_out("2026-11-08T01:00:02Z", "m1", 0, "[output truncated]"),
            ev.clone(),
            item.clone(),
        ];
        for i in 0..pairs {
            if i % 2 == 0 {
                lines.extend([ev.clone(), item.clone()]);
            } else {
                lines.extend([item.clone(), ev.clone()]);
            }
        }
        lines.push(codex_exec("2026-11-08T01:00:20Z", "m2", "git status"));
        lines.push(codex_out("2026-11-08T01:00:21Z", "m2", 0, "clean"));
        super::write_jsonl(&dir, &format!("{name}/2026/rollout.jsonl"), &lines);
        let doc = super::run_json("polls", &["--codex-root", dir.join(name).to_str().unwrap()]);
        assert_eq!(
            doc["detectors"]["polls"]["summary"]["truncation_reruns"], expect,
            "{name}"
        );
    }
}

#[test]
fn codex_separate_truncation_notice_flags_single_call_only() {
    let dir = super::temp_dir("tr", "codex-notice");
    let ts = "2026-11-08T01:00:00.000Z";
    let ms = 1_794_099_600_000_i64;
    let notice_out = |id: &str| {
        let body = json!({"exit_code": 0, "output": "head"}).to_string();
        json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call_output", "call_id": id, "output": [{"type": "input_text", "text": body}, {"type": "input_text", "text": "[output truncated]"}]}}).to_string()
    };
    let multi_exec = json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "mc1", "input": "text(await tools.exec_command({cmd: \"rustc --version\"})); text(await tools.exec_command({cmd: \"cargo check\"}));"}}).to_string();
    let multi_out = json!({"type": "response_item", "timestamp": ts, "payload": {"type": "custom_tool_call_output", "call_id": "mc1", "output": [{"type": "input_text", "text": r#"{"exit_code":0,"output":"ok"}"#}, {"type": "input_text", "text": r#"{"exit_code":0,"output":"[output truncated]"}"#}]}}).to_string();
    let lines = vec![
        // Single-command tied to archived CommandExecution + separate [output truncated] notice.
        codex_exec(ts, "s1", "git status"),
        codex_item(ts, &["git", "status"], 0, "head", ms + 1),
        notice_out("s1"),
        codex_exec(ts, "s2", "git status"),
        codex_out(ts, "s2", 0, "clean"),
        // Single-command untied (by_position) + separate [output truncated] notice.
        codex_exec(ts, "u1", "git diff"),
        notice_out("u1"),
        codex_exec(ts, "u2", "git diff"),
        codex_out(ts, "u2", 0, "clean"),
        // Multi-command tied cell: clean command must NOT inherit sibling's truncation marker.
        multi_exec,
        codex_item(ts, &["rustc", "--version"], 0, "ok", ms + 2),
        codex_item(ts, &["cargo", "check"], 0, "full archived check", ms + 3),
        multi_out,
        codex_exec(ts, "mc2", "rustc --version"),
        codex_out(ts, "mc2", 0, "ok"),
        codex_exec(ts, "mc3", "cargo check"),
        codex_out(ts, "mc3", 0, "ok"),
    ];
    super::write_jsonl(&dir, "2026/rollout.jsonl", &lines);
    let doc = super::run_json("polls", &["--codex-root", dir.to_str().unwrap()]);
    let s = &doc["detectors"]["polls"]["summary"];
    assert_eq!(
        (
            s["truncated_outputs"].clone(),
            s["truncation_reruns"].clone()
        ),
        (json!(3), json!(3))
    );
    let rows = super::table_rows(&doc, "polls", "Truncation-triggered command re-runs");
    assert!(
        rows.iter().all(|r| r["verb"] != "rustc"),
        "clean sibling must not be flagged: {rows:?}"
    );
}
