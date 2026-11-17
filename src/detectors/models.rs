//! Input-side cost of Claude replies by scope, model, reasoning effort and subagent type,
//! and which subagent sessions ran on their parent session's highest-priced model.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::cache::{Entry, Usage, count, field, input_price, object, read, read_usage};
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
    /// A reply that did not read since the previous reply, by its message id if it has
    /// one, and whether it is in the window: unless a later block of the same reply reads,
    /// it counts as skipped.
    unreadable: Option<(Option<String>, bool)>,
    /// Replies in the window none of whose records reads as a [`Reply`].
    skipped: u64,
    /// Replies counted in the window whose usage records no split by lifetime.
    without_split: u64,
}

/// A Claude reply as this section reads it. Every field is required but `effort` and
/// `attributionAgent`, which a reply may lack; a reply whose record lacks a required
/// field, or holds null or another type in any of them, is skipped.
struct Reply<'a> {
    /// `message.id`: Claude Code writes a record for each block of a reply.
    id: &'a str,
    /// `message.model`.
    model: &'a str,
    /// `sessionId`: a subagent's records carry its parent session's.
    session: &'a str,
    /// `effort`, the reasoning effort.
    effort: Option<&'a str>,
    /// `attributionAgent`, the subagent type.
    agent: Option<&'a str>,
    /// `message.usage`'s `input_tokens` and `output_tokens`, each an integer from 0 to
    /// 2^40, and its cache counts as `cache` reads them.
    input: u64,
    output: u64,
    usage: Usage,
}

/// A reply record read as a [`Reply`], or the name of a field that does not read.
fn read_reply(v: &Value) -> Result<Reply<'_>, &'static str> {
    let message = field(v, "message", object)?;
    let usage = field(message, "usage", object)?;
    Ok(Reply {
        id: field(message, "id", Value::as_str)?,
        model: field(message, "model", Value::as_str)?,
        session: field(v, "sessionId", Value::as_str)?,
        effort: label(v, "effort")?,
        agent: label(v, "attributionAgent")?,
        input: field(usage, "input_tokens", count)?,
        output: field(usage, "output_tokens", count)?,
        usage: read_usage(usage)?,
    })
}

/// `object`'s `key` when it is a string, `None` when it is missing, else the key.
fn label<'a>(object: &'a Value, key: &'static str) -> Result<Option<&'a str>, &'static str> {
    object.get(key).map(|v| v.as_str().ok_or(key)).transpose()
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
        let reply = match read(r, read_reply) {
            Entry::Reply(reply) => reply,
            Entry::Skipped(id) => {
                // A later block of a reply already counted or skipped adds nothing.
                if !id.is_some_and(|id| self.ids.contains(id)) {
                    self.unreadable(id, r.in_window);
                }
                return;
            }
            Entry::Compaction | Entry::Other => return,
        };
        if self.ids.contains(reply.id) {
            return;
        }
        self.readable(reply.id);
        if !r.in_window {
            return;
        }
        let u = &reply.usage;
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let weighted = reply.input as f64
            + u.write as f64 * u.multiplier
            + u.read as f64 * read_multiplier(reply.model);
        let shown = |label: Option<&str>| label.map_or_else(|| NONE.to_owned(), printed);
        let key = (
            s.scope(),
            printed(reply.model),
            shown(reply.effort),
            shown(reply.agent),
        );
        let g = self.groups.entry(key).or_default();
        g.replies += 1;
        g.input = g.input.saturating_add(reply.input);
        g.write = g.write.saturating_add(u.write);
        g.read = g.read.saturating_add(u.read);
        g.output = g.output.saturating_add(reply.output);
        g.cost.add(weighted, price);
        g.unpriced += u64::from(price.is_none());
        self.without_split += u64::from(!u.split);
        self.session(s, reply.session, weighted, price);
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
            ("replies_without_split", json!(self.without_split)),
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
        format!("Each reply counts once (the first record of its message.id); synthetic replies are left out, and a reply none of whose records has a model name and token counts that are non-negative integers summing within 64 bits counts as skipped (replies skipped) and adds to no total, so with any skipped the totals are a floor. Effort is the reasoning effort the reply records and agent type the subagent type Claude Code records (attributionAgent); {NONE} marks a reply that records neither."),
        format!("Input usd est is an input-side estimate, not spend: uncached input at the input price, cache writes at 1.25 times it (5-minute) or 2 times (1-hour), as the reply records (1.25 times for replies without split, which record none), and cache reads at the model's cache read price, 0.025, 0.05 or 0.1 times the input price as Anthropic's pricing page gave on 16 November 2026, where the input price is {price}. Output tokens are counted but not priced, as shaphan reads input prices only; output costs several times as much as input per token. Input usd share is each row's part of all rows' input usd est. Unpriced replies are those of a model without a price. An illustration, not a bill."),
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

    /// Counts the pending unreadable reply, if any, as skipped when it is in the window,
    /// so that any later block of it is ignored.
    fn skip(&mut self) {
        if let Some((id, in_window)) = self.unreadable.take() {
            self.ids.extend(id);
            self.skipped += u64::from(in_window);
        }
    }

    /// Notes a reply with message id `id`, if it has one, that does not read.
    fn unreadable(&mut self, id: Option<&str>, in_window: bool) {
        if !self.pending(id) {
            self.skip();
            self.unreadable = Some((id.map(str::to_owned), in_window));
        }
    }

    /// Notes a reply with message id `id` that reads: a pending block of it that did not
    /// read is not skipped, and a pending other reply is.
    fn readable(&mut self, id: &str) {
        if self.pending(Some(id)) {
            self.unreadable = None;
        } else {
            self.skip();
        }
        self.ids.insert(id.to_owned());
    }

    /// Notes a reply's model price for its session: the lowest of a subagent session,
    /// or the highest of a main session.
    fn session(&mut self, s: &Session, session: &str, weighted: f64, price: Option<f64>) {
        if s.subagent {
            let sub = self.sub.get_or_insert_with(Sub::default);
            if sub.parent.is_none() {
                sub.parent = Some(session.to_owned());
            }
            sub.cost.add(weighted, price);
            match price {
                Some(p) => sub.lowest = Some(sub.lowest.map_or(p, |l| l.min(p))),
                None => sub.unpriced = true,
            }
        } else {
            let top = self.parents.entry(session.to_owned()).or_default();
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
}
