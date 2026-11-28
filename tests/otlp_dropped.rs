//! Integration tests for surfacing OTLP GenAI spans excluded by `gen_ai.operation.name`.

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

const SINCE: &str = "2026-11-01T00:00:00Z";
const IN_WIN: &str = "1794272400000000000";
const BEFORE_WIN: &str = "1760000000000000000";

fn run(args: &[&str]) -> (i32, String, String) {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty-otlp-dropped");
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

fn cells(row: &Value, keys: &[&str]) -> Value {
    keys.iter().map(|k| row[*k].clone()).collect()
}

fn kv(key: &str, value: Value) -> Value {
    json!({"key": key, "value": value})
}

fn otlp_span(trace: Option<&str>, id: Option<&str>, end: Option<&str>, attrs: Vec<Value>) -> Value {
    let mut s = json!({"attributes": attrs});
    if let Some(t) = trace {
        s["traceId"] = json!(t);
    }
    if let Some(i) = id {
        s["spanId"] = json!(i);
    }
    if let Some(e) = end {
        s["endTimeUnixNano"] = json!(e);
    }
    s
}

fn model_usage(op: Value) -> Vec<Value> {
    vec![
        kv("gen_ai.operation.name", op),
        kv(
            "gen_ai.request.model",
            json!({"stringValue": "claude-sonnet-4-5-20250929"}),
        ),
        kv("gen_ai.usage.input_tokens", json!({"intValue": "10"})),
        kv("gen_ai.usage.output_tokens", json!({"intValue": "2"})),
    ]
}

fn dropped(trace: Option<&str>, id: Option<&str>, end: Option<&str>, op: &str) -> Value {
    otlp_span(trace, id, end, model_usage(json!({"stringValue": op})))
}

fn write_export(dir: &Path, name: &str, spans: Vec<Value>) {
    let doc = json!({"resourceSpans": [{"scopeSpans": [{"spans": spans}]}]});
    std::fs::write(dir.join(name), doc.to_string()).expect("write export");
}

#[test]
fn dropped_only_otlp_file_counts_in_window_across_formats_and_skips_outside() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("otlp-dropped-only");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    write_export(
        &dir,
        "only.json",
        vec![dropped(
            Some("t0"),
            Some("d0"),
            Some(IN_WIN),
            "custom.chat-v2:beta_1",
        )],
    );
    let root = dir.to_str().expect("utf8");
    assert_dropped_only_in_window(root);
    assert_dropped_only_tools(root);
    assert_dropped_only_outside(&dir, root);
    let _ = std::fs::remove_dir_all(&dir);
}

fn assert_dropped_only_in_window(root: &str) {
    let base = [
        "models",
        "--otlp-root",
        root,
        "--agent",
        "otlp",
        "--since",
        SINCE,
    ];
    let (code, text, err) = run(&base);
    assert_eq!(code, 0, "{err}");
    assert!(!text.contains("Nothing scanned"), "{text}");
    assert!(text.contains("otlp non inference spans: 1"), "{text}");
    assert!(text.contains("custom.chat-v2:beta_1"), "{text}");

    let (code, json_out, err) = run(&[&base[..], &["--json"]].concat());
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(&json_out).expect("json");
    assert_eq!(doc["records_scanned"], json!(true));
    assert_eq!(
        cells(&doc["roots"][0], &["files_with_records", "lines_skipped"]),
        json!([1, 0])
    );
    let s = &doc["detectors"]["models"]["summary"];
    assert_eq!(
        cells(s, &["replies", "otlp_non_inference_spans"]),
        json!([0, 1])
    );
    assert_eq!(
        doc["detectors"]["models"]["tables"][1]["rows"],
        json!([{"operation": "custom.chat-v2:beta_1", "spans": 1}])
    );

    let (code, csv_out, err) = run(&[&base[..], &["--csv"]].concat());
    assert_eq!(code, 0, "{err}");
    assert!(
        csv_out.contains("models,summary,,,otlp_non_inference_spans,1\r\n"),
        "{csv_out}"
    );
    assert!(
        csv_out.contains("models,cell,1,0,operation,\"\"\"custom.chat-v2:beta_1\"\"\"\r\n"),
        "{csv_out}"
    );
    assert!(csv_out.contains("models,cell,1,0,spans,1\r\n"), "{csv_out}");
}

fn assert_dropped_only_tools(root: &str) {
    let (code, tools_out, err) = run(&[
        "tools",
        "--otlp-root",
        root,
        "--agent",
        "otlp",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{err}");
    let tools_doc: Value = serde_json::from_str(&tools_out).expect("tools json");
    assert_eq!(tools_doc["records_scanned"], json!(true));
    assert_eq!(tools_doc["roots"][0]["files_with_records"], json!(1));
}

fn assert_dropped_only_outside(dir: &Path, root: &str) {
    // Rewrite with a pre-window span timestamp (file mtime stays >= SINCE so the file is read).
    write_export(
        dir,
        "only.json",
        vec![dropped(
            Some("t0"),
            Some("d0"),
            Some(BEFORE_WIN),
            "custom.chat-v2:beta_1",
        )],
    );
    let base = [
        "models",
        "--otlp-root",
        root,
        "--agent",
        "otlp",
        "--since",
        SINCE,
    ];
    let (code, old_text, err) = run(&base);
    assert_eq!(code, 0, "{err}");
    assert!(old_text.contains("Nothing scanned"), "{old_text}");
    let (code, old_json, err) = run(&[&base[..], &["--json"]].concat());
    assert_eq!(code, 0, "{err}");
    let old_doc: Value = serde_json::from_str(&old_json).expect("old json");
    assert_eq!(old_doc["records_scanned"], json!(false));
    assert_eq!(
        cells(&old_doc["roots"][0], &["files", "files_with_records"]),
        json!([1, 0])
    );
    assert_eq!(
        old_doc["detectors"]["models"]["summary"]["otlp_non_inference_spans"],
        json!(0)
    );
    assert_eq!(
        old_doc["detectors"]["models"]["tables"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

fn edge_tool_and_non_string_spans() -> Vec<Value> {
    vec![
        dropped(Some("t1"), Some("mt1"), Some(IN_WIN), "execute_tool"),
        otlp_span(
            Some("t1"),
            Some("mt2"),
            Some(IN_WIN),
            vec![kv(
                "gen_ai.operation.name",
                json!({"stringValue": "execute_tool"}),
            )],
        ),
        otlp_span(
            Some("t1"),
            Some("vt1"),
            Some(IN_WIN),
            vec![
                kv(
                    "gen_ai.operation.name",
                    json!({"stringValue": "execute_tool"}),
                ),
                kv("gen_ai.tool.name", json!({"stringValue": "search"})),
                kv(
                    "gen_ai.request.model",
                    json!({"stringValue": "claude-sonnet-4-5-20250929"}),
                ),
                kv("gen_ai.usage.input_tokens", json!({"intValue": "10"})),
            ],
        ),
        dropped(Some("t1"), Some("notime"), None, "embeddings"),
        otlp_span(
            Some("t1"),
            Some("non_str"),
            Some(IN_WIN),
            model_usage(json!({"intValue": "7"})),
        ),
    ]
}

fn write_edge_exports(dir: &Path) {
    let long_op = "a".repeat(129);
    let mut a = vec![
        dropped(Some("t1"), Some("e1"), Some(IN_WIN), "embeddings"),
        dropped(Some("t1"), Some("e2"), Some(IN_WIN), "embeddings"),
        dropped(None, Some("a1"), Some(IN_WIN), "invoke_agent"),
        dropped(Some("t1"), Some(""), Some(IN_WIN), "zebra"),
        dropped(Some("t1"), Some("p1"), Some(IN_WIN), ""),
        dropped(Some("t1"), Some("p2"), Some(IN_WIN), "bad op SENTINEL"),
        dropped(Some("t1"), Some("p3"), Some(IN_WIN), "chat_é"),
        dropped(Some("t1"), Some("p4"), Some(IN_WIN), &long_op),
    ];
    a.extend(edge_tool_and_non_string_spans());
    write_export(dir, "a.json", a);
    write_export(
        dir,
        "b.json",
        vec![
            dropped(Some("t1"), Some("e1"), Some(IN_WIN), "embeddings"),
            dropped(Some(""), Some("a1"), Some(IN_WIN), "invoke_agent"),
            dropped(Some("t1"), None, Some(IN_WIN), "zebra"),
        ],
    );
}

#[test]
fn dropped_genai_spans_respect_filter_dedup_privacy_limit_and_table_order() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("otlp-dropped-edges");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("edge dir");
    write_edge_exports(&dir);
    let root = dir.to_str().expect("utf8");

    for flag in [[].as_slice(), &["--json"], &["--csv"]] {
        let mut args = vec![
            "models",
            "--otlp-root",
            root,
            "--agent",
            "otlp",
            "--since",
            SINCE,
        ];
        args.extend_from_slice(flag);
        let (code, out, err) = run(&args);
        assert_eq!(code, 0, "{err}");
        assert!(!out.contains("SENTINEL"), "{out}");
    }

    assert_edge_tables(root);
    assert_edge_limit(root);
    let _ = std::fs::remove_dir_all(&dir);
}

fn assert_edge_tables(root: &str) {
    let (code, out, err) = run(&[
        "models",
        "--otlp-root",
        root,
        "--agent",
        "otlp",
        "--since",
        SINCE,
        "--by-branch",
        "--by-day",
        "--json",
    ]);
    assert_eq!(code, 0, "{err}");
    let doc: Value = serde_json::from_str(&out).expect("edge json");
    assert_eq!(doc["roots"][0]["lines_skipped"], json!(0));
    let s = &doc["detectors"]["models"]["summary"];
    assert_eq!(
        cells(s, &["replies", "otlp_non_inference_spans"]),
        json!([1, 11])
    );
    let tables = doc["detectors"]["models"]["tables"]
        .as_array()
        .expect("tables");
    assert_eq!(tables.len(), 4, "{tables:?}");
    assert!(tables[1]["rows"][0].get("branch").is_some());
    assert!(tables[2]["rows"][0].get("day").is_some());
    let got: Vec<Value> = tables[3]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|r| cells(r, &["operation", "spans"]))
        .collect();
    assert_eq!(
        got,
        [
            json!(["(other)", 4]),
            json!(["embeddings", 2]),
            json!(["invoke_agent", 2]),
            json!(["zebra", 2]),
            json!(["execute_tool", 1]),
        ]
    );
}

fn assert_edge_limit(root: &str) {
    let (code, lim_out, err) = run(&[
        "models",
        "--otlp-root",
        root,
        "--agent",
        "otlp",
        "--since",
        SINCE,
        "--limit",
        "2",
        "--json",
    ]);
    assert_eq!(code, 0, "{err}");
    let lim_doc: Value = serde_json::from_str(&lim_out).expect("lim json");
    assert_eq!(
        lim_doc["detectors"]["models"]["summary"]["otlp_non_inference_spans"],
        json!(11)
    );
    assert_eq!(
        lim_doc["detectors"]["models"]["tables"][1]["omitted"],
        json!(3)
    );
    assert_eq!(
        lim_doc["detectors"]["models"]["tables"][1]["rows"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
}
