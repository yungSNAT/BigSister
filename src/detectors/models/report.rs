//! The section's notes, its totals and its tables.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use serde_json::{Value, json};

use super::*;
use crate::detectors::cache::input_price;
use crate::prices::Prices;
use crate::report::{Opts, Table};
use crate::scan::Session;

/// The section's notes.
pub(super) fn notes(opts: &Opts) -> Vec<String> {
    let price = input_price(opts);
    let mut notes = vec![
        format!("Each reply counts once (the first record of its message.id); synthetic replies are left out, and a reply none of whose records reads whole (a message with an id, a model and a usage whose input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens and, when it has a cache_creation split, ephemeral_5m_input_tokens and ephemeral_1h_input_tokens are integers from 0 to 2^40; a session id; and effort and agent type as text when recorded) counts as skipped (replies skipped) and adds to no total, so with any skipped the totals are a floor. Effort is the reasoning effort the reply records and agent type the subagent type Claude Code records (attributionAgent); {NONE} marks a reply that records neither."),
        format!("Input usd est is an input-side estimate, not spend: uncached input at the input price, cache writes at 1.25 times it (5-minute) or 2 times (1-hour), as the reply records (1.25 times for replies without split, which record none), and cache reads at the model's cache read price, 0.025, 0.05 or 0.1 times the input price as Anthropic's pricing page gave on 24 November 2026, where the input price is {price}. Output tokens are counted but not priced, as bigsister reads input prices only; output costs several times as much as input per token. Input usd share is each row's part of all rows' input usd est, rounded to 2 places, so shares need not sum to 1. Unpriced replies are those of a model without a price, or of one priced by prompt size whose prompt size is not recorded; dollars and shares leave them out, and replies unpriced and unpriced input tokens (their input, cache write and cache read tokens) count them whatever --limit cuts. An illustration, not a bill."),
        "Each model is ranked at its base input price (its price for the shortest prompt, so a model priced by prompt size ranks the same whatever the prompt), from the bundled prices or the price file's with --prices, whatever --usd-per-mtok gives: a subagent session is on its parent's top model when the cheapest model of its replies in the window costs at least the dearest of its parent session's, so the same model on both sides is on top, and below when cheaper. It is indeterminate when the parent session (the main session whose id its records carry) was not read or had no reply in the window, or it or the subagent had a reply in the window skipped or on a model without a price (a skipped one without a session id, or whose blocks name different sessions, counts against every session its file's records in the window name, and against none, counting only in replies skipped, when no record names one), as such a reply's model may be dearer or cheaper. On top input usd est sums those sessions' input usd est.".to_owned(),
    ];
    if opts.by_branch {
        notes.push(format!("By Git branch (--by-branch) groups the same replies by the gitBranch Claude Code records on each reply, the branch checked out in its working folder when it was written, not the branch its work was later merged to; {NONE} marks a reply that records none, an empty one or one that is not text, and (other) a branch whose name holds characters other than letters, digits and _, -, ., : or / between non-empty parts, or is over 128 characters long. Its dollars are the same estimate as above, rounded to cents in each row, so its total can differ from the first table's by a few cents."));
    }
    if opts.by_day {
        notes.push(format!("By local day (--by-day) groups the same replies by the day of each reply's recorded time in this machine's time zone (UTC when it cannot be found), latest first, so --limit keeps the latest days; {NONE} marks a reply whose time does not read. Its dollars are the same estimate as above, rounded to cents in each row."));
    }
    notes
}

/// Unrounded aggregate input-side USD and completeness counts for comparison reports.
#[derive(Clone, Copy, Default)]
pub(crate) struct DiffTotals {
    pub input_usd: f64,
    pub unpriced_replies: u64,
    pub skipped_replies: u64,
}

impl Models {
    /// Counts `reply`, of record `v`, under its Git branch and its local day.
    pub(super) fn add_by_branch_and_day(
        &mut self,
        v: &Value,
        reply: &Reply<'_>,
        weighted: f64,
        price: Option<f64>,
    ) {
        self.branches
            .entry(reply.branch.map_or_else(|| NONE.to_owned(), printed_branch))
            .or_default()
            .add(reply, weighted, price);
        self.days
            .entry(local_day(v))
            .or_default()
            .add(reply, weighted, price);
    }

    /// The first table, then the tables by branch and by day when asked for.
    pub(super) fn tables(&self, opts: &Opts) -> Vec<Table> {
        [
            Some(self.table(opts)),
            opts.by_branch.then(|| self.branch_table(opts)),
            opts.by_day.then(|| self.day_table(opts)),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Unrounded input-side USD total and unpriced/skipped reply counts across all groups.
    pub(crate) fn diff_totals(&self, rate: Option<f64>) -> DiffTotals {
        let input_usd: f64 = self.groups.values().map(|g| g.cost.usd(rate)).sum();
        let unpriced_replies = if rate.is_some() {
            0
        } else {
            self.groups.values().map(|g| g.unpriced).sum()
        };
        DiffTotals {
            input_usd,
            unpriced_replies,
            skipped_replies: self.skipped,
        }
    }

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
            branches: BTreeMap::new(),
            days: BTreeMap::new(),
            parents: HashMap::new(),
            unsure: HashSet::new(),
            subs: Vec::new(),
            unreadable: None,
            skipped: 0,
            without_split: 0,
            // nosemgrep: codacy.yaml.security.hard-coded-tokens -- a count of model tokens, not a credential
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
    pub(super) fn skip(&mut self) {
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
    pub(super) fn unreadable(&mut self, id: Option<&str>, in_window: bool, session: Option<&str>) {
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
    pub(super) fn readable(&mut self, id: &str) {
        if self.pending(Some(id)) {
            self.unreadable = None;
        } else {
            self.skip();
        }
        self.ids.insert(id.to_owned());
    }

    /// Notes a reply's dollars at its `price`, and its model's `base` price for its
    /// session: the lowest of a subagent session, or the highest of a main session.
    pub(super) fn session(
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

    pub(super) fn rank(&self, sub: &Sub) -> Rank {
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
        let groups = self
            .groups
            .iter()
            .map(|((scope, model, effort, agent), g)| {
                (
                    vec![json!(scope), json!(model), json!(effort), json!(agent)],
                    g,
                )
            });
        group_table(
            "By scope, model, effort and agent type, most input dollars first",
            &["scope", "model", "effort", "agent_type"],
            groups,
            opts,
        )
    }

    /// The table by Git branch, most input dollars first.
    fn branch_table(&self, opts: &Opts) -> Table {
        let groups = self.branches.iter().map(|(b, g)| (vec![json!(b)], g));
        group_table(
            "By Git branch, most input dollars first",
            &["branch"],
            groups,
            opts,
        )
    }

    /// The days, latest first, so that `--limit` keeps the latest.
    pub(super) fn day_table(&self, opts: &Opts) -> Table {
        let groups = self.days.iter().map(|(d, g)| (vec![json!(d)], g));
        let all = Opts {
            limit: usize::MAX,
            ..opts.clone()
        };
        let mut table = group_table("By local day, latest first", &["day"], groups, &all);
        table.rows.sort_by(|a, b| b[0].as_str().cmp(&a[0].as_str()));
        table.truncate(opts.limit);
        table
    }
}

/// A table of `groups`, each with its key cells, under `keys` and the counts' columns,
/// most input dollars first.
pub(super) fn group_table<'a>(
    title: &str,
    keys: &[&'static str],
    groups: impl Iterator<Item = (Vec<Value>, &'a Group)>,
    opts: &Opts,
) -> Table {
    let rate = opts.usd_per_mtok;
    let groups: Vec<_> = groups.collect();
    let total: f64 = groups.iter().map(|(_, g)| g.cost.usd(rate)).sum();
    let mut rows: Vec<(f64, Vec<Value>)> = groups
        .into_iter()
        .map(|(mut row, g)| {
            let usd = g.cost.usd(rate);
            let share = (total > 0.0).then(|| cents(usd / total));
            row.extend([
                json!(g.replies),
                json!(g.input),
                json!(g.write),
                json!(g.read),
                json!(g.output),
                json!(cents(usd)),
                json!(share),
                json!(if rate.is_some() { 0 } else { g.unpriced }),
            ]);
            (usd, row)
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut columns = keys.to_vec();
    columns.extend([
        "replies",
        "input_tokens",
        "cache_write_tokens",
        "cache_read_tokens",
        "output_tokens",
        "input_usd_est",
        "input_usd_share",
        "unpriced_replies",
    ]);
    let mut table = Table::new(title, columns);
    table.rows = rows.into_iter().map(|(_, r)| r).collect();
    table.truncate(opts.limit);
    table
}
