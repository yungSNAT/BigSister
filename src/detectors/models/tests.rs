use super::*;

#[test]
fn days_list_latest_first_and_the_limit_keeps_the_latest() {
    let mut m = models();
    for d in ["2026-11-02", NONE, "2026-11-09", "2026-11-05"] {
        m.days.insert(d.to_owned(), Group::default());
    }
    let opts = Opts {
        limit: 2,
        paths: false,
        by_branch: false,
        by_day: true,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    let t = m.day_table(&opts);
    let days: Vec<&Value> = t.rows.iter().map(|r| &r[0]).collect();
    assert_eq!(days, [&json!("2026-11-09"), &json!("2026-11-05")]);
    assert_eq!(t.omitted, 2);
    for record in [
        json!({}),
        json!({"timestamp": "yesterday"}),
        json!({"timestamp": 7}),
    ] {
        assert_eq!(local_day(&record), NONE, "{record}");
    }
}

#[test]
fn a_branch_prints_as_its_slash_separated_names_or_other() {
    assert_eq!(printed_branch("ryan/fix-1.2"), "ryan/fix-1.2");
    for odd in ["a//b", "/a", "a b", "a/\u{1b}[31m", &"x".repeat(129)] {
        assert_eq!(printed_branch(odd), "(other)", "{odd}");
    }
}

/// A reply of session `s` on model `m` with `input` uncached input tokens, 1 output
/// token and no cache use.
fn reply(id: &str, input: &Value) -> Value {
    json!({"type": "assistant", "timestamp": "2026-11-08T01:00:00Z", "sessionId": "s",
               "message": {"model": "m", "id": id, "usage": {
                   "input_tokens": input, "output_tokens": 1,
                   "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                   "cache_creation": {"ephemeral_5m_input_tokens": 0,
                                      "ephemeral_1h_input_tokens": 0}}}})
}

const SPLIT: &[&str] = &["message", "usage", "cache_creation"];

/// Each field a reply reads, as its path from the record, and values of other types.
fn fields() -> Vec<(&'static [&'static str], Vec<Value>)> {
    let count = |wrong: Value| vec![wrong, json!(1.0)];
    vec![
        (&["message"], vec![json!("m")]),
        (&["message", "id"], vec![json!(7)]),
        (&["message", "model"], vec![json!(7)]),
        (&SPLIT[..2], vec![json!(7)]),
        (&["message", "usage", "input_tokens"], count(json!(-1))),
        (&["message", "usage", "output_tokens"], count(json!(1.5))),
        (
            &["message", "usage", "cache_creation_input_tokens"],
            count(json!("1")),
        ),
        (
            &["message", "usage", "cache_read_input_tokens"],
            count(json!(true)),
        ),
        (SPLIT, vec![json!([])]),
        (
            &[
                "message",
                "usage",
                "cache_creation",
                "ephemeral_5m_input_tokens",
            ],
            count(json!(-1)),
        ),
        (
            &[
                "message",
                "usage",
                "cache_creation",
                "ephemeral_1h_input_tokens",
            ],
            count(json!("1")),
        ),
        (&["sessionId"], vec![json!(7)]),
    ]
}

/// `record` with the field at `path` removed (`None`) or set to `value`.
fn with(record: &Value, path: &[&str], value: Option<&Value>) -> Value {
    let mut record = record.clone();
    let (key, parents) = path.split_last().expect("a path");
    let parent = parents.iter().fold(&mut record, |v, k| &mut v[*k]);
    let parent = parent.as_object_mut().expect("an object");
    match value {
        Some(v) => parent.insert((*key).to_owned(), v.clone()),
        None => parent.remove(*key),
    };
    record
}

/// Replies skipped and replies counted.
fn tally(m: &Models) -> (u64, u64) {
    (m.skipped, m.groups.values().map(|g| g.replies).sum())
}

#[test]
fn a_reply_with_a_field_missing_null_or_mistyped_is_skipped() {
    let mut b = reply("b", &json!(10));
    b["effort"] = json!("high");
    b["attributionAgent"] = json!("worker");
    let a = (reply("a", &json!(10)), true);
    let c = (reply("c", &json!(10)), true);
    for (path, wrong) in fields() {
        // A usage may lack the split, as `cache` reads it.
        let missing = (path != SPLIT).then_some(None);
        let values = missing.into_iter().chain([Some(&Value::Null)]);
        for value in values.chain(wrong.iter().map(Some)) {
            let b = with(&b, path, value);
            let case = format!("{path:?} {value:?}");
            assert_eq!(read_reply(&b).err(), path.last().copied(), "{case}");
            let m = run(&[a.clone(), (b, true), c.clone()]);
            assert_eq!(tally(&m), (1, 2), "{case}");
        }
    }
    let m = run(&[(with(&b, SPLIT, None), true)]);
    assert_eq!(tally(&m), (0, 1), "no split");
    // A label a reply may lack: missing is (none); null or another type is skipped.
    for key in ["effort", "attributionAgent"] {
        let m = run(&[(with(&b, &[key], None), true)]);
        assert_eq!(tally(&m), (0, 1), "{key} missing");
        assert!(m.groups.keys().any(|k| k.2 == NONE || k.3 == NONE), "{key}");
        for value in [Value::Null, json!(7)] {
            let m = run(&[(with(&b, &[key], Some(&value)), true)]);
            assert_eq!(tally(&m), (1, 0), "{key} {value}");
        }
    }
}

/// The section with model `m` at US$5 per million input tokens.
fn models() -> Models {
    let prices =
        r#"{"m": {"input_cost_per_token": 5e-06}, "cheap": {"input_cost_per_token": 1e-06}}"#;
    Models::new(Rc::new(Prices::from_file(prices, "test").expect("prices")))
}

/// The detector after `records` of one main session, each in the window or not.
fn run(records: &[(Value, bool)]) -> Models {
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut m = models();
    m.begin(&s);
    for (v, in_window) in records {
        m.record(
            &s,
            &Record {
                value: v,
                in_window: *in_window,
            },
        );
    }
    m.end(&s);
    m
}

#[test]
fn a_reply_is_skipped_once_when_no_record_of_it_has_readable_usage() {
    let (good, bad) = (json!(10), json!("x"));
    let skipped = |records: &[(&str, &Value, bool)]| {
        let records: Vec<_> = records
            .iter()
            .map(|(id, input, in_window)| (reply(id, input), *in_window))
            .collect();
        let m = run(&records);
        (m.skipped, m.groups.values().map(|g| g.replies).sum::<u64>())
    };
    // 2 unreadable blocks of one reply skip it once.
    assert_eq!(skipped(&[("a", &bad, true), ("a", &bad, true)]), (1, 0));
    // A later readable block of the reply counts it, not skipped.
    assert_eq!(skipped(&[("a", &bad, true), ("a", &good, true)]), (0, 1));
    // An unreadable block of a reply already counted adds nothing.
    assert_eq!(skipped(&[("a", &good, true), ("a", &bad, true)]), (0, 1));
    // An unreadable reply superseded by another reply, and one outside the window.
    assert_eq!(
        skipped(&[("a", &bad, true), ("b", &good, true), ("c", &bad, false)]),
        (1, 1)
    );
}

#[test]
fn a_reply_without_a_string_model_is_skipped_and_a_skipped_reply_stays_skipped() {
    let (good, bad) = (json!(10), json!("x"));
    let mut no_model = reply("b", &good);
    no_model["message"]
        .as_object_mut()
        .expect("message")
        .remove("model");
    let m = run(&[(reply("a", &good), true), (no_model, true)]);
    assert_eq!(m.skipped, 1);
    // a's later block, after b superseded it, is not counted as well.
    let m = run(&[
        (reply("a", &bad), true),
        (reply("b", &bad), true),
        (reply("a", &good), true),
    ]);
    assert_eq!(m.skipped, 2);
    assert!(m.groups.is_empty());
}

/// A record at a price, in the window or not.
type At = (Value, Option<f64>, bool);

/// Feeds one main or subagent file of `records` to `m`.
fn file(m: &mut Models, subagent: bool, records: &[At]) {
    let s = Session {
        agent: Agent::Claude,
        subagent,
    };
    m.begin(&s);
    for (v, price, in_window) in records {
        m.read_by(&s, *price, *in_window);
        let r = Record {
            value: v,
            in_window: *in_window,
        };
        m.record(&s, &r);
    }
    m.end(&s);
}

/// The ranks of subagent files `subs` after the main file `parent`.
fn ranks(parent: &[At], subs: &[&[At]]) -> Vec<&'static str> {
    let mut m = models();
    file(&mut m, false, parent);
    for sub in subs {
        file(&mut m, true, sub);
    }
    let rank = |sub| match m.rank(sub) {
        Rank::OnTop => "on top",
        Rank::Below => "below",
        Rank::Unknown => "indeterminate",
    };
    m.subs.iter().map(rank).collect()
}

#[test]
fn a_skipped_reply_leaves_its_subagent_session_indeterminate() {
    let (good, bad) = (json!(10), json!("x"));
    let top = || (reply("p", &good), Some(5.0), true);
    let on = || (reply("a", &good), Some(5.0), true);
    let skipped = |id| (reply(id, &bad), Some(9.0), true);
    assert_eq!(ranks(&[top()], &[&[on()]]), ["on top"]);
    // A subagent reply on a cheaper model ranks the session below its parent.
    let mut cheaper = reply("c", &good);
    cheaper["message"]["model"] = json!("cheap");
    assert_eq!(
        ranks(&[top()], &[&[on(), (cheaper, Some(1.0), true)]]),
        ["below"]
    );
    // The subagent's skipped reply may have been on a cheaper model.
    assert_eq!(ranks(&[top()], &[&[on(), skipped("b")]]), ["indeterminate"]);
    // A subagent session whose every reply is skipped is still a session.
    assert_eq!(ranks(&[top()], &[&[skipped("b")]]), ["indeterminate"]);
    // The parent's skipped reply may have been on a dearer model.
    assert_eq!(ranks(&[top(), skipped("q")], &[&[on()]]), ["indeterminate"]);
    // A reply skipped outside the window, or whose later block reads, changes nothing.
    let (mut early, recovered) = (skipped("q"), (reply("q", &good), Some(5.0), true));
    early.2 = false;
    assert_eq!(ranks(&[early, top()], &[&[on()]]), ["on top"]);
    assert_eq!(
        ranks(&[skipped("q"), recovered, top()], &[&[on()]]),
        ["on top"]
    );
}

/// A reply of `session` at US$5 in the window.
fn at(id: &str, session: &str, input: &Value) -> At {
    let mut v = reply(id, input);
    v["sessionId"] = json!(session);
    (v, Some(5.0), true)
}

#[test]
fn a_parent_s_skipped_reply_affects_only_its_own_session() {
    let (good, bad) = (json!(10), json!("x"));
    // One main file holds sessions s and t; t's skipped reply leaves only t's subagent
    // indeterminate, and a skipped reply without a session id leaves both.
    let sub = |session| [at("a", session, &good)];
    let (s, t) = (sub("s"), sub("t"));
    let parent = [at("p", "s", &good), at("q", "t", &good)];
    assert_eq!(ranks(&parent, &[&s, &t]), ["on top", "on top"]);
    let mut with_skip = parent.to_vec();
    with_skip.push(at("r", "t", &bad));
    assert_eq!(ranks(&with_skip, &[&s, &t]), ["on top", "indeterminate"]);
    let mut no_session = at("r", "t", &bad);
    no_session
        .0
        .as_object_mut()
        .expect("record")
        .remove("sessionId");
    let mut with_skip = parent.to_vec();
    with_skip.push(no_session);
    assert_eq!(
        ranks(&with_skip, &[&s, &t]),
        ["indeterminate", "indeterminate"]
    );
    // Unreadable blocks of one reply that name different sessions leave both unsure.
    let mut with_skip = parent.to_vec();
    with_skip.extend([at("r", "s", &bad), at("r", "t", &bad)]);
    assert_eq!(
        ranks(&with_skip, &[&s, &t]),
        ["indeterminate", "indeterminate"]
    );
}

#[test]
fn an_unpriced_parent_reply_leaves_its_subagents_indeterminate() {
    let good = json!(10);
    // The unpriced reply's model may be dearer than the priced one's.
    let mut unpriced = (reply("q", &good), None, true);
    unpriced.0["message"]["model"] = json!("unpriced");
    let parent = [(reply("p", &good), Some(5.0), true), unpriced];
    let sub = [(reply("a", &good), Some(5.0), true)];
    assert_eq!(ranks(&parent, &[&sub]), ["indeterminate"]);
}

#[test]
fn a_skipped_reply_alone_in_a_main_file_still_marks_the_sessions_it_names() {
    let (good, bad) = (json!(10), json!("x"));
    let mut m = models();
    file(&mut m, false, &[at("p", "s", &good), at("q", "t", &good)]);
    // Another main file holds only one reply's unreadable blocks, naming s and t.
    file(&mut m, false, &[at("r", "s", &bad), at("r", "t", &bad)]);
    for session in ["s", "t"] {
        file(&mut m, true, &[at("a", session, &good)]);
    }
    let unknown: Vec<bool> = m
        .subs
        .iter()
        .map(|sub| matches!(m.rank(sub), Rank::Unknown))
        .collect();
    assert_eq!(unknown, [true, true]);
}
