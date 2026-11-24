//! Edge-case regression tests for `bigsister diff` pricing, completeness and window bounds.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use super::diff::{diff_json, metric_row};

fn effort_reply(ts: &str, id: &str, model: &str, effort: &str, input: u64) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"u-{id}","sessionId":"s1","effort":"{effort}","timestamp":"{ts}","message":{{"model":"{model}","id":"{id}","role":"assistant","content":[],"usage":{{"input_tokens":{input},"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
    )
}

#[test]
fn diff_input_usd_is_invariant_to_regrouping_sub_cent_rows() {
    let root = super::temp_dir("diff", "regroup");
    // Two 1,000-token replies at $4/M ($0.004 each = $0.008 -> $0.01).
    let grouped = vec![
        effort_reply(
            "2026-11-08T01:00:00Z",
            "g1",
            "claude-opus-5-5",
            "high",
            1_000,
        ),
        effort_reply(
            "2026-11-08T01:01:00Z",
            "g2",
            "claude-opus-5-5",
            "high",
            1_000,
        ),
    ];
    let split = vec![
        effort_reply(
            "2026-11-08T01:00:00Z",
            "s1",
            "claude-opus-5-5",
            "low",
            1_000,
        ),
        effort_reply(
            "2026-11-08T01:01:00Z",
            "s2",
            "claude-opus-5-5",
            "high",
            1_000,
        ),
    ];
    super::write_jsonl(&root, "before/proj/s.jsonl", &grouped);
    super::write_jsonl(&root, "after/proj/s.jsonl", &split);
    let doc = diff_json(&root.join("before"), &root.join("after"), &[]);
    let usd = metric_row(&doc, "input_usd_est");
    assert_eq!(
        (usd["before"].clone(), usd["after"].clone()),
        (json!(0.01), json!(0.01))
    );
    assert_eq!(
        (usd["delta"].clone(), usd["delta_pct"].clone()),
        (json!(0.0), json!(0.0))
    );
    assert_eq!(
        doc["detectors"]["diff"]["summary"]["input_usd_delta_pct"],
        json!(0.0)
    );
}

#[test]
fn diff_marks_unpriced_cost_delta_null_whilst_preserving_zero_priced_models() {
    let root = super::temp_dir("diff", "unpriced");
    let priced = vec![effort_reply(
        "2026-11-08T01:00:00Z",
        "p1",
        "claude-opus-5-5",
        "high",
        250_000,
    )];
    let unpriced = vec![
        effort_reply(
            "2026-11-08T01:00:00Z",
            "u1",
            "claude-unknown-model",
            "high",
            250_000,
        ),
        r#"{"type":"assistant","uuid":"u-bad","sessionId":"s1","timestamp":"2026-11-08T01:01:00Z","message":{"model":"claude-opus-5-5","id":"bad1","usage":{"input_tokens":"oops"}}}"#
            .to_owned(),
    ];
    let zero = vec![effort_reply(
        "2026-11-08T01:00:00Z",
        "z1",
        "claude-free",
        "high",
        250_000,
    )];
    super::write_jsonl(&root, "priced/proj/s.jsonl", &priced);
    super::write_jsonl(&root, "unpriced/proj/s.jsonl", &unpriced);
    super::write_jsonl(&root, "zero/proj/s.jsonl", &zero);
    assert_unpriced_and_zero(&root);
}

fn assert_unpriced_and_zero(root: &Path) {
    let doc = diff_json(&root.join("priced"), &root.join("unpriced"), &[]);
    let s = &doc["detectors"]["diff"]["summary"];
    assert_eq!(
        (
            s["before_unpriced_replies"].clone(),
            s["after_unpriced_replies"].clone()
        ),
        (json!(0), json!(1))
    );
    assert_eq!(s["after_skipped_replies"], json!(1));
    assert_eq!(
        (
            s["input_usd_delta_pct"].clone(),
            s["usd_token_divergence_pp"].clone()
        ),
        (Value::Null, Value::Null)
    );
    assert_eq!(metric_row(&doc, "input_usd_est")["delta_pct"], Value::Null);
    assert!(
        doc["detectors"]["diff"]["notes"]
            .to_string()
            .contains("Floor total")
    );

    let pf = root.join("zero-prices.json");
    fs::write(
        &pf,
        r#"{"claude-opus-5-5":{"input_cost_per_token":4e-6},"claude-free":{"input_cost_per_token":0.0}}"#,
    )
    .unwrap();
    let doc_z = diff_json(
        &root.join("priced"),
        &root.join("zero"),
        &["--prices", pf.to_str().unwrap()],
    );
    assert_eq!(
        doc_z["detectors"]["diff"]["summary"]["after_unpriced_replies"],
        json!(0)
    );
    assert_eq!(
        doc_z["detectors"]["diff"]["summary"]["input_usd_delta_pct"],
        json!(-100.0)
    );
}

#[test]
fn diff_window_schema_waste_is_invariant_to_appending_replies_after_end() {
    let root = super::temp_dir("diff", "append-inv");
    let big_desc = "d".repeat(20_000);
    let shape = format!(
        r#"{{"type":"api-request-shape","timestamp":"2026-11-08T01:00:00Z","shape":{{"system":[],"tools":[{{"name":"mcp__idle__ping","description":"{big_desc}"}}]}}}}"#
    );
    let in_win = effort_reply(
        "2026-11-08T01:01:00Z",
        "w1",
        "claude-opus-5-5",
        "high",
        1_000,
    );
    let after_end = effort_reply(
        "2026-11-09T05:00:00Z",
        "w2",
        "claude-fable-5-2",
        "high",
        1_000,
    );
    super::write_jsonl(&root, "base/proj/s.jsonl", &[shape.clone(), in_win.clone()]);
    super::write_jsonl(&root, "appended/proj/s.jsonl", &[shape, in_win, after_end]);
    let run_win = |sub: &str| -> Value {
        let (code, stdout, stderr) = super::run(&[
            "diff",
            "2026-11-08T00:00:00Z..2026-11-09T00:00:00Z",
            "2026-11-09T00:00:00Z..2026-11-10T00:00:00Z",
            "--agent",
            "claude",
            "--claude-root",
            root.join(sub).to_str().unwrap(),
            "--json",
        ]);
        assert_eq!(code, 0, "{stderr}");
        serde_json::from_str(&stdout).unwrap()
    };
    let d1 = run_win("base");
    let d2 = run_win("appended");
    let w1 = metric_row(&d1, "waste_usd_est")["before"].as_f64().unwrap();
    let w2 = metric_row(&d2, "waste_usd_est")["before"].as_f64().unwrap();
    assert!(w1 > 0.0, "expected priced idle schema waste, got {w1}");
    assert_eq!(w1, w2);
}
