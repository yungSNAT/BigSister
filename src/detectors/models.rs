//! Input-side cost of Claude replies by scope, model, reasoning effort and subagent type,
//! and which subagent sessions ran on their parent session's highest-priced model.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde_json::{Value, json};

use super::cache::{Entry, Usage, count, field, input_price, object, read, read_usage};
use super::tools::printed;
use crate::prices::Prices;
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};

/// Cache reads as a multiple of the uncached input price, by the exact model id, from
/// <https://platform.claude.com/docs/en/about-claude/pricing> as read on 16 November 2026.
fn read_multiplier(model: &str) -> f64 {
    match model {
        "claude-fable-5-2" | "claude-mythos-5-2" | "claude-fable-5-1" | "claude-mythos-5-1" => {
            0.025
        }
        "claude-opus-5-5" | "claude-sonnet-5-5" => 0.05,
        _ => 0.1,
    }
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
    /// The lowest base input price of its replies' models, and whether one had none or
    /// was skipped.
    lowest: Option<f64>,
    unsure: bool,
    cost: Cost,
}

/// How a subagent session's models compare with its parent session's dearest.
enum Rank {
    OnTop,
    Below,
    Unknown,
}

/// Input-side cost by scope, model, effort and agent type.
pub struct Models {
    /// The prices models are ranked by, at their base price.
    prices: Rc<Prices>,
    /// The message ids of the file's replies so far.
    ids: HashSet<String>,
    /// The price the scan gave for the reply about to be recorded.
    price: Option<f64>,
    /// The file's, when it is a subagent's with a reply in the window.
    sub: Option<Sub>,
    /// The main session ids the file's replies and unreadable records in the window
    /// carry, and whether one of its replies in the window was skipped with no one
    /// session id to mark.
    here: HashSet<String>,
    skipped_here: bool,
    groups: BTreeMap<(&'static str, String, String, String), Group>,
    /// By main session id, the highest base input price of its replies' models in the
    /// window.
    parents: HashMap<String, f64>,
    /// Main sessions with a reply in the window skipped or on a model without a price,
    /// which may have been dearer.
    unsure: HashSet<String>,
    subs: Vec<Sub>,
    /// A reply that did not read since the previous reply: unless a later block of the
    /// same reply reads, it counts as skipped.
    unreadable: Option<Unreadable>,
    /// Replies in the window none of whose records reads as a [`Reply`].
    skipped: u64,
    /// Replies counted in the window whose usage records no split by lifetime.
    without_split: u64,
    /// The input, cache write and cache read tokens of replies counted in the window
    /// without a price, each count at most 2^40.
    unpriced_tokens: u64,
}

/// A reply that did not read: its message id if it has one, whether it is in the window,
/// and in a main session's file the session id it carries, if it has one.
struct Unreadable {
    id: Option<String>,
    in_window: bool,
    session: Option<String>,
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

fn models_wants(_s: &Session, line: &str) -> bool {
    line.contains("\"type\":\"assistant\"")
}

impl Detector for Models {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        models_wants(s, line)
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Exact(models_wants))
    }

    fn begin(&mut self, _s: &Session) {
        self.ids.clear();
        self.price = None;
        self.sub = None;
        self.here.clear();
        self.skipped_here = false;
    }

    fn end(&mut self, s: &Session) {
        self.skip();
        // A skipped reply leaves its session's lowest or highest price unknown.
        if self.skipped_here {
            if s.subagent {
                self.sub.get_or_insert_with(Sub::default).unsure = true;
            } else {
                self.unsure.extend(self.here.drain());
            }
        }
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
                    // A main session's skipped reply leaves its own session's top unknown.
                    let session = field(r.value, "sessionId", Value::as_str).ok();
                    let session = session.filter(|_| !s.subagent);
                    self.unreadable(id, r.in_window, session);
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
        if price.is_none() {
            g.unpriced += 1;
            let tokens = reply.input.saturating_add(u.write).saturating_add(u.read);
            self.unpriced_tokens = self.unpriced_tokens.saturating_add(tokens);
        }
        self.without_split += u64::from(!u.split);
        let base = self.prices.base_usd_per_mtok(reply.model);
        self.session(s, reply.session, weighted, price, base);
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
        // A rate prices every reply.
        let unpriced = if rate.is_some() {
            (0, 0)
        } else {
            let replies: u64 = self.groups.values().map(|g| g.unpriced).sum();
            (replies, self.unpriced_tokens)
        };
        let summary = [
            (
                "replies",
                json!(self.groups.values().map(|g| g.replies).sum::<u64>()),
            ),
            ("replies_skipped", json!(self.skipped)),
            ("replies_without_split", json!(self.without_split)),
            ("replies_unpriced", json!(unpriced.0)),
            ("unpriced_input_tokens", json!(unpriced.1)),
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
        format!("Each reply counts once (the first record of its message.id); synthetic replies are left out, and a reply none of whose records reads whole (a message with an id, a model and a usage whose input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens and, when it has a cache_creation split, ephemeral_5m_input_tokens and ephemeral_1h_input_tokens are integers from 0 to 2^40; a session id; and effort and agent type as text when recorded) counts as skipped (replies skipped) and adds to no total, so with any skipped the totals are a floor. Effort is the reasoning effort the reply records and agent type the subagent type Claude Code records (attributionAgent); {NONE} marks a reply that records neither."),
        format!("Input usd est is an input-side estimate, not spend: uncached input at the input price, cache writes at 1.25 times it (5-minute) or 2 times (1-hour), as the reply records (1.25 times for replies without split, which record none), and cache reads at the model's cache read price, 0.025, 0.05 or 0.1 times the input price as Anthropic's pricing page gave on 16 November 2026, where the input price is {price}. Output tokens are counted but not priced, as bigsister reads input prices only; output costs several times as much as input per token. Input usd share is each row's part of all rows' input usd est, rounded to 2 places, so shares need not sum to 1. Unpriced replies are those of a model without a price, or of one priced by prompt size whose prompt size is not recorded; dollars and shares leave them out, and replies unpriced and unpriced input tokens (their input, cache write and cache read tokens) count them whatever --limit cuts. An illustration, not a bill."),
        "Each model is ranked at its base input price (its price for the shortest prompt, so a model priced by prompt size ranks the same whatever the prompt), from the bundled prices or the price file's with --prices, whatever --usd-per-mtok gives: a subagent session is on its parent's top model when the cheapest model of its replies in the window costs at least the dearest of its parent session's, so the same model on both sides is on top, and below when cheaper. It is indeterminate when the parent session (the main session whose id its records carry) was not read or had no reply in the window, or it or the subagent had a reply in the window skipped or on a model without a price (a skipped one without a session id, or whose blocks name different sessions, counts against every session its file's records in the window name, and against none, counting only in replies skipped, when no record names one), as such a reply's model may be dearer or cheaper. On top input usd est sums those sessions' input usd est.".to_owned(),
    ]
}

impl Models {
    /// The section, ranking models by their base price in `prices`.
    #[must_use]
    pub fn new(prices: Rc<Prices>) -> Self {
        Self {
            prices,
            ids: HashSet::new(),
            price: None,
            sub: None,
            here: HashSet::new(),
            skipped_here: false,
            groups: BTreeMap::new(),
            parents: HashMap::new(),
            unsure: HashSet::new(),
            subs: Vec::new(),
            unreadable: None,
            skipped: 0,
            without_split: 0,
            unpriced_tokens: 0,
        }
    }

    /// Whether the pending unreadable reply has message id `id`: a later block of it.
    fn pending(&self, id: Option<&str>) -> bool {
        self.unreadable
            .as_ref()
            .is_some_and(|u| u.id.is_some() && u.id.as_deref() == id)
    }

    /// Counts the pending unreadable reply, if any, as skipped when it is in the window,
    /// so that any later block of it is ignored.
    fn skip(&mut self) {
        let Some(u) = self.unreadable.take() else {
            return;
        };
        self.ids.extend(u.id);
        if u.in_window {
            self.skipped += 1;
            match u.session {
                Some(session) => {
                    self.unsure.insert(session);
                }
                None => self.skipped_here = true,
            }
        }
    }

    /// Notes a reply with message id `id`, if it has one, that does not read.
    fn unreadable(&mut self, id: Option<&str>, in_window: bool, session: Option<&str>) {
        // The file's sessions, which a skip with no one session to mark leaves unsure.
        if in_window && let Some(session) = session {
            self.here.insert(session.to_owned());
        }
        if !self.pending(id) {
            self.skip();
            self.unreadable = Some(Unreadable {
                id: id.map(str::to_owned),
                in_window,
                session: session.map(str::to_owned),
            });
        } else if let Some(u) = &mut self.unreadable
            && u.session.as_deref() != session
        {
            // Blocks of one reply that name different sessions leave the file's unsure.
            u.session = None;
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

    /// Notes a reply's dollars at its `price`, and its model's `base` price for its
    /// session: the lowest of a subagent session, or the highest of a main session.
    fn session(
        &mut self,
        s: &Session,
        session: &str,
        weighted: f64,
        price: Option<f64>,
        base: Option<f64>,
    ) {
        if s.subagent {
            let sub = self.sub.get_or_insert_with(Sub::default);
            if sub.parent.is_none() {
                sub.parent = Some(session.to_owned());
            }
            sub.cost.add(weighted, price);
            match base {
                Some(p) => sub.lowest = Some(sub.lowest.map_or(p, |l| l.min(p))),
                None => sub.unsure = true,
            }
        } else {
            if !self.here.contains(session) {
                self.here.insert(session.to_owned());
            }
            match base {
                Some(p) => {
                    let top = self.parents.entry(session.to_owned()).or_insert(p);
                    *top = top.max(p);
                }
                // An unpriced model may be dearer than the priced ones.
                None => {
                    self.unsure.insert(session.to_owned());
                }
            }
        }
    }

    fn rank(&self, sub: &Sub) -> Rank {
        let top = sub
            .parent
            .as_ref()
            .filter(|p| !self.unsure.contains(*p))
            .and_then(|p| self.parents.get(p));
        match (top, sub.lowest) {
            (Some(&top), Some(low)) if !sub.unsure => {
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
            m.read_by(&s, *price);
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
}
