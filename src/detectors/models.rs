//! Input-side cost of Claude replies by scope, model, reasoning effort and subagent type,
//! and which subagent sessions ran on their parent session's highest-priced model.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::cache::{count, input_price, usage};
use super::tools::printed;
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};

/// Cache reads as a multiple of the uncached input price, by the exact model id, from
/// <https://platform.claude.com/docs/en/about-claude/pricing> as read on 16 November 2026;
/// any other model's is [`READ_DEFAULT`].
const READ_MULTIPLIERS: [(&str, f64); 6] = [
    ("claude-fable-5-2", 0.025),
    ("claude-mythos-5-2", 0.025),
    ("claude-fable-5-1", 0.025),
    ("claude-mythos-5-1", 0.025),
    ("claude-opus-5-5", 0.05),
    ("claude-sonnet-5-5", 0.05),
];
const READ_DEFAULT: f64 = 0.1;

fn read_multiplier(model: &str) -> f64 {
    READ_MULTIPLIERS
        .iter()
        .find(|(id, _)| *id == model)
        .map_or(READ_DEFAULT, |&(_, m)| m)
}

/// Printed for a field a reply does not record, which [`printed`] never gives.
const NONE: &str = "(none)";

/// Input tokens weighted by their price multipliers, and those of priced replies in
/// dollars per million tokens.
#[derive(Default, Clone, Copy)]
struct Cost {
    weighted: f64,
    weighted_usd: f64,
}

impl Cost {
    fn add(&mut self, weighted: f64, price: Option<f64>) {
        self.weighted += weighted;
        self.weighted_usd += weighted * price.unwrap_or(0.0);
    }

    /// Dollars at the bundled prices, or at the given rate.
    fn usd(self, rate: Option<f64>) -> f64 {
        rate.map_or(self.weighted_usd, |r| self.weighted * r) / 1e6
    }
}

/// Replies of one scope, model, effort and agent type.
#[derive(Default)]
struct Group {
    replies: u64,
    input: u64,
    write: u64,
    read: u64,
    output: u64,
    cost: Cost,
    unpriced: u64,
}

/// A subagent session's replies in the window.
#[derive(Default)]
struct Sub {
    /// The id of the main session it ran for, from its records' `sessionId`.
    parent: Option<String>,
    /// The lowest input price of its replies, and whether one had no price.
    lowest: Option<f64>,
    unpriced: bool,
    cost: Cost,
}

/// How a subagent session's models compare with its parent session's dearest.
enum Rank {
    OnTop,
    Below,
    Unknown,
}

/// Input-side cost by scope, model, effort and agent type.
#[derive(Default)]
pub struct Models {
    /// The message ids of the file's replies so far.
    ids: HashSet<String>,
    /// The price the scan gave for the reply about to be recorded.
    price: Option<f64>,
    /// The file's, when it is a subagent's with a reply in the window.
    sub: Option<Sub>,
    groups: BTreeMap<(&'static str, String, String, String), Group>,
    /// By main session id, the highest input price of its replies in the window, if any
    /// had one.
    parents: HashMap<String, Option<f64>>,
    subs: Vec<Sub>,
    /// A reply whose usage could not be read since the previous reply, by its message id
    /// if it has one, and whether it is in the window: unless a later block of the same
    /// reply is readable, it counts as skipped.
    unreadable: Option<(Option<String>, bool)>,
    /// Replies in the window none of whose records has readable usage.
    skipped: u64,
}

/// A reply's uncached input and output tokens.
fn tokens(reply: &Value) -> Option<(u64, u64)> {
    let u = reply.pointer("/message/usage")?;
    Some((count(u, "input_tokens")?, count(u, "output_tokens")?))
}

/// A string field of a record as printed, or [`NONE`] when the record has none.
fn field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .map_or_else(|| NONE.to_owned(), printed)
}

impl Detector for Models {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"type\":\"assistant\"")
    }

    fn begin(&mut self, _s: &Session) {
        self.ids.clear();
        self.price = None;
        self.sub = None;
    }

    fn end(&mut self, _s: &Session) {
        self.skip();
        self.subs.extend(self.sub.take());
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
        self.price = usd_per_mtok;
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        // The scan gives a reply's price just before the reply, and none for a copy.
        let price = self.price.take();
        let v = r.value;
        let model = v.pointer("/message/model").and_then(Value::as_str);
        let Some(model) = model.filter(|m| r.kind() == "assistant" && *m != "<synthetic>") else {
            return;
        };
        // Claude Code writes a record for each block of a reply, each with its usage.
        let id = v.pointer("/message/id").and_then(Value::as_str);
        if id.is_some_and(|id| self.ids.contains(id)) {
            return;
        }
        // A reply whose counts do not sum within 64 bits is as unreadable as a malformed one.
        let readable = usage(v).zip(tokens(v)).filter(|(u, (input, output))| {
            [u.read, u.write, *input, *output]
                .into_iter()
                .try_fold(0_u64, u64::checked_add)
                .is_some()
        });
        let Some((u, (input, output))) = readable else {
            self.unreadable(id, r.in_window);
            return;
        };
        self.readable(id);
        if !r.in_window {
            return;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let weighted =
            input as f64 + u.write as f64 * u.multiplier + u.read as f64 * read_multiplier(model);
        let key = (
            s.scope(),
            printed(model),
            field(v, "effort"),
            field(v, "attributionAgent"),
        );
        let g = self.groups.entry(key).or_default();
        g.replies += 1;
        g.input = g.input.saturating_add(input);
        g.write = g.write.saturating_add(u.write);
        g.read = g.read.saturating_add(u.read);
        g.output = g.output.saturating_add(output);
        g.cost.add(weighted, price);
        g.unpriced += u64::from(price.is_none());
        self.session(s, v, weighted, price);
    }

    fn section(&self, opts: &Opts) -> Section {
        let rate = opts.usd_per_mtok;
        let (mut on_top, mut below, mut unknown, mut on_top_usd) = (0_u64, 0_u64, 0_u64, 0.0);
        for sub in &self.subs {
            match self.rank(sub) {
                Rank::OnTop => {
                    on_top += 1;
                    on_top_usd += sub.cost.usd(rate);
                }
                Rank::Below => below += 1,
                Rank::Unknown => unknown += 1,
            }
        }
        let summary = [
            (
                "replies",
                json!(self.groups.values().map(|g| g.replies).sum::<u64>()),
            ),
            ("replies_skipped", json!(self.skipped)),
            ("subagent_sessions", json!(self.subs.len())),
            ("subagent_sessions_on_top_model", json!(on_top)),
            ("subagent_sessions_below_top_model", json!(below)),
            ("subagent_sessions_indeterminate", json!(unknown)),
            ("on_top_input_usd_est", json!(cents(on_top_usd))),
        ];
        Section {
            name: "models",
            title: "input-side cost by scope, model, reasoning effort and subagent type (Claude)",
            summary: summary
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            tables: vec![self.table(opts)],
            notes: notes(opts),
        }
    }
}

/// Dollars rounded to cents.
fn cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

/// The section's notes.
fn notes(opts: &Opts) -> Vec<String> {
    let price = input_price(opts);
    vec![
        format!("Each reply counts once (the first record of its message.id); synthetic replies are left out, and a reply none of whose records has token counts that are non-negative integers summing within 64 bits counts as skipped (replies skipped) and adds to no total, so with any skipped the totals are a floor. Effort is the reasoning effort the reply records and agent type the subagent type Claude Code records (attributionAgent); {NONE} marks a reply that records neither."),
        format!("Input usd est is an input-side estimate, not spend: uncached input at the input price, cache writes at 1.25 times it (5-minute) or 2 times (1-hour), as the reply records, and cache reads at the model's cache read price, 0.025, 0.05 or 0.1 times the input price as Anthropic's pricing page gave on 16 November 2026, where the input price is {price}. Output tokens are counted but not priced, as shaphan reads input prices only; output costs several times as much as input per token. Input usd share is each row's part of all rows' input usd est. Unpriced replies are those of a model without a price. An illustration, not a bill."),
        "A subagent session is on its parent's top model when every reply of it in the window is on a model whose input price is at least the highest of its parent session's replies in the window; below when one is cheaper; indeterminate when the parent session (the main session whose id its records carry) was not read or had no priced reply in the window, or the subagent had an unpriced reply. Prices compared are the bundled ones, or the price file's with --prices, whatever --usd-per-mtok gives; on top input usd est sums those sessions' input usd est.".to_owned(),
    ]
}

impl Models {
    /// Whether the pending unreadable reply has message id `id`: a later block of it.
    fn pending(&self, id: Option<&str>) -> bool {
        self.unreadable
            .as_ref()
            .is_some_and(|(bad, _)| bad.is_some() && bad.as_deref() == id)
    }

    /// Counts the pending unreadable reply, if any, as skipped when it is in the window.
    fn skip(&mut self) {
        if let Some((_, in_window)) = self.unreadable.take() {
            self.skipped += u64::from(in_window);
        }
    }

    /// Notes a reply with message id `id` whose usage cannot be read, in the window or not.
    fn unreadable(&mut self, id: Option<&str>, in_window: bool) {
        if !self.pending(id) {
            self.skip();
            self.unreadable = Some((id.map(str::to_owned), in_window));
        }
    }

    /// Notes a readable reply with message id `id`: a pending unreadable block of it is
    /// not skipped, and a pending other reply is.
    fn readable(&mut self, id: Option<&str>) {
        if self.pending(id) {
            self.unreadable = None;
        } else {
            self.skip();
        }
        if let Some(id) = id {
            self.ids.insert(id.to_owned());
        }
    }

    /// Notes a reply's model price for its session: the lowest of a subagent session,
    /// or the highest of a main session.
    fn session(&mut self, s: &Session, v: &Value, weighted: f64, price: Option<f64>) {
        let session = v.get("sessionId").and_then(Value::as_str);
        if s.subagent {
            let sub = self.sub.get_or_insert_with(Sub::default);
            if sub.parent.is_none() {
                sub.parent = session.map(str::to_owned);
            }
            sub.cost.add(weighted, price);
            match price {
                Some(p) => sub.lowest = Some(sub.lowest.map_or(p, |l| l.min(p))),
                None => sub.unpriced = true,
            }
        } else if let Some(id) = session {
            let top = self.parents.entry(id.to_owned()).or_default();
            if let Some(p) = price {
                *top = Some(top.map_or(p, |t| t.max(p)));
            }
        }
    }

    fn rank(&self, sub: &Sub) -> Rank {
        let top = sub
            .parent
            .as_ref()
            .and_then(|p| self.parents.get(p))
            .copied()
            .flatten();
        match (top, sub.lowest) {
            (Some(top), Some(low)) if !sub.unpriced => {
                if low >= top {
                    Rank::OnTop
                } else {
                    Rank::Below
                }
            }
            _ => Rank::Unknown,
        }
    }

    /// The table by scope, model, effort and agent type, most input dollars first.
    fn table(&self, opts: &Opts) -> Table {
        let rate = opts.usd_per_mtok;
        let total: f64 = self.groups.values().map(|g| g.cost.usd(rate)).sum();
        let mut rows: Vec<(f64, Vec<Value>)> = self
            .groups
            .iter()
            .map(|((scope, model, effort, agent), g)| {
                let usd = g.cost.usd(rate);
                let share = (total > 0.0).then(|| cents(usd / total));
                let row = vec![
                    json!(scope),
                    json!(model),
                    json!(effort),
                    json!(agent),
                    json!(g.replies),
                    json!(g.input),
                    json!(g.write),
                    json!(g.read),
                    json!(g.output),
                    json!(cents(usd)),
                    json!(share),
                    json!(if rate.is_some() { 0 } else { g.unpriced }),
                ];
                (usd, row)
            })
            .collect();
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut table = Table::new(
            "By scope, model, effort and agent type, most input dollars first",
            vec![
                "scope",
                "model",
                "effort",
                "agent_type",
                "replies",
                "input_tokens",
                "cache_write_tokens",
                "cache_read_tokens",
                "output_tokens",
                "input_usd_est",
                "input_usd_share",
                "unpriced_replies",
            ],
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        table
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(id: &str, input: &Value) -> Value {
        json!({"type": "assistant", "timestamp": "2026-11-08T01:00:00Z",
               "message": {"model": "m", "id": id,
                           "usage": {"input_tokens": input, "output_tokens": 1}}})
    }

    /// The detector after `records` of one main session, each in the window or not.
    fn run(records: &[(Value, bool)]) -> Models {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut m = Models::default();
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
        // An unreadable reply superseded by another reply, and one outside the window.
        assert_eq!(
            skipped(&[("a", &bad, true), ("b", &good, true), ("c", &bad, false)]),
            (1, 1)
        );
    }
}
