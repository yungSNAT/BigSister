use super::*;

fn records(line: &str) -> (Vec<Value>, u64) {
    let mut out = Vec::new();
    let bad = Otlp::default().read(line, &mut out);
    let parsed = out
        .iter()
        .map(|l| serde_json::from_str(l).expect("JSON"))
        .collect();
    (parsed, bad)
}

/// One export holding `spans`, each a JSON object.
fn export(spans: &[Value]) -> String {
    json!({"resourceSpans": [{"scopeSpans": [{"spans": spans}]}]}).to_string()
}

fn attr(key: &str, value: Value) -> Value {
    json!({"key": key, "value": value})
}

fn inference(id: &str, end: &str, attrs: Vec<Value>) -> Value {
    json!({"traceId": "t1", "spanId": id, "endTimeUnixNano": end, "attributes": attrs})
}

#[test]
fn cache_tokens_are_taken_out_of_the_input_they_are_counted_in() {
    let span = inference(
        "s1",
        "1763600000000000000",
        vec![
            attr("gen_ai.request.model", json!({"stringValue": "m-req"})),
            attr("gen_ai.response.model", json!({"stringValue": "m-resp"})),
            attr("gen_ai.usage.input_tokens", json!({"intValue": "1000"})),
            attr(
                "gen_ai.usage.cache_read.input_tokens",
                json!({"intValue": 600}),
            ),
            attr(
                "gen_ai.usage.cache_creation.input_tokens",
                json!({"intValue": "100"}),
            ),
            attr("gen_ai.usage.output_tokens", json!({"intValue": "50"})),
        ],
    );
    let (out, bad) = records(&export(&[span]));
    assert_eq!(bad, 0);
    let r = &out[0];
    assert_eq!(r["type"], "assistant");
    assert_eq!(r["sessionId"], "t1");
    assert_eq!(r["message"]["id"], "t1/s1");
    assert_eq!(r["message"]["model"], "m-resp");
    assert_eq!(r["timestamp"], "2025-11-20T00:53:20Z");
    assert_eq!(
        r["message"]["usage"],
        json!({"input_tokens": 300, "cache_read_input_tokens": 600,
               "cache_creation_input_tokens": 100, "output_tokens": 50})
    );
}

#[test]
fn the_current_cache_write_key_wins_and_the_conversation_names_the_session() {
    let span = inference(
        "s1",
        "1",
        vec![
            attr("gen_ai.request.model", json!({"stringValue": "m"})),
            attr("gen_ai.conversation.id", json!({"stringValue": "conv"})),
            attr("gen_ai.usage.input_tokens", json!({"intValue": "100"})),
            attr(
                "gen_ai.usage.cache_write.input_tokens",
                json!({"intValue": "40"}),
            ),
            attr(
                "gen_ai.usage.cache_creation.input_tokens",
                json!({"intValue": "90"}),
            ),
        ],
    );
    let (out, _) = records(&export(&[span]));
    assert_eq!(out[0]["sessionId"], "conv");
    assert_eq!(out[0]["message"]["model"], "m");
    assert_eq!(
        out[0]["message"]["usage"],
        json!({"input_tokens": 60, "cache_read_input_tokens": 0,
               "cache_creation_input_tokens": 40})
    );
}

#[test]
fn input_is_left_out_when_missing_or_smaller_than_its_cache_tokens() {
    let model = attr("gen_ai.request.model", json!({"stringValue": "m"}));
    let over = inference(
        "a",
        "1",
        vec![
            model.clone(),
            attr("gen_ai.usage.input_tokens", json!({"intValue": "10"})),
            attr(
                "gen_ai.usage.cache_read.input_tokens",
                json!({"intValue": "600"}),
            ),
            attr("gen_ai.usage.output_tokens", json!({"intValue": "5"})),
        ],
    );
    let output_only = inference(
        "b",
        "2",
        vec![
            model,
            attr("gen_ai.usage.output_tokens", json!({"intValue": "5"})),
        ],
    );
    let (out, bad) = records(&export(&[over, output_only]));
    assert_eq!(bad, 0);
    assert_eq!(
        out[0]["message"]["usage"],
        json!({"cache_read_input_tokens": 600, "cache_creation_input_tokens": 0,
               "output_tokens": 5})
    );
    assert_eq!(
        out[1]["message"]["usage"],
        json!({"cache_read_input_tokens": 0, "cache_creation_input_tokens": 0,
               "output_tokens": 5})
    );
}

#[test]
fn a_tool_span_is_a_call_and_its_result_with_no_content() {
    let ok = json!({"traceId": "t", "spanId": "x1", "endTimeUnixNano": "20",
    "attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
        attr("gen_ai.tool.name", json!({"stringValue": "search"})),
        attr("gen_ai.tool.call.id", json!({"stringValue": "call-9"})),
        attr("gen_ai.tool.call.arguments", json!({"stringValue": "SENTINEL"})),
    ]});
    let failed = json!({"traceId": "t", "spanId": "x2", "endTimeUnixNano": "10",
    "status": {"code": 2},
    "attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
        attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
    ]});
    let typed = json!({"traceId": "t", "spanId": "x3", "endTimeUnixNano": "30",
    "attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
        attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
        attr("error.type", json!({"stringValue": "timeout"})),
    ]});
    let mut out = Vec::new();
    assert_eq!(
        Otlp::default().read(&export(&[ok, failed, typed]), &mut out),
        0
    );
    assert!(!out.concat().contains("SENTINEL"));
    let out: Vec<Value> = out
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(out.len(), 6);
    // In end-time order: x2, x1, x3.
    let call = &out[0]["message"]["content"][0];
    assert_eq!(call["id"], "t/x2");
    assert_eq!(call["name"], "fetch");
    assert_eq!(call["input"], json!({}));
    assert_eq!(out[0]["message"]["model"], "<synthetic>");
    let result = &out[1]["message"]["content"][0];
    assert_eq!(result["tool_use_id"], "t/x2");
    assert_eq!(result["is_error"], true);
    // The trace and span ids, as a call id need not be unique across traces.
    assert_eq!(out[2]["message"]["content"][0]["id"], "t/x1");
    assert_eq!(out[3]["message"]["content"][0]["is_error"], false);
    assert_eq!(out[5]["message"]["content"][0]["is_error"], true);
}

#[test]
fn other_spans_are_ignored_and_broken_ones_counted() {
    let http = json!({"spanId": "h", "endTimeUnixNano": "1",
        "attributes": [attr("http.method", json!({"stringValue": "GET"}))]});
    let no_id = inference(
        "",
        "1",
        vec![
            attr("gen_ai.request.model", json!({"stringValue": "m"})),
            attr("gen_ai.usage.input_tokens", json!({"intValue": "1"})),
        ],
    );
    let no_time = json!({"spanId": "z", "attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
        attr("gen_ai.tool.name", json!({"stringValue": "t"})),
    ]});
    let (out, bad) = records(&export(&[http, no_id, no_time]));
    assert!(out.is_empty());
    assert_eq!(bad, 2);
    assert_eq!(records("not JSON").1, 1);
    assert_eq!(records("{\"resourceLogs\":[]}").1, 1);
    assert_eq!(records("  ").1, 0);
}

#[test]
fn deprecated_token_names_are_read_when_the_current_ones_are_absent() {
    let span = inference(
        "d",
        "1",
        vec![
            attr("gen_ai.request.model", json!({"stringValue": "m"})),
            attr("gen_ai.usage.prompt_tokens", json!({"intValue": "70"})),
            attr("gen_ai.usage.completion_tokens", json!({"intValue": "7"})),
        ],
    );
    let (out, bad) = records(&export(&[span]));
    assert_eq!(bad, 0);
    assert_eq!(
        out[0]["message"]["usage"],
        json!({"input_tokens": 70, "cache_read_input_tokens": 0,
               "cache_creation_input_tokens": 0, "output_tokens": 7})
    );
}

#[test]
fn a_span_written_again_counts_once_and_calls_sharing_an_id_stay_apart() {
    let call = |id: &str| {
        json!({"traceId": format!("t-{id}"), "spanId": id, "endTimeUnixNano": "5",
        "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
            attr("gen_ai.tool.call.id", json!({"stringValue": "call_0"})),
        ]})
    };
    let line = export(&[call("p"), call("q")]);
    let mut file = Otlp::default();
    let mut out = Vec::new();
    assert_eq!(file.read(&line, &mut out), 0);
    assert_eq!(file.read(&line, &mut out), 0);
    let ids: Vec<Value> = out
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).expect("JSON"))
        .map(|v| v["message"]["content"][0].clone())
        .map(|b| {
            b.get("id")
                .or_else(|| b.get("tool_use_id"))
                .cloned()
                .unwrap()
        })
        .collect();
    assert_eq!(ids, ["t-p/p", "t-p/p", "t-q/q", "t-q/q"]);
}

#[test]
fn an_incomplete_span_does_not_hide_the_first_complete_copy() {
    let complete = inference(
        "s",
        "1",
        vec![
            attr("gen_ai.request.model", json!({"stringValue": "m"})),
            attr("gen_ai.usage.input_tokens", json!({"intValue": "7"})),
        ],
    );
    let mut incomplete = complete.clone();
    incomplete["endTimeUnixNano"] = Value::Null;
    let mut file = Otlp::default();
    let mut out = Vec::new();
    assert_eq!(file.read(&export(&[incomplete]), &mut out), 1);
    assert_eq!(
        file.read(&export(&[complete.clone(), complete]), &mut out),
        0
    );
    assert_eq!(out.len(), 1);
}

#[test]
fn an_end_time_reads_as_a_string_or_a_number() {
    assert_eq!(nanos(&json!("20")), Some(20));
    assert_eq!(nanos(&json!(20)), Some(20));
    assert_eq!(nanos(&json!(-1)), None);
}

#[test]
fn spans_sharing_a_span_id_across_traces_stay_apart() {
    let call = |trace: &str, code: u64| {
        json!({"traceId": trace, "spanId": "s", "endTimeUnixNano": "5",
        "status": {"code": code},
        "attributes": [
            attr("gen_ai.operation.name", json!({"stringValue": "execute_tool"})),
            attr("gen_ai.tool.name", json!({"stringValue": "fetch"})),
        ]})
    };
    let (out, bad) = records(&export(&[call("t1", 0), call("t2", 2)]));
    assert_eq!(bad, 0);
    let blocks: Vec<&Value> = out.iter().map(|v| &v["message"]["content"][0]).collect();
    assert_eq!(blocks[0]["id"], "t1/s");
    assert_eq!(blocks[1]["tool_use_id"], "t1/s");
    assert_eq!(blocks[1]["is_error"], false);
    assert_eq!(blocks[2]["id"], "t2/s");
    assert_eq!(blocks[3]["tool_use_id"], "t2/s");
    assert_eq!(blocks[3]["is_error"], true);
}

fn dropped_span(trace: Option<&str>, id: Option<&str>, end: Option<&str>, op: &str) -> Value {
    let mut s = json!({"attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": op})),
        attr("gen_ai.request.model", json!({"stringValue": "m"})),
        attr("gen_ai.usage.input_tokens", json!({"intValue": "10"})),
    ]});
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

#[test]
fn non_inference_genai_spans_emit_dropped_records_and_require_both_ids_to_dedup() {
    let no_usage = json!({"traceId": "t1", "spanId": "u0", "endTimeUnixNano": "5",
    "attributes": [
        attr("gen_ai.operation.name", json!({"stringValue": "retrieval"})),
        attr("gen_ai.request.model", json!({"stringValue": "m"})),
    ]});
    let line = export(&[
        dropped_span(Some("t1"), Some("s1"), None, "embeddings"),
        dropped_span(Some("t1"), Some("s1"), Some("20"), "embeddings"),
        dropped_span(Some("t1"), Some("s1"), Some("20"), "embeddings"),
        dropped_span(Some(""), Some("s2"), Some("10"), "invoke_agent"),
        dropped_span(Some(""), Some("s2"), Some("10"), "invoke_agent"),
        dropped_span(Some("t2"), Some(""), Some("15"), "custom_op"),
        dropped_span(Some("t2"), Some(""), Some("15"), "custom_op"),
        no_usage,
    ]);
    let (out, bad) = records(&line);
    assert_eq!(bad, 0);
    let got: Vec<(&str, Option<&str>, &str)> = out
        .iter()
        .map(|r| {
            (
                r["type"].as_str().unwrap(),
                r["id"].as_str(),
                r["operation"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        [
            ("otlp_dropped", None, "invoke_agent"),
            ("otlp_dropped", None, "invoke_agent"),
            ("otlp_dropped", None, "custom_op"),
            ("otlp_dropped", None, "custom_op"),
            ("otlp_dropped", Some("t1/s1"), "embeddings"),
        ]
    );
}
