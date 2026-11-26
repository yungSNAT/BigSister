
use super::*;

fn value(s: &str) -> Value {
    serde_json::from_str(s).expect("valid JSON")
}

/// A file holding shape `s` and the requests `lines`, each in the window.
fn file_of(lines: &[&str]) -> File {
    let mut f = File::default();
    f.shapes.insert("s".to_owned(), 0);
    f.shape_list
        .push(shape_parts(&value(r#"{"system":[],"tools":[]}"#)));
    for line in lines {
        add_request(&mut f, &value(line), true);
    }
    f
}

fn messages(f: &File, i: usize) -> Vec<u32> {
    let mut out = Vec::new();
    f.each_message(i, |m| out.push(m));
    out
}

/// Each request's estimated tokens add up to its recorded tokens; a tie in remainders
/// gives the token to the earlier part.
#[test]
fn apportion_splits_by_largest_remainder() {
    assert_eq!(apportion(300, &[37, 153, 37]), [49, 202, 49]);
    assert_eq!(apportion(2, &[1, 1, 1]), [1, 1, 0]);
    assert_eq!(apportion(5, &[0, 0]), [0, 0]);
    assert_eq!(apportion(0, &[3, 4]), [0, 0]);
}

/// Claude Code sends text content as one text block, and consecutive user records as
/// one message holding their blocks, up to `MAX_RUN` records; a reply ends a run.
#[test]
fn user_records_match_as_sent_alone_and_in_runs() {
    let role = json!("user");
    let text = |t: &str| json!({"type": "text", "text": t});
    let held =
        |f: &File, blocks: Vec<Value>| f.ids.contains_key(&hash_of(&role, &Value::Array(blocks)));
    let mut f = File::default();
    f.add_user(&role, &json!("a"));
    f.add_user(&role, &json!([text("b")]));
    assert!(held(&f, vec![text("a")]));
    assert!(held(&f, vec![text("a"), text("b")]));
    f.run = (Vec::new(), 0);
    f.add_user(&role, &json!([text("c")]));
    assert!(!held(&f, vec![text("a"), text("b"), text("c")]));
    for i in 1..=MAX_RUN {
        f.add_user(&role, &json!([text(&i.to_string())]));
    }
    let run: Vec<Value> = std::iter::once(text("c"))
        .chain((1..=MAX_RUN).map(|i| text(&i.to_string())))
        .collect();
    assert!(held(&f, run[..MAX_RUN].to_vec()));
    assert!(!held(&f, run), "a run longer than MAX_RUN is not matched");
}

/// Only a reply ends a run of user records: a user or assistant record without a
/// message object, and a record of another kind, leave it whole.
#[test]
fn only_a_reply_with_a_message_ends_a_run() {
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let user = |t: &str| {
        json!({"type": "user", "message": {"role": "user",
                                    "content": [{"type": "text", "text": t}]}})
    };
    let mut d = Context::default();
    d.begin(&s);
    for v in [
        user("a"),
        json!({"type": "user", "message": "a"}),
        json!({"type": "assistant", "message": null}),
        json!({"type": "api-request-other"}),
        user("b"),
    ] {
        d.record(
            &s,
            &Record {
                value: &v,
                in_window: true,
            },
        );
    }
    let blocks = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
    assert!(d.cur.ids.contains_key(&hash_of(&json!("user"), &blocks)));
}

/// A user record whose content is neither text nor blocks, or whose role is not
/// `user`, ends a run: the records either side are not one message.
#[test]
fn a_record_of_another_shape_or_role_ends_a_run() {
    let user = json!("user");
    let text = |t: &str| json!({"type": "text", "text": t});
    let held =
        |f: &File, blocks: Vec<Value>| f.ids.contains_key(&hash_of(&user, &Value::Array(blocks)));
    for (role, content) in [(json!("user"), json!(7)), (json!("system"), json!("x"))] {
        let mut f = File::default();
        f.add_user(&user, &json!([text("a")]));
        f.add_user(&role, &content);
        f.add_user(&user, &json!([text("b")]));
        assert!(held(&f, vec![text("b")]), "{role} {content}");
        assert!(!held(&f, vec![text("a"), text("b")]), "{role} {content}");
    }
    assert_eq!(message_parts(&user, &json!(7)), [0; PARTS.len()]);
}

/// A request keeps its base's first `keep` messages, all of them when `keep` is
/// larger, then adds its tail, through any number of generations.
#[test]
fn each_message_follows_bases_through_several_generations() {
    let h = |c: char| format!("\"{}\"", c.to_string().repeat(64));
    let (a, b, c, d, e, g) = (h('a'), h('b'), h('c'), h('d'), h('e'), h('f'));
    let f = file_of(&[
        &format!(r#"{{"id":"r1","keep":0,"tail":[{a},{b},{c}],"shapeHash":"s"}}"#),
        &format!(r#"{{"id":"r2","base":"r1","keep":2,"tail":[{d}],"shapeHash":"s"}}"#),
        &format!(r#"{{"id":"r3","base":"r2","keep":9,"tail":[{e}],"shapeHash":"s"}}"#),
        &format!(r#"{{"id":"r4","base":"r3","keep":0,"tail":[{g}],"shapeHash":"s"}}"#),
        r#"{"id":"r5","base":"r3","keep":1,"tail":[],"shapeHash":"s"}"#,
    ]);
    assert_eq!(f.not_rebuilt, 0);
    assert_eq!(messages(&f, 0), [0, 1, 2]);
    assert_eq!(messages(&f, 1), [0, 1, 3]);
    assert_eq!(messages(&f, 2), [0, 1, 3, 4]);
    assert_eq!(messages(&f, 3), [5]);
    assert_eq!(messages(&f, 4), [0]);
    assert_eq!((1..5).map(|i| f.len(i)).collect::<Vec<_>>(), [3, 4, 1, 1]);
}

/// A request whose `base`, `keep` or `tail` is missing where needed or of the wrong
/// type is not rebuilt, rather than read as a request with fewer messages.
#[test]
fn a_request_with_a_malformed_field_is_not_rebuilt() {
    let f = file_of(&[
        r#"{"id":"r0","keep":0,"tail":[],"shapeHash":"s"}"#,
        r#"{"id":"r1","base":5,"keep":0,"tail":[],"shapeHash":"s"}"#,
        r#"{"id":"r2","base":"r0","keep":"0","tail":[],"shapeHash":"s"}"#,
        r#"{"id":"r3","base":"r0","keep":-1,"tail":[],"shapeHash":"s"}"#,
        r#"{"id":"r4","base":"r0","tail":[],"shapeHash":"s"}"#,
        r#"{"id":"r5","keep":0,"tail":"x","shapeHash":"s"}"#,
        r#"{"id":"r6","keep":0,"shapeHash":"s"}"#,
        r#"{"id":"r7","keep":0,"tail":[1],"shapeHash":"s"}"#,
        r#"{"id":"r8","base":null,"keep":0,"tail":[],"shapeHash":"s"}"#,
    ]);
    assert_eq!(f.not_rebuilt, 7);
    assert_eq!(f.requests.len(), 2, "r0, and r8 with a null base");
}

/// The digest Python's `hashlib.sha256(json.dumps(m, separators=(",", ":"),
/// ensure_ascii=False, sort_keys=True).encode())` gives for the fixture's first prompt.
#[test]
fn a_message_hashes_as_sorted_compact_json() {
    let m: Value =
        serde_json::from_str(r#"{"role":"user","content":"Find the sentinel-prompt file."}"#)
            .expect("valid JSON");
    assert_eq!(
        hash_of(&m["role"], &m["content"]),
        hex32("f90cf44c749421a11b1900b7762b044b9477e3bb6bf90ad40849adf1a2884654").expect("hex")
    );
}

#[test]
fn hex32_takes_only_64_hex_digits() {
    assert_eq!(hex32(&"0".repeat(63)), None);
    assert_eq!(hex32(&"g".repeat(64)), None);
    assert_eq!(hex32(&"aB".repeat(32)), Some([0xab; 32]));
}

#[test]
fn each_block_counts_in_one_part() {
    let m: Value = serde_json::from_str(
        r#"{"role":"user","content":[
                {"type":"text","text":"  <system-reminder>r</system-reminder>"},
                {"type":"text","text":"hi"},
                {"type":"tool_result","tool_use_id":"t","content":"x"},
                {"type":"redacted_thinking","data":"abc"},
                {"type":"document","source":{}}]}"#,
    )
    .expect("valid JSON");
    let p = message_parts(&m["role"], &m["content"]);
    assert_eq!(p[REMINDERS], 38);
    assert_eq!(p[TEXT], 2);
    assert_eq!(p[TOOL_RESULTS], 54);
    assert_eq!(p[THINKING], 3);
    assert_eq!(p[OTHER_BLOCKS], 31);
    assert_eq!(p.iter().sum::<u64>(), 38 + 2 + 54 + 3 + 31);
}
