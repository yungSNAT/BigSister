//! Cache writes of each Claude reply, split into new context and a rebuild of what the
//! previous reply had cached, with the rebuild's likely cause.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::report::{Opts, Section, Table, ratio};
use crate::scan::{Agent, Detector, Record, Session};
use crate::time::parse_ts;

/// Why a reply wrote to the cache, in report order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Cause {
    /// The session's first reply, which has no earlier cache.
    First,
    /// Context the previous reply had not cached: the new turn, or a compacted context.
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
        if tokens == 0 {
            return;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let weighted = tokens as f64 * multiplier;
        self.replies += 1;
        self.tokens += tokens;
        self.weighted += weighted;
        match usd_per_mtok {
            Some(usd) => self.weighted_usd += weighted * usd,
            None => self.unpriced += tokens,
        }
    }
}

/// What the previous reply of the session left cached.
struct Previous {
    /// Tokens it read from and wrote to the cache: the prefix cached after it.
    cached: u64,
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
    /// Whether a compaction came after the previous reply.
    compacted: bool,
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
}

/// A reply's usage, as far as the cache goes.
struct Usage {
    read: u64,
    write: u64,
    /// The write price multiplier, weighted by the tokens written at each lifetime.
    multiplier: f64,
    one_hour: bool,
}

/// A count in `usage`: a missing or null field is 0; any other value that is not a
/// non-negative integer makes the usage unreadable.
fn count(usage: &Value, key: &str) -> Option<u64> {
    match usage.get(key) {
        None | Some(Value::Null) => Some(0),
        Some(v) => v.as_u64(),
    }
}

fn usage(reply: &Value) -> Option<Usage> {
    let u = reply.pointer("/message/usage")?;
    let write = count(u, "cache_creation_input_tokens")?;
    let split = u.get("cache_creation").unwrap_or(&Value::Null);
    let five = count(split, "ephemeral_5m_input_tokens")?;
    let hour = count(split, "ephemeral_1h_input_tokens")?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts stay far below 2^52"
    )]
    let multiplier = if five + hour > 0 {
        (five as f64 * WRITE_5M + hour as f64 * WRITE_1H) / (five + hour) as f64
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
        self.compacted = false;
        self.ids.clear();
        self.price = None;
        self.counted = false;
        self.idle = false;
    }

    fn end(&mut self, s: &Session) {
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
            self.compacted = true;
            return;
        }
        let model = v.pointer("/message/model").and_then(Value::as_str);
        let Some(model) = model.filter(|m| r.kind() == "assistant" && *m != "<synthetic>") else {
            return;
        };
        // Claude Code writes a record for each block of a reply, each with its usage.
        if let Some(id) = v.pointer("/message/id").and_then(Value::as_str)
            && !self.ids.insert(id.to_owned())
        {
            return;
        }
        let Some(u) = usage(v) else {
            return;
        };
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        let (cause, rebuilt) = match &self.previous {
            None => (Cause::First, 0),
            Some(_) if self.compacted => (Cause::New, 0),
            Some(p) => {
                let ttl = if self.one_hour {
                    ONE_HOUR
                } else {
                    FIVE_MINUTES
                };
                let gap = at.zip(p.at).map(|(now, then)| (now - then).num_seconds());
                let cause = if gap.is_some_and(|g| g > ttl) {
                    Cause::Idle
                } else if p.model != model {
                    Cause::Switch
                } else {
                    Cause::Other
                };
                (cause, u.write.min(p.cached.saturating_sub(u.read)))
            }
        };
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
        self.compacted = false;
        self.previous = Some(Previous {
            cached: u.read.saturating_add(u.write),
            model: model.to_owned(),
            at,
        });
    }

    fn section(&self, opts: &Opts) -> Section {
        let total: u64 = self.writes.values().map(|w| w.tokens).sum();
        let idle = || {
            self.writes
                .iter()
                .filter(|((_, c), _)| *c == Cause::Idle)
                .map(|(_, w)| w)
        };
        let idle_rebuilds: u64 = idle().map(|w| w.replies).sum();
        let idle_tokens: u64 = idle().map(|w| w.tokens).sum();
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
                let usd = opts
                    .usd_per_mtok
                    .map_or(w.weighted_usd, |rate| w.weighted * rate);
                let unpriced = if opts.usd_per_mtok.is_some() {
                    0
                } else {
                    w.unpriced
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
        let price = opts.usd_per_mtok.map_or_else(
            || format!("the uncached input price of the reply's model, from prices bundled with this release and read on {}", crate::prices::READ),
            |rate| format!("US${rate} per million tokens, the rate given with --usd-per-mtok"),
        );
        Section {
            name: "cache",
            title: "cache writes: new context, and rebuilds of what the previous reply had cached (Claude)",
            summary: vec![
                ("sessions".to_owned(), json!(self.sessions.values().sum::<u64>())),
                ("write_tokens".to_owned(), json!(total)),
                ("idle_rebuilds".to_owned(), json!(idle_rebuilds)),
                ("sessions_with_idle_rebuilds".to_owned(), json!(self.sessions_idle)),
                ("idle_rebuild_share".to_owned(), ratio(idle_tokens, total)),
            ],
            tables: vec![table],
            notes: vec![
                "Each reply's cache writes (cache_creation_input_tokens) are measured, not estimated. A reply after the first rebuilt the tokens the previous reply had cached (its cache reads and writes) that it did not read, up to what it wrote; the rest of its writes are new context. A reply after a compaction writes new context only.".to_owned(),
                "A rebuild follows an idle gap when the time between the 2 replies exceeds the cache's lifetime: 1 hour once a reply of the session has written 1-hour cache, otherwise 5 minutes. Otherwise it follows a model switch when the model changed, and is rebuilt otherwise when neither holds, as after a changed prefix.".to_owned(),
                format!("Dollars (write usd est) price the tokens at the cache write price, 1.25 times {price} for 5-minute writes and 2 times for 1-hour writes, split as the reply records (1.25 when it does not): an illustration, not a bill. A warm cache would have read rebuilt tokens at a tenth of the input price. Unpriced tokens are those of a model without a price."),
            ],
        }
    }
}

impl Cache {
    fn add(
        &mut self,
        scope: &'static str,
        cause: Cause,
        tokens: u64,
        u: &Usage,
        price: Option<f64>,
    ) {
        self.writes
            .entry((scope, cause))
            .or_default()
            .add(tokens, u.multiplier, price);
    }
}
