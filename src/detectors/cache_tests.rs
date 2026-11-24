use super::*;

/// A reply on model `m` whose usage is `usage` over zero counts and a zero split; a
/// `usage` that is not an object replaces it whole.
fn reply(id: &str, at: &str, usage: &Value) -> Value {
    let mut full = json!({"cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        "cache_creation": {"ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0}});
    match (full.as_object_mut(), usage.as_object()) {
        (Some(full), Some(given)) => full.extend(given.clone()),
        _ => full = usage.clone(),
    }
    json!({"type": "assistant", "timestamp": at,
           "message": {"model": "m", "id": id, "usage": full}})
}

#[test]
fn a_split_write_weighs_each_lifetime_and_an_overflowing_split_is_unreadable() {
    let split = |five: u64, hour: u64| {
        reply(
            "a",
            "2026-11-08T01:00:00Z",
            &json!({"cache_creation": {
            "ephemeral_5m_input_tokens": five, "ephemeral_1h_input_tokens": hour}}),
        )
    };
    let u = read_reply(&split(1000, 1000)).expect("a reply").usage;
    assert!((u.multiplier - 1.625).abs() < 1e-12, "{}", u.multiplier);
    assert!(u.one_hour);
    assert!(read_reply(&split(u64::MAX, 1)).is_err());
}

fn time(at: &str) -> Timestamp {
    parse_ts(at).expect("a time")
}

/// A reply on model `m` at `at` that read and wrote so many tokens, for `cause`.
fn read_and_wrote(at: &str, read: u64, write: u64) -> Reply<'static> {
    Reply {
        id: "b",
        model: "m",
        at: time(at),
        usage: Usage {
            uncached_input: 0,
            read,
            write,
            multiplier: WRITE_5M,
            one_hour: false,
            split: true,
        },
    }
}

#[test]
fn a_gap_a_millisecond_over_the_lifetime_is_idle() {
    let c = Cache {
        previous: Some(Previous {
            read: 0,
            write: 100,
            model: "m".to_owned(),
            at: time("2026-11-08T01:00:00.000Z"),
        }),
        ..Cache::default()
    };
    let cause = |at: &str| c.cause(&read_and_wrote(at, 0, 100)).0;
    assert!(cause("2026-11-08T01:05:00.001Z") == Cause::Idle);
    assert!(cause("2026-11-08T01:05:00.000Z") == Cause::Other);
}

/// The detector after `records` of one main session, each in the window or not.
fn run_with(records: &[(&Value, bool)]) -> Cache {
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut c = Cache::default();
    c.begin(&s);
    for &(value, in_window) in records {
        c.record(&s, &Record { value, in_window });
    }
    c.end(&s);
    c
}

/// The detector after `records` of one main session, all in the window.
fn run(records: &[Value]) -> Cache {
    run_with(&records.iter().map(|v| (v, true)).collect::<Vec<_>>())
}

/// The cache tokens by cause after `records` of one main session.
fn causes(records: &[Value]) -> [Option<u64>; 3] {
    let c = run(records);
    [Cause::First, Cause::New, Cause::Idle]
        .map(|cause| c.writes.get(&("main", cause)).map(|w| w.tokens))
}

#[test]
fn replies_with_no_readable_usage_are_counted_as_skipped() {
    let write = json!({"cache_creation_input_tokens": 100});
    let bad = json!({"cache_creation_input_tokens": "x"});
    let a = reply("a", "2026-11-08T01:00:00Z", &write);
    let b_bad = reply("b", "2026-11-08T03:00:00Z", &bad);
    let b_good = reply("b", "2026-11-08T03:00:00Z", &write);
    let c_bad = reply("c", "2026-11-08T05:00:00Z", &bad);
    let c = reply("c", "2026-11-08T05:00:00Z", &write);
    let skipped = |records: &[Value]| run(records).skipped;
    assert_eq!(skipped(&[a.clone(), b_bad.clone(), c.clone()]), 1);
    assert_eq!(skipped(&[a.clone(), b_bad.clone(), b_good, c.clone()]), 0);
    assert_eq!(skipped(&[a.clone(), b_bad.clone(), c_bad, c.clone()]), 1);
    // A second unreadable block of the same reply is the same skipped reply.
    assert_eq!(skipped(&[a.clone(), b_bad.clone(), b_bad.clone(), c]), 1);
    // Still unreadable when the session ends.
    assert_eq!(skipped(&[a.clone(), b_bad.clone()]), 1);
    // Outside the window, a skipped reply is not counted.
    let in_window = |bad: bool| run_with(&[(&b_bad, bad), (&a, true)]).skipped;
    assert_eq!((in_window(false), in_window(true)), (0, 1));
}

#[test]
fn a_reply_without_a_string_model_is_skipped_and_breaks_the_chain() {
    let write = json!({"cache_creation_input_tokens": 100});
    let a = reply("a", "2026-11-08T01:00:00Z", &write);
    let c = reply("c", "2026-11-08T01:01:00Z", &write);
    for model in [None, Some(json!(null)), Some(json!(7))] {
        let mut b = reply("b", "2026-11-08T01:00:30Z", &write);
        let message = b["message"].as_object_mut().expect("message");
        match model.clone() {
            Some(m) => message.insert("model".to_owned(), m),
            None => message.remove("model"),
        };
        let records = [a.clone(), b, c.clone()];
        assert_eq!(run(&records).skipped, 1, "{model:?}");
        // c's writes are new context, not a rebuild of what a cached.
        assert_eq!(causes(&records), [Some(100), Some(100), None], "{model:?}");
    }
}

/// Each field a reply reads, as its path from the record, and values of other types.
fn fields() -> Vec<(&'static [&'static str], Vec<Value>)> {
    vec![
        (&["message"], vec![json!("m")]),
        (&["message", "id"], vec![json!(7)]),
        (&["message", "model"], vec![json!(7)]),
        (&USAGE_SPLIT[..2], vec![json!(7)]),
        (
            &["message", "usage", "cache_creation_input_tokens"],
            vec![json!(-1), json!(1.0)],
        ),
        (
            &["message", "usage", "cache_read_input_tokens"],
            vec![json!(1.5), json!(1.0)],
        ),
        (USAGE_SPLIT, vec![json!([])]),
        (
            &[
                "message",
                "usage",
                "cache_creation",
                "ephemeral_5m_input_tokens",
            ],
            vec![json!("1"), json!(1.0)],
        ),
        (
            &[
                "message",
                "usage",
                "cache_creation",
                "ephemeral_1h_input_tokens",
            ],
            vec![json!(true), json!(1.0)],
        ),
        (&["timestamp"], vec![json!("yesterday"), json!(7)]),
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

#[test]
fn a_reply_with_a_field_missing_null_or_mistyped_is_skipped_and_breaks_the_chain() {
    let write = json!({"cache_creation_input_tokens": 100});
    let a = reply("a", "2026-11-08T01:00:00Z", &write);
    let b = reply("b", "2026-11-08T01:00:30Z", &write);
    let c = reply("c", "2026-11-08T01:01:00Z", &write);
    for (path, wrong) in fields() {
        // A reply without a split reads (a_reply_without_a_split_reads_at_the_5_minute_price).
        let missing = (path != USAGE_SPLIT).then_some(None);
        let values = missing.into_iter().chain([Some(&Value::Null)]);
        for value in values.chain(wrong.iter().map(Some)) {
            let records = [a.clone(), with(&b, path, value), c.clone()];
            let case = format!("{path:?} {value:?}");
            assert_eq!(
                read_reply(&records[1]).err(),
                path.last().copied(),
                "{case}"
            );
            let run = run(&records);
            assert_eq!(run.skipped, 1, "{case}");
            // b adds nothing, and c's writes are new context, not a rebuild.
            let total = run.writes.values().map(|w| w.tokens).sum::<u64>();
            assert_eq!(total, 200, "{case}");
            assert_eq!(causes(&records), [Some(100), Some(100), None], "{case}");
        }
    }
}

const USAGE_SPLIT: &[&str] = &["message", "usage", "cache_creation"];

#[test]
fn a_reply_without_a_split_reads_at_the_5_minute_price() {
    let write = json!({"cache_creation_input_tokens": 100});
    let a = reply("a", "2026-11-08T01:00:00Z", &write);
    let b = with(
        &reply("b", "2026-11-08T01:00:30Z", &write),
        USAGE_SPLIT,
        None,
    );
    let c = run(&[a.clone(), b.clone()]);
    assert_eq!((c.skipped, c.without_split), (0, 1));
    let rebuilt = c.writes[&("main", Cause::Other)];
    assert_eq!(rebuilt.tokens, 100);
    assert!(
        (rebuilt.weighted - 125.0).abs() < 1e-9,
        "{}",
        rebuilt.weighted
    );
    // Outside the window it is not counted.
    assert_eq!(run_with(&[(&a, true), (&b, false)]).without_split, 0);
}

#[test]
fn blocks_without_a_message_id_are_each_skipped() {
    let write = json!({"cache_creation_input_tokens": 100});
    let block = with(
        &reply("a", "2026-11-08T01:00:00Z", &write),
        &["message", "id"],
        None,
    );
    let c = run(&[block.clone(), block]);
    assert_eq!(c.skipped, 2);
    assert!(c.writes.is_empty());
}

#[test]
fn a_skipped_reply_s_later_blocks_are_not_counted() {
    let bad = json!({"cache_creation_input_tokens": "x"});
    let a_bad = reply("a", "2026-11-08T01:00:00Z", &bad);
    let b_bad = reply("b", "2026-11-08T01:00:10Z", &bad);
    let a = reply(
        "a",
        "2026-11-08T01:00:20Z",
        &json!({"cache_creation_input_tokens": 100}),
    );
    let c = run(&[a_bad, b_bad, a]);
    assert_eq!(c.skipped, 2);
    assert!(c.writes.is_empty());
}

#[test]
fn a_first_reply_after_a_compaction_is_new_context() {
    let compaction = json!({"type": "system", "subtype": "compact_boundary"});
    let a = reply(
        "a",
        "2026-11-08T01:00:00Z",
        &json!({"cache_creation_input_tokens": 100}),
    );
    assert_eq!(causes(&[compaction, a]), [None, Some(100), None]);
}

#[test]
fn an_unreadable_reply_makes_the_next_writes_new_context() {
    let write = json!({"cache_creation_input_tokens": 100});
    let bad = json!({"cache_creation_input_tokens": "x"});
    let a = reply("a", "2026-11-08T01:00:00Z", &write);
    let c = reply("c", "2026-11-08T05:00:00Z", &write);
    // b unreadable in every block: c's writes are new context.
    let b = reply("b", "2026-11-08T03:00:00Z", &bad);
    assert_eq!(
        causes(&[a.clone(), b, c.clone()]),
        [Some(100), Some(100), None]
    );
    // b unreadable, then readable in its next block: b rebuilds what a cached after 2
    // hours, and c what b cached.
    let b_bad = reply("b", "2026-11-08T03:00:00Z", &bad);
    let b_good = reply("b", "2026-11-08T03:00:00Z", &write);
    assert_eq!(causes(&[a, b_bad, b_good, c]), [Some(100), None, Some(200)]);
}

#[test]
fn after_an_unreadable_first_reply_the_writes_are_new_context() {
    let a_bad = reply(
        "a",
        "2026-11-08T01:00:00Z",
        &json!({"cache_creation_input_tokens": "x"}),
    );
    let b = reply(
        "b",
        "2026-11-08T01:01:00Z",
        &json!({"cache_creation_input_tokens": 100}),
    );
    assert_eq!(causes(&[a_bad, b]), [None, Some(100), None]);
}

#[test]
fn a_break_stands_when_the_next_reply_is_unreadable_too() {
    // b unreadable in every block, then c unreadable and readable in its next block:
    // the break at b stands, so c's writes are new context, not an idle rebuild.
    let write = json!({"cache_creation_input_tokens": 100});
    let bad = json!({"cache_creation_input_tokens": "x"});
    let replies = [
        reply("a", "2026-11-08T01:00:00Z", &write),
        reply("b", "2026-11-08T03:00:00Z", &bad),
        reply("c", "2026-11-08T05:00:00Z", &bad),
        reply("c", "2026-11-08T05:00:00Z", &write),
    ];
    assert_eq!(causes(&replies), [Some(100), Some(100), None]);
}

#[test]
fn a_count_above_2_40_is_unreadable() {
    let writes = |n: u64| {
        reply(
            "a",
            "2026-11-08T01:00:00Z",
            &json!({"cache_creation_input_tokens": n}),
        )
    };
    let write = |n| read_reply(&writes(n)).map(|r| r.usage.write);
    assert_eq!(write(1 << 40), Ok(1 << 40));
    assert_eq!(write((1 << 40) + 1), Err("cache_creation_input_tokens"));
}

#[test]
fn usage_or_a_split_that_is_not_an_object_is_skipped() {
    let at = "2026-11-08T01:00:00Z";
    for bad in [json!(null), json!(7), json!([1])] {
        let usage = read_reply(&reply("a", at, &bad)).err();
        assert_eq!(usage, Some("usage"), "usage {bad}");
        let split = json!({"cache_creation_input_tokens": 100, "cache_creation": bad});
        let split = read_reply(&reply("a", at, &split)).err();
        assert_eq!(split, Some("cache_creation"), "split {bad}");
    }
}

#[test]
fn replies_writing_u64_max_add_nothing_and_never_overflow() {
    let max = json!({"cache_creation_input_tokens": u64::MAX});
    let replies = ["a", "b", "c"].map(|id| reply(id, "2026-11-08T01:00:00Z", &max));
    assert_eq!(causes(&replies), [None, None, None]);
}

#[test]
fn a_rebuild_counts_what_the_previous_reply_left_without_overflow() {
    let c = Cache {
        previous: Some(Previous {
            read: u64::MAX,
            write: 1,
            model: "m".to_owned(),
            at: time("2026-11-08T01:00:00Z"),
        }),
        ..Cache::default()
    };
    let reply = read_and_wrote("2026-11-08T01:00:00Z", u64::MAX, 5);
    assert_eq!(c.cause(&reply), (Cause::Other, 1));
}
