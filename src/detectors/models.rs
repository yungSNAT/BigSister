//! Input-side cost of Claude replies by scope, model, reasoning effort and subagent type,
//! and which subagent sessions ran on their parent session's highest-priced model.

mod report;
#[cfg(test)]
mod tests;

pub(crate) use report::DiffTotals;
use report::notes;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use jiff::tz::TimeZone;
use serde_json::{Value, json};

use super::cache::{Entry, Usage, count, field, object, read, read_usage};
use super::tools::printed;
use crate::prices::Prices;
use crate::report::{Opts, Section};
use crate::scan::{Agent, Detector, Record, Session};
use crate::time::parse_ts;

/// Cache reads as a multiple of the uncached input price, by the exact model id, from
/// <https://platform.claude.com/docs/en/about-claude/pricing> as read on 24 November 2026.
pub(super) fn read_multiplier(model: &str) -> f64 {
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

/// A Git branch as printed: segments [`printed`] prints as they are, joined by `/`, up
/// to 128 characters; anything else is `(other)`.
fn printed_branch(name: &str) -> String {
    let fits = name.len() <= 128 && name.split('/').all(|part| printed(part) == part);
    if fits { name } else { "(other)" }.to_owned()
}

/// A label `v` records under `key`, if it is a string other than empty; anything else
/// counts as unrecorded rather than skipping the reply.
fn optional<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
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

/// Replies of one scope, model, effort and agent type, or of one Git branch.
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

impl Group {
    fn add(&mut self, reply: &Reply<'_>, weighted: f64, price: Option<f64>) {
        let u = &reply.usage;
        self.replies += 1;
        self.input = self.input.saturating_add(reply.input);
        self.write = self.write.saturating_add(u.write);
        self.read = self.read.saturating_add(u.read);
        self.output = self.output.saturating_add(reply.output);
        self.cost.add(weighted, price);
        self.unpriced += u64::from(price.is_none());
    }
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
    overlap: crate::otlp::Overlap,
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
    /// By Git branch, as the reply's record gives it.
    branches: BTreeMap<String, Group>,
    /// By the local day of the reply's time, `YYYY-MM-DD`.
    days: BTreeMap<String, Group>,
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
    // nosemgrep: codacy.yaml.security.hard-coded-tokens -- a count of model tokens, not a credential
    unpriced_tokens: u64,
    /// Trace-qualified ids (`traceId/spanId`) of dropped OTLP GenAI spans already counted.
    dropped_ids: HashSet<String>,
    /// In-window OTLP GenAI spans excluded by `gen_ai.operation.name`, by sanitised name.
    otlp_dropped: BTreeMap<String, u64>,
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
    /// `gitBranch`, the branch checked out when Claude Code wrote the record.
    branch: Option<&'a str>,
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
        branch: optional(v, "gitBranch"),
        input: field(usage, "input_tokens", count)?,
        output: field(usage, "output_tokens", count)?,
        usage: read_usage(usage)?,
    })
}

pub(crate) fn countable_identity(v: &Value) -> Option<(&str, &str)> {
    read_reply(v).ok().map(|r| (r.model, r.id))
}

/// `object`'s `key` when it is a string, `None` when it is missing, else the key.
fn label<'a>(object: &'a Value, key: &'static str) -> Result<Option<&'a str>, &'static str> {
    object.get(key).map(|v| v.as_str().ok_or(key)).transpose()
}

fn models_wants(s: &Session, line: &str) -> bool {
    line.contains("\"type\":\"assistant\"")
        || (s.agent == Agent::Otlp && line.contains("\"type\":\"otlp_dropped\""))
}

impl Detector for Models {
    fn export_keys(&mut self, keys: std::rc::Rc<crate::otlp::ExportKeys>) {
        self.overlap.enable(keys);
    }

    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Otlp]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        models_wants(s, line)
    }

    fn scans_whole_file(&self) -> bool {
        true
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

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>, _in_window: bool) {
        self.price = usd_per_mtok;
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        if s.agent == Agent::Otlp && r.kind() == "otlp_dropped" {
            self.record_otlp_dropped(r);
            return;
        }
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
        if !r.in_window || self.counted_counterpart(s, &reply) {
            return;
        }
        self.record_reply(s, r.value, &reply, price);
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
            (
                "otlp_non_inference_spans",
                json!(self.otlp_dropped.values().sum::<u64>()),
            ),
        ];
        Section {
            name: "models",
            title: "input-side cost by scope, model, reasoning effort and subagent type (Claude)",
            summary: summary
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
            tables: self.tables(opts),
            notes: notes(opts, !self.otlp_dropped.is_empty()),
        }
    }
}

/// The local day of `record`'s time, `YYYY-MM-DD`, or [`NONE`] when it has none that reads.
fn local_day(record: &Value) -> String {
    record
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_ts)
        .map_or_else(
            || NONE.to_owned(),
            |t| t.to_zoned_datetime(TimeZone::system()).date().to_string(),
        )
}

/// Dollars rounded to cents.
fn cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}
