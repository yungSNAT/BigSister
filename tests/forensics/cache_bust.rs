//! Sub-TTL (< 5 min) cache-bust detection and divergence tests.

use serde_json::{Value, json};

use super::{find_row, reply as raw_reply, table_rows};

fn reply(ts: &str, id: &str, model: &str, write: u64, read: u64, content: &Value) -> String {
    raw_reply(ts, id, model, 10, write, read, content)
}

fn bust_rows(doc: &Value) -> &[Value] {
    table_rows(doc, "cache", "Sub-TTL cache busts (< 5 min)")
}

fn find_bust<'a>(rows: &'a [Value], scope: &str, cause: &str) -> &'a Value {
    find_row(rows, &[("scope", json!(scope)), ("cause", json!(cause))])
}

fn fixture_part_a() -> Vec<String> {
    let txt = json!([{"type": "text", "text": "SENTINEL-BUST-TEXT"}]);
    let exit_plan = json!([{"type": "tool_use", "id": "tu1", "name": "ExitPlanMode", "input": {}}]);
    vec![
        json!({"type": "api-request-shape", "timestamp": "2026-11-08T01:00:00Z", "id": "sh1"}).to_string(),
        json!({"type": "api-request", "timestamp": "2026-11-08T01:00:00Z", "shape": "sh1"}).to_string(),
        json!({"type": "api-request", "timestamp": "2026-11-08T01:00:00Z", "shape": "sh1", "keep": 0, "tail": ["a", "b"]}).to_string(),
        reply("2026-11-08T01:00:05Z", "m1", "claude-opus-5-5", 100_000, 0, &txt),
        reply("2026-11-08T01:00:06Z", "m1", "claude-opus-5-5", 100_000, 0, &exit_plan).replacen("u-m1", "u-m1b", 1),
        json!({"type": "assistant", "uuid": "u-m1c", "timestamp": "2026-11-08T01:00:06Z", "message": {"id": "m1", "model": "claude-opus-5-5"}}).to_string(),
        reply("2026-11-08T01:00:07Z", "m2", "claude-opus-5-5", 85_000, 20_000, &txt),
        json!({"type": "api-request-shape", "timestamp": "2026-11-08T01:00:30Z", "id": "sh2"}).to_string(),
        json!({"type": "attachment", "timestamp": "2026-11-08T01:00:31Z", "attachment": {"type": "deferred_tools_delta"}}).to_string(),
        reply("2026-11-08T01:00:35Z", "m3", "claude-opus-5-5", 60_000, 45_000, &txt),
    ]
}

fn fixture_part_b() -> Vec<String> {
    let txt = json!([{"type": "text", "text": "ok"}]);
    let rewind = reply(
        "2026-11-08T01:02:10Z",
        "m6",
        "claude-sonnet-5-5",
        40_000,
        10_000,
        &txt,
    )
    .replacen('{', r#"{"parentUuid":"u-m1b","#, 1);
    let mut v = vec![
        reply("2026-11-08T01:01:00Z", "m4", "claude-sonnet-5-5", 100_000, 5_000, &txt),
        json!({"type": "api-request", "timestamp": "2026-11-08T01:01:20Z", "shape": "sh2", "keep": 1, "tail": ["c"]}).to_string(),
        json!({"type": "system", "subtype": "microcompact_boundary", "timestamp": "2026-11-08T01:01:21Z"}).to_string(),
        reply("2026-11-08T01:01:30Z", "m5", "claude-sonnet-5-5", 50_000, 55_000, &txt),
        rewind,
    ];
    v.extend(fixture_part_c(&txt));
    v
}

fn fixture_part_c(txt: &Value) -> Vec<String> {
    vec![
        json!({"type": "api-request-shape", "timestamp": "2026-11-08T01:02:15Z"}).to_string(),
        reply("2026-11-08T01:02:20Z", "m7", "claude-sonnet-5-5", 45_000, 5_000, txt),
        json!({"type": "system", "subtype": "compact_boundary", "timestamp": "2026-11-08T01:02:25Z"}).to_string(),
        reply("2026-11-08T01:02:30Z", "m8", "claude-sonnet-5-5", 30_000, 0, txt),
    ]
}

#[test]
fn sub_ttl_cache_busts_classify_causes_and_divergence() {
    let dir = super::temp_dir("cb", "causes");
    let mut lines = fixture_part_a();
    lines.extend(fixture_part_b());
    super::write_jsonl(&dir, "proj/s1.jsonl", &lines);
    let unpriced = vec![
        reply(
            "2026-11-08T02:00:00Z",
            "u1",
            "claude-custom-unpriced",
            100_000,
            0,
            &json!([]),
        ),
        reply(
            "2026-11-08T02:00:10Z",
            "u2",
            "claude-custom-unpriced",
            90_000,
            10_000,
            &json!([]),
        ),
        reply(
            "2026-11-08T02:00:20Z",
            "u3",
            "claude-custom-unpriced-2",
            90_000,
            10_000,
            &json!([]),
        ),
        reply(
            "2026-11-08T02:00:19.500Z",
            "u4",
            "claude-custom-unpriced-2",
            80_000,
            10_000,
            &json!([]),
        ),
    ];
    super::write_jsonl(&dir, "proj/subagents/agent-1.jsonl", &unpriced);

    let root = dir.to_str().expect("utf8");
    let doc = super::run_json("cache", &["--claude-root", root]);
    assert!(!doc.to_string().contains("SENTINEL-BUST-TEXT"));
    assert_causes(&doc);
    assert_pricing_and_unpriced(root, &unpriced);
}

fn assert_causes(doc: &Value) {
    let s = &doc["detectors"]["cache"]["summary"];
    assert_eq!(s["sub_ttl_busts"], 8);
    assert!(
        s["sub_ttl_bust_usd_share"].as_f64().unwrap()
            > s["sub_ttl_bust_token_share"].as_f64().unwrap()
    );
    let rows = bust_rows(doc);
    let tool = find_bust(rows, "main", "tool load / schema change");
    assert_eq!(
        (tool["busts"].clone(), tool["rebuilt_tokens"].clone()),
        (json!(2), json!(140_000))
    );
    assert!(tool["usd_to_token_ratio"].as_f64().unwrap() > 1.0);
    assert_eq!(
        find_bust(rows, "main", "model swap")["rebuilt_tokens"],
        100_000
    );
    assert_eq!(
        find_bust(rows, "main", "mid-history edit / rewind")["busts"],
        2
    );
    assert_eq!(
        find_bust(rows, "main", "proxy or prefix change")["busts"],
        1
    );
    let sub = find_bust(rows, "subagent", "proxy or prefix change");
    assert_eq!(
        (
            sub["unpriced_tokens"].clone(),
            sub["usd_to_token_ratio"].clone()
        ),
        (json!(90_000), Value::Null)
    );
}

fn assert_pricing_and_unpriced(root: &str, unpriced: &[String]) {
    let flat = super::run_json("cache", &["--claude-root", root, "--usd-per-mtok", "10"]);
    let sub = find_bust(bust_rows(&flat), "subagent", "proxy or prefix change");
    assert_eq!(sub["unpriced_tokens"], 0);
    assert!(sub["penalty_usd_est"].as_f64().unwrap() > 0.0);

    let udir = super::temp_dir("cb", "unpriced");
    super::write_jsonl(&udir, "proj/u.jsonl", unpriced);
    let udoc = super::run_json("cache", &["--claude-root", udir.to_str().unwrap()]);
    assert_eq!(
        udoc["detectors"]["cache"]["summary"]["sub_ttl_bust_usd_share"],
        Value::Null
    );
}

#[test]
fn unpriced_sessions_do_not_inflate_priced_divergence_ratio() {
    let priced_dir = super::temp_dir("cb", "ratio-priced");
    let mixed_dir = super::temp_dir("cb", "ratio-mixed");
    let mut lines = fixture_part_a();
    lines.extend(fixture_part_b());
    super::write_jsonl(&priced_dir, "proj/s1.jsonl", &lines);
    super::write_jsonl(&mixed_dir, "proj/s1.jsonl", &lines);
    let unpriced = vec![
        reply(
            "2026-11-08T02:00:00Z",
            "u1",
            "claude-custom-unpriced",
            500_000,
            0,
            &json!([]),
        ),
        reply(
            "2026-11-08T02:00:10Z",
            "u2",
            "claude-custom-unpriced",
            400_000,
            100_000,
            &json!([]),
        ),
    ];
    super::write_jsonl(&mixed_dir, "proj/subagents/agent-1.jsonl", &unpriced);

    let pure = super::run_json("cache", &["--claude-root", priced_dir.to_str().unwrap()]);
    let mixed = super::run_json("cache", &["--claude-root", mixed_dir.to_str().unwrap()]);
    let r_pure =
        find_bust(bust_rows(&pure), "main", "tool load / schema change")["usd_to_token_ratio"]
            .clone();
    let r_mixed =
        find_bust(bust_rows(&mixed), "main", "tool load / schema change")["usd_to_token_ratio"]
            .clone();
    assert_eq!(r_pure, r_mixed);
}
