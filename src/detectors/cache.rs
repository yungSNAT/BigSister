//! Cache writes of each Claude reply, split into new context and a rebuild of what the
//! previous reply had cached, with the rebuild's likely cause.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

use crate::report::{Opts, Section, Table, ratio};
use crate::scan::{Agent, Detector, Record, Session};
use crate::time::parse_ts;

/// Why a reply wrote to the cache, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Cause {
    /// The session's first reply, which has no earlier cache.
    First,
    /// Context the previous reply had not cached: the new turn, or all of a reply's writes
    /// after a compaction or a reply with no readable usage.
    New,
    /// Rebuilt after a gap longer than the cache's lifetime.
    Idle,
    /// Rebuilt on another model than the previous reply's, which has its own cache.
    Switch,
    /// Rebuilt for any other reason, such as a changed prefix.
    Other,
}

impl Cause {
    fn label(self) -> &'static str {
        match self {
            Cause::First => "first reply",
            Cause::New => "new context",
            Cause::Idle => "rebuilt after idle gap",
            Cause::Switch => "rebuilt after model switch",
            Cause::Other => "rebuilt otherwise",
        }
    }
}

/// Cache writes of one scope and cause.
#[derive(Default, Clone, Copy)]
struct Writes {
    replies: u64,
    tokens: u64,
    /// The tokens, each times its write price multiplier.
    weighted: f64,
    /// The weighted tokens of priced replies, each times its model's input price per
    /// million tokens.
    weighted_usd: f64,
    unpriced: u64,
}

impl Writes {
    fn add(&mut self, tokens: u64, multiplier: f64, usd_per_mtok: Option<f64>) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let weighted = tokens as f64 * multiplier;
        // shortcut: sums saturate; with each count at most MAX_COUNT, that takes more than
        // 2^24 replies of a row, each claiming over a trillion tokens.
        self.replies += 1;
        self.tokens = self.tokens.saturating_add(tokens);
        self.weighted += weighted;
        match usd_per_mtok {
            Some(usd) => self.weighted_usd += weighted * usd,
            None => self.unpriced = self.unpriced.saturating_add(tokens),
        }
    }
}

/// What the previous reply of the session left cached.
struct Previous {
    /// Tokens it read from and wrote to the cache: the prefix cached after it.
    read: u64,
    write: u64,
    model: String,
    at: Option<DateTime<Utc>>,
}

/// The cache's lifetimes: 5 minutes by default, an hour once a reply writes at that
/// lifetime.
const FIVE_MINUTES: i64 = 5 * 60;
const ONE_HOUR: i64 = 60 * 60;
/// Write prices as multiples of the uncached input price, by lifetime.
const WRITE_5M: f64 = 1.25;
const WRITE_1H: f64 = 2.0;

/// Cache writes by scope and cause.
#[derive(Default)]
pub struct Cache {
    previous: Option<Previous>,
    /// Whether a reply of the session so far wrote at the 1-hour lifetime.
    one_hour: bool,
    /// Whether the next reply's writes are all new context: a compaction came after the
    /// previous reply, or a reply's usage could not be read.
    fresh: bool,
    /// A reply whose usage could not be read since the previous reply, by its message id
    /// if it has one, and whether it is in the window: unless a later block of the same
    /// reply is readable, the chain breaks and the reply counts as skipped.
    unreadable: Option<(Option<String>, bool)>,
    /// The message ids of the session's replies so far.
    ids: HashSet<String>,
    /// The price the scan gave for the reply about to be recorded.
    price: Option<f64>,
    /// Whether the session had a reply in the window, and one rebuilt after an idle gap.
    counted: bool,
    idle: bool,
    writes: BTreeMap<(&'static str, Cause), Writes>,
    sessions: BTreeMap<&'static str, u64>,
    sessions_idle: u64,
    /// Replies in the window none of whose records has readable usage.
    skipped: u64,
}

/// A reply's usage, as far as the cache goes.
struct Usage {
    read: u64,
    write: u64,
    /// The write price multiplier, weighted by the tokens written at each lifetime.
    multiplier: f64,
    one_hour: bool,
}

/// The largest token count read, 2^40: far more than any context holds, and small
/// enough that token sums stay exact.
const MAX_COUNT: u64 = 1 << 40;

/// A count in `usage`: a missing or null field is 0; any other value that is not an
/// integer from 0 to [`MAX_COUNT`] makes the usage unreadable.
fn count(usage: &Value, key: &str) -> Option<u64> {
    match usage.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(v) => v.as_u64().filter(|&n| n <= MAX_COUNT),
    }
}

fn usage(reply: &Value) -> Option<Usage> {
    let u = reply.pointer("/message/usage")?;
    let write = count(u, "cache_creation_input_tokens")?;
    let split = u.get("cache_creation").unwrap_or(&Value::Null);
    let five = count(split, "ephemeral_5m_input_tokens")?;
    let hour = count(split, "ephemeral_1h_input_tokens")?;
    let both = five + hour;
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts stay far below 2^52"
    )]
    let multiplier = if both > 0 {
        (five as f64 * WRITE_5M + hour as f64 * WRITE_1H) / both as f64
    } else {
        WRITE_5M
    };
    Some(Usage {
        read: count(u, "cache_read_input_tokens")?,
        write,
        multiplier,
        one_hour: hour > 0,
    })
}

impl Detector for Cache {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"type\":\"assistant\"") || line.contains("\"compact_boundary\"")
    }

    fn begin(&mut self, _s: &Session) {
        self.previous = None;
        self.one_hour = false;
        self.fresh = false;
        self.unreadable = None;
        self.ids.clear();
        self.price = None;
        self.counted = false;
        self.idle = false;
    }

    fn end(&mut self, s: &Session) {
        self.skip();
        if self.counted {
            *self.sessions.entry(s.scope()).or_default() += 1;
        }
        if self.idle {
            self.sessions_idle += 1;
        }
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
        self.price = usd_per_mtok;
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        // The scan gives a reply's price just before the reply, and none for a copy.
        let price = self.price.take();
        let v = r.value;
        if r.kind() == "system"
            && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
        {
            self.fresh = true;
            return;
        }
        let model = v.pointer("/message/model").and_then(Value::as_str);
        let Some(model) = model.filter(|m| r.kind() == "assistant" && *m != "<synthetic>") else {
            return;
        };
        // Claude Code writes a record for each block of a reply, each with its usage.
        let id = v.pointer("/message/id").and_then(Value::as_str);
        if id.is_some_and(|id| self.ids.contains(id)) {
            return;
        }
        let Some(u) = usage(v) else {
            self.unreadable(id, r.in_window);
            return;
        };
        self.readable(id);
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        let (cause, rebuilt) = self.cause(model, at, &u);
        if r.in_window {
            self.counted = true;
            let scope = s.scope();
            if cause == Cause::First {
                self.add(scope, Cause::First, u.write, &u, price);
            } else {
                self.add(scope, Cause::New, u.write - rebuilt, &u, price);
                self.add(scope, cause, rebuilt, &u, price);
                self.idle |= cause == Cause::Idle && rebuilt > 0;
            }
        }
        self.one_hour |= u.one_hour;
        self.fresh = false;
        self.previous = Some(Previous {
            read: u.read,
            write: u.write,
            model: model.to_owned(),
            at,
        });
    }

    fn section(&self, opts: &Opts) -> Section {
        let total = self
            .writes
            .values()
            .map(|w| w.tokens)
            .fold(0, u64::saturating_add);
        let idle = || {
            self.writes
                .iter()
                .filter(|((_, c), _)| *c == Cause::Idle)
                .map(|(_, w)| w)
        };
        let idle_tokens = idle().map(|w| w.tokens).fold(0, u64::saturating_add);
        Section {
            name: "cache",
            title: "cache writes: new context, and rebuilds of what the previous reply had cached (Claude)",
            summary: vec![
                (
                    "sessions".to_owned(),
                    json!(self.sessions.values().sum::<u64>()),
                ),
                ("write_tokens".to_owned(), json!(total)),
                (
                    "idle_rebuilds".to_owned(),
                    json!(idle().map(|w| w.replies).sum::<u64>()),
                ),
                (
                    "sessions_with_idle_rebuilds".to_owned(),
                    json!(self.sessions_idle),
                ),
                ("idle_rebuild_share".to_owned(), ratio(idle_tokens, total)),
                ("replies_skipped".to_owned(), json!(self.skipped)),
            ],
            tables: vec![self.table(opts)],
            notes: notes(opts),
        }
    }
}

/// The input price dollars are reckoned from: the `--usd-per-mtok` rate, or the reply's
/// model's price in the `--prices` file or the bundled prices.
pub(super) fn input_price(opts: &Opts) -> String {
    match (opts.usd_per_mtok, &opts.prices_file) {
        (Some(rate), _) => {
            format!("US${rate} per million tokens, the rate given with --usd-per-mtok")
        }
        (None, Some(file)) => format!(
            "the uncached input price of the reply's model in the price file {file}, given with --prices"
        ),
        (None, None) => format!(
            "the uncached input price of the reply's model, from prices bundled with this release and read on {}",
            crate::prices::READ
        ),
    }
}

/// The section's notes.
fn notes(opts: &Opts) -> Vec<String> {
    let price = input_price(opts);
    vec![
        "Each reply's cache writes (cache_creation_input_tokens) are measured. Rebuilt tokens are an estimate of what was cached again: of the tokens the previous reply in the session left cached (its cache reads and writes), those this reply did not read, up to what it wrote. They assume each reply extends the previous one's prompt, as Claude Code's replies in one file do; the rest of a reply's writes are new context, and a reply after a compaction, or after a reply none of whose records has readable usage, writes new context only. Such replies count as skipped (replies skipped) and add to no total, so with any skipped the totals are a floor.".to_owned(),
        "A rebuild follows an idle gap when the time between the 2 replies exceeds the cache's lifetime: 1 hour once a reply of the session has written 1-hour cache, otherwise 5 minutes. Otherwise it follows a model switch when the model changed, and is rebuilt otherwise when neither holds, as after a rewind or a changed prefix.".to_owned(),
        format!("Dollars (write usd est) estimate what the writes cost, not what could have been saved: the tokens at the cache write price, 1.25 times the input price for 5-minute writes and 2 times for 1-hour writes, split as the reply records (1.25 times when it does not), where the input price is {price}. A warm cache would have read rebuilt tokens at a fraction of the input price: a tenth or less, by model. Unpriced tokens are those of a model without a price. An illustration, not a bill."),
    ]
}

impl Cache {
    /// Why a reply on `model`, recorded `at`, wrote to the cache, and how many of its
    /// writes rebuilt what the previous reply had cached.
    fn cause(&self, model: &str, at: Option<DateTime<Utc>>, u: &Usage) -> (Cause, u64) {
        if self.fresh {
            return (Cause::New, 0);
        }
        let Some(p) = &self.previous else {
            return (Cause::First, 0);
        };
        let ttl = if self.one_hour {
            ONE_HOUR
        } else {
            FIVE_MINUTES
        };
        let cause = if at
            .zip(p.at)
            .is_some_and(|(now, then)| now - then > TimeDelta::seconds(ttl))
        {
            Cause::Idle
        } else if p.model != model {
            Cause::Switch
        } else {
            Cause::Other
        };
        // What the previous reply left cached and this one did not read, without overflow.
        let left = if p.read >= u.read {
            (p.read - u.read).saturating_add(p.write)
        } else {
            p.write.saturating_sub(u.read - p.read)
        };
        (cause, u.write.min(left))
    }

    /// Whether the pending unreadable reply has message id `id`: a later block of it.
    fn pending(&self, id: Option<&str>) -> bool {
        self.unreadable
            .as_ref()
            .is_some_and(|(bad, _)| bad.is_some() && bad.as_deref() == id)
    }

    /// Counts the pending unreadable reply, if any, as skipped when it is in the window,
    /// and says whether there was one.
    fn skip(&mut self) -> bool {
        let Some((_, in_window)) = self.unreadable.take() else {
            return false;
        };
        self.skipped += u64::from(in_window);
        true
    }

    /// Notes a reply with message id `id` whose usage cannot be read. After an earlier
    /// unreadable reply other than this one, the chain is broken whatever follows.
    fn unreadable(&mut self, id: Option<&str>, in_window: bool) {
        if self.pending(id) {
            return;
        }
        if self.skip() {
            self.fresh = true;
        }
        self.unreadable = Some((id.map(str::to_owned), in_window));
    }

    /// Notes a readable reply with message id `id`. After an unreadable reply other than
    /// this one, what the previous reply left cached is unknown, so nothing counts as
    /// rebuilt.
    fn readable(&mut self, id: Option<&str>) {
        if self.pending(id) {
            self.unreadable = None;
        } else if self.skip() {
            self.fresh = true;
        }
        if let Some(id) = id {
            self.ids.insert(id.to_owned());
        }
    }

    /// The table by scope and cause, with dollars at the bundled prices or the given rate.
    fn table(&self, opts: &Opts) -> Table {
        let mut table = Table::new(
            "By scope and cause",
            vec![
                "scope",
                "cause",
                "replies",
                "write_tokens",
                "write_usd_est",
                "unpriced_tokens",
            ],
        );
        table.rows = self
            .writes
            .iter()
            .map(|((scope, cause), w)| {
                let (usd, unpriced) = match opts.usd_per_mtok {
                    Some(rate) => (w.weighted * rate, 0),
                    None => (w.weighted_usd, w.unpriced),
                };
                vec![
                    json!(scope),
                    json!(cause.label()),
                    json!(w.replies),
                    json!(w.tokens),
                    json!((usd / 1e4).round() / 100.0),
                    json!(unpriced),
                ]
            })
            .collect();
        table.truncate(opts.limit);
        table
    }

    fn add(
        &mut self,
        scope: &'static str,
        cause: Cause,
        tokens: u64,
        u: &Usage,
        price: Option<f64>,
    ) {
        // A reply with no writes of this cause leaves no row behind.
        if tokens == 0 {
            return;
        }
        self.writes
            .entry((scope, cause))
            .or_default()
            .add(tokens, u.multiplier, price);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(id: &str, at: &str, usage: &Value) -> Value {
        json!({"type": "assistant", "timestamp": at,
               "message": {"model": "m", "id": id, "usage": usage}})
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
        let u = usage(&split(1000, 1000)).expect("usage");
        assert!((u.multiplier - 1.625).abs() < 1e-12, "{}", u.multiplier);
        assert!(u.one_hour);
        assert!(usage(&split(u64::MAX, 1)).is_none());
    }

    #[test]
    fn a_gap_a_millisecond_over_the_lifetime_is_idle() {
        let c = Cache {
            previous: Some(Previous {
                read: 0,
                write: 100,
                model: "m".to_owned(),
                at: parse_ts("2026-11-08T01:00:00.000Z"),
            }),
            ..Cache::default()
        };
        let u = Usage {
            read: 0,
            write: 100,
            multiplier: WRITE_5M,
            one_hour: false,
        };
        let cause = |at: &str| c.cause("m", parse_ts(at), &u).0;
        assert!(cause("2026-11-08T01:05:00.001Z") == Cause::Idle);
        assert!(cause("2026-11-08T01:05:00.000Z") == Cause::Other);
    }

    /// The cache tokens by cause after `replies` of one main session.
    /// The detector after `records` of one main session, all in the window.
    fn run(records: &[Value]) -> Cache {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut c = Cache::default();
        c.begin(&s);
        for v in records {
            c.record(
                &s,
                &Record {
                    value: v,
                    in_window: true,
                },
            );
        }
        c.end(&s);
        c
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
        assert_eq!(skipped(&[a.clone(), b_bad.clone(), c_bad, c]), 1);
        // Still unreadable when the session ends.
        assert_eq!(skipped(&[a, b_bad]), 1);
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
        assert_eq!(usage(&writes(1 << 40)).map(|u| u.write), Some(1 << 40));
        assert!(usage(&writes((1 << 40) + 1)).is_none());
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
                at: None,
            }),
            ..Cache::default()
        };
        let u = Usage {
            read: u64::MAX,
            write: 5,
            multiplier: WRITE_5M,
            one_hour: false,
        };
        assert_eq!(c.cause("m", None, &u), (Cause::Other, 1));
    }
}
