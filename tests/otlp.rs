//! Runs `bigsister` over synthetic OTLP/JSON trace files of GenAI spans.

use bigsister::{
    detectors::models::Models,
    otlp::ExportKeys,
    prices::Prices,
    report::Opts,
    scan::{Agent, Detector, Memory, scan_memory},
};
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

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
        assert_eq!(
            cells(sonnet, &tokens),
            json!(["main", 2, 400, 100, 1000, 33])
        );
        assert_eq!(sonnet["unpriced_replies"], json!(0));
        let other = row(models, "model", "synthetic-model-x");
        assert_eq!(cells(other, &tokens), json!(["main", 1, 50, 0, 0, 5]));
        assert_eq!(other["unpriced_replies"], json!(1));
        assert_eq!(models.len(), 2, "{models:?}");
        // The span whose cache tokens exceed its input is skipped, not guessed.
        assert_eq!(
            doc["detectors"]["models"]["summary"]["replies_skipped"],
            json!(1)
        );
        let roots = doc["roots"].as_array().expect("roots");
        assert_eq!(roots.len(), 1, "{roots:?}");
        assert_eq!(roots[0]["agent"], json!("otlp"));
        assert_eq!(roots[0]["files"], json!(2));
        assert_eq!(roots[0]["lines_skipped"], json!(0));
    }
}

#[test]
fn only_inference_operations_count_as_replies() {
    let root = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/otlp-operations"
    );
    let args = ["models", "--otlp-root", root, "--agent", "otlp"];
    let (code, stdout, stderr) = run(&[&args[..], &["--since", SINCE, "--json"]].concat());
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let models = rows(&doc, "models");
    // The chat span and the span naming no operation count; the embeddings span and the
    // invoke_agent span, whose usage totals its inference spans', do not.
    assert_eq!(models.len(), 1, "{models:?}");
    let sonnet = row(models, "model", "claude-sonnet-4-5-20250929");
    assert_eq!(
        cells(sonnet, &["replies", "input_tokens", "output_tokens"]),
        json!([2, 140, 14])
    );
    let summary = &doc["detectors"]["models"]["summary"];
    assert_eq!(summary["replies_skipped"], json!(0), "{summary}");
    assert_eq!(summary["replies_unpriced"], json!(0), "{summary}");
    assert_eq!(summary["otlp_non_inference_spans"], json!(2), "{summary}");
    let tables = doc["detectors"]["models"]["tables"]
        .as_array()
        .expect("tables");
    assert_eq!(tables.len(), 2, "{tables:?}");
    let dropped_rows: Vec<Value> = tables[1]["rows"]
        .as_array()
        .expect("dropped rows")
        .iter()
        .map(|r| cells(r, &["operation", "spans"]))
        .collect();
    assert_eq!(
        dropped_rows,
        [json!(["embeddings", 1]), json!(["invoke_agent", 1])]
    );
}

#[test]
fn tool_spans_count_calls_and_errors_but_no_cascade_or_churn() {
    let doc = report(&[]);
    let tools = rows(&doc, "tools");
    let keys = ["agent", "calls", "errors", "result_chars"];
    assert_eq!(
        cells(row(tools, "tool", "search"), &keys),
        json!(["otlp", 1, 0, 0])
    );
    // 3 erring calls of one tool with the same (empty) input, in a row across 2 traces.
    assert_eq!(
        cells(row(tools, "tool", "fetch"), &keys),
        json!(["otlp", 3, 3, 0])
    );
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

#[test]
fn repeated_exports_and_identified_transcript_copies_count_once() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/overlap");
    let native = root.join("claude");
    let export = root.join("otlp");
    for (choice, replies, native_calls, exported_calls) in
        [("all", 3, 1, 1), ("claude", 1, 1, 0), ("otlp", 3, 0, 2)]
    {
        let (code, out, err) = run(&[
            "all",
            "--since",
            SINCE,
            "--json",
            "--usd-per-mtok",
            "100",
            "--agent",
            choice,
            "--claude-root",
            native.to_str().unwrap(),
            "--otlp-root",
            export.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
        let doc: Value = serde_json::from_str(&out).unwrap();
        let models = rows(&doc, "models");
        assert_eq!(models[0]["replies"], replies, "{choice}: {models:?}");
        assert_eq!(models[0]["input_tokens"], replies * 100);
        assert_eq!(models[0]["output_tokens"], replies * 5);
        assert!(
            (models[0]["input_usd_est"].as_f64().unwrap() - f64::from(replies) * 0.01).abs() < 1e-9
        );
        let tools = rows(&doc, "tools");
        for (source, calls) in [("claude", native_calls), ("otlp", exported_calls)] {
            let result = tools.iter().find(|row| row["agent"] == source);
            if calls == 0 {
                assert!(result.is_none(), "{choice}: {tools:?}");
            } else {
                let result = result.expect("counted source");
                assert_eq!(result["calls"], calls);
                assert_eq!(result["errors"], calls);
                assert_eq!(
                    result["result_chars"],
                    if source == "claude" { 7 } else { 0 }
                );
            }
        }
    }
}

#[test]
fn distinct_spans_claiming_one_native_identity_remain_countable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/overlap");
    let (code, out, err) = run(&[
        "all",
        "--since",
        SINCE,
        "--json",
        "--claude-root",
        root.join("claude").to_str().unwrap(),
        "--otlp-root",
        root.join("ambiguous").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(&out).unwrap();
    let model = &rows(&doc, "models")[0];
    assert_eq!(
        cells(model, &["replies", "input_tokens", "output_tokens"]),
        json!([3, 300, 15])
    );
    let tools = rows(&doc, "tools");
    assert_eq!(
        cells(row(tools, "agent", "otlp"), &["calls", "errors"]),
        json!([2, 2])
    );
    assert_eq!(
        cells(row(tools, "agent", "claude"), &["calls", "result_chars"]),
        json!([1, 7])
    );
}

#[test]
fn native_pairs_survive_sparse_copies_in_either_order_and_incomplete_claimants() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/overlap");
    let root = std::env::temp_dir().join(format!("bigsister-enrichment-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    for (first, second) in [
        ("otlp/one.json", "enrichment/sparse.json"),
        ("enrichment/sparse.json", "otlp/one.json"),
    ] {
        for (source, name) in [
            (first, "a.json"),
            (second, "b.json"),
            ("enrichment/incomplete.json", "c.json"),
        ] {
            std::fs::copy(fixture.join(source), root.join(name)).unwrap();
        }
        let (code, out, err) = run(&[
            "all",
            "--since",
            SINCE,
            "--json",
            "--claude-root",
            fixture.join("claude").to_str().unwrap(),
            "--otlp-root",
            root.to_str().unwrap(),
        ]);
        assert_eq!(code, 0, "{err}");
        let doc: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            cells(&rows(&doc, "models")[0], &["replies", "input_tokens"]),
            json!([1, 100])
        );
        let tools = rows(&doc, "tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            cells(&tools[0], &["agent", "calls", "result_chars"]),
            json!(["claude", 1, 7])
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mixed_in_memory_exports_use_both_formats_and_require_a_trace_for_native_precedence() {
    let native = Memory {
        agent: Agent::Claude,
        path: "transcript.jsonl",
        text: include_str!("fixtures/overlap/claude/transcript.jsonl"),
    };
    let complete = include_str!("fixtures/overlap/otlp/one.json");
    let anonymous = include_str!("fixtures/overlap/no-trace/export.json");
    let export = |path, text| Memory {
        agent: Agent::Otlp,
        path,
        text,
    };
    let repeated = vec![
        export("first.json", complete),
        native,
        export("again.jsonl", complete),
    ];
    let missing_trace = vec![native, export("anonymous.json", anonymous)];
    assert_mixed_memory_replies(&repeated, SINCE, 1);
    assert_mixed_memory_replies(&repeated, "2030-01-01T00:00:00Z", 0);
    assert_mixed_memory_replies(&missing_trace, SINCE, 2);
}

fn assert_mixed_memory_replies(files: &[Memory], since: &str, expected: u64) {
    let prices = Rc::new(Prices::bundled());
    let opts = Opts {
        limit: 10,
        paths: false,
        by_branch: false,
        by_day: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    let mut detectors: Vec<Box<dyn Detector>> = vec![
        Box::new(Models::new(Rc::clone(&prices))),
        Box::new(ExportKeys::default()),
    ];
    let roots = scan_memory(
        files,
        "fixture",
        Some(since.parse().unwrap()),
        &prices,
        &mut detectors,
    );
    assert_eq!(roots.len(), 2);
    let section = detectors[0].section(&opts);
    assert_eq!(
        section
            .summary
            .iter()
            .find(|(k, _)| k == "replies")
            .unwrap()
            .1,
        json!(expected)
    );
    let identities = detectors[1].section(&opts);
    assert!(identities.tables.is_empty() && identities.summary.is_empty());
}
