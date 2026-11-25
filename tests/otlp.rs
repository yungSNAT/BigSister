//! Runs `bigsister` over synthetic OTLP/JSON trace files of GenAI spans.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

const SINCE: &str = "2026-11-01T00:00:00Z";
const OTLP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/otlp");

/// Runs `bigsister` with an empty home folder, so that no folder of the machine running
/// the tests is read.
fn run(args: &[&str]) -> (i32, String, String) {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty-otlp");
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

fn report(extra: &[&str]) -> Value {
    let mut args = vec!["all", "--otlp-root", OTLP, "--since", SINCE, "--json"];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.contains("SENTINEL"), "{stdout}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn rows<'a>(doc: &'a Value, detector: &str) -> &'a Vec<Value> {
    doc["detectors"][detector]["tables"][0]["rows"]
        .as_array()
        .expect("rows")
}

fn row<'a>(rows: &'a [Value], key: &str, value: &str) -> &'a Value {
    rows.iter()
        .find(|r| r[key] == json!(value))
        .unwrap_or_else(|| panic!("no {key} {value} in {rows:?}"))
}

fn cells(row: &Value, keys: &[&str]) -> Value {
    keys.iter().map(|k| row[*k].clone()).collect()
}

#[test]
fn inference_spans_give_input_side_cost_by_model() {
    for agent in ["all", "otlp"] {
        let doc = report(&["--agent", agent]);
        let models = rows(&doc, "models");
        let tokens = [
            "scope",
            "replies",
            "input_tokens",
            "cache_write_tokens",
            "cache_read_tokens",
            "output_tokens",
        ];
        // Cache tokens are taken out of the input that counts them; the pretty-printed
        // file's reply is the second; the span before the window counts nowhere.
        let sonnet = row(models, "model", "claude-sonnet-4-5-20250929");
        assert_eq!(cells(sonnet, &tokens), json!(["main", 2, 400, 100, 1000, 33]));
        assert!(sonnet["input_usd_est"].as_f64().expect("priced") > 0.0);
        let other = row(models, "model", "synthetic-model-x");
        assert_eq!(cells(other, &tokens), json!(["main", 1, 50, 0, 0, 5]));
        assert_eq!(other["unpriced_replies"], json!(1));
        assert_eq!(models.len(), 2, "{models:?}");
        // The span whose cache tokens exceed its input is skipped, not guessed.
        assert_eq!(doc["detectors"]["models"]["summary"]["replies_skipped"], json!(1));
        let roots = doc["roots"].as_array().expect("roots");
        assert_eq!(roots.len(), 1, "{roots:?}");
        assert_eq!(roots[0]["agent"], json!("otlp"));
        assert_eq!(roots[0]["files"], json!(2));
        assert_eq!(roots[0]["lines_skipped"], json!(0));
    }
}

#[test]
fn tool_spans_count_calls_and_errors_but_no_cascade_or_churn() {
    let doc = report(&[]);
    let tools = rows(&doc, "tools");
    let keys = ["agent", "calls", "errors", "result_chars"];
    assert_eq!(cells(row(tools, "tool", "search"), &keys), json!(["otlp", 1, 0, 0]));
    // 3 erring calls of one tool with the same (empty) input, in a row across 2 traces.
    assert_eq!(cells(row(tools, "tool", "fetch"), &keys), json!(["otlp", 3, 3, 0]));
    let summary = &doc["detectors"]["tools"]["summary"];
    assert_eq!(summary["cascades"], json!(0));
    assert_eq!(summary["retry_churn_calls"], json!(0));
}

#[test]
fn otlp_files_are_read_only_from_a_given_folder() {
    let (code, _, stderr) = run(&["models", "--agent", "otlp", "--since", SINCE]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("--otlp-root"), "{stderr}");
    let (code, _, stderr) = run(&["models", "--sample", "--otlp-root", OTLP]);
    assert_ne!(code, 0);
    assert!(stderr.contains("--otlp-root"), "{stderr}");
}
