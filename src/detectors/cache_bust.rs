//! Sub-TTL (< 5 minute) cache-bust detection, cause attribution and token-% vs dollar-%
//! divergence for Claude Code transcripts.

use std::collections::{BTreeMap, HashSet};

use jiff::SignedDuration;

use serde_json::{Value, json};

use super::models::read_multiplier;
use super::{name_of, tool_uses};
use crate::report::{Opts, Table, ratio};
use crate::scan::Record;

/// Threshold below which a cache rebuild cannot be explained by the 5-minute TTL.
const SUB_TTL_SECS: i64 = 5 * 60;

/// Tool calls that load new tool definitions or swap modes and can invalidate a prefix.
const BUST_COLUMNS: [&str; 10] = [
    "scope",
    "cause",
    "busts",
    "rebuilt_tokens",
    "bust_token_share",
    "write_usd_est",
    "penalty_usd_est",
    "bust_usd_share",
    "usd_to_token_ratio",
    "unpriced_tokens",
];

const TOOL_LOAD_CALLS: [&str; 3] = ["ToolSearch", "ExitPlanMode", "EnterPlanMode"];

/// Likely cause of a sub-TTL (< 5 min) cache bust, in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum BustCause {
    ModelSwap,
    ToolLoad,
    MidHistoryEdit,
    ProxyOrPrefix,
}

impl BustCause {
    fn label(self) -> &'static str {
        match self {
            BustCause::ModelSwap => "model swap",
            BustCause::ToolLoad => "tool load / schema change",
            BustCause::MidHistoryEdit => "mid-history edit / rewind",
            BustCause::ProxyOrPrefix => "proxy or prefix change",
        }
    }
}

/// Inputs from one decoded Claude reply for sub-TTL cache-bust attribution.
pub(super) struct ReplyCtx<'a> {
    pub scope: &'static str,
    pub in_window: bool,
    pub model: &'a str,
    pub prev_model: Option<&'a str>,
    pub gap: Option<SignedDuration>,
    pub uncached_input: u64,
    pub read: u64,
    pub write: u64,
    pub rebuilt: u64,
    pub write_mult: f64,
    pub price: Option<f64>,
    pub fresh: bool,
    pub value: &'a Value,
}

/// Aggregated sub-TTL cache busts for one `(scope, cause)` pair.
#[derive(Default, Clone, Copy)]
struct BustRow {
    count: u64,
    rebuilt: u64,
    write_weighted: f64,
    write_weighted_usd: f64,
    warm_weighted: f64,
    warm_weighted_usd: f64,
    unpriced: u64,
}

impl BustRow {
    fn add(&mut self, rebuilt: u64, write_mult: f64, read_mult: f64, price: Option<f64>) {
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let n = rebuilt as f64;
        let write_w = n * write_mult;
        let warm_w = n * read_mult;
        self.count += 1;
        self.rebuilt = self.rebuilt.saturating_add(rebuilt);
        self.write_weighted += write_w;
        self.warm_weighted += warm_w;
        match price {
            Some(usd) => {
                self.write_weighted_usd += write_w * usd;
                self.warm_weighted_usd += warm_w * usd;
            }
            None => self.unpriced = self.unpriced.saturating_add(rebuilt),
        }
    }

    fn write_usd(&self, rate: Option<f64>) -> f64 {
        rate.map_or(self.write_weighted_usd, |r| self.write_weighted * r) / 1e6
    }

    fn penalty_usd(&self, rate: Option<f64>) -> f64 {
        let warm = rate.map_or(self.warm_weighted_usd, |r| self.warm_weighted * r) / 1e6;
        (self.write_usd(rate) - warm).max(0.0)
    }

    fn unpriced_for(&self, rate: Option<f64>) -> u64 {
        if rate.is_some() { 0 } else { self.unpriced }
    }
}

/// Tracks sub-TTL cache busts and total input token vs dollar shares across sessions.
#[derive(Default)]
pub(super) struct Tracker {
    last_shape: Option<String>,
    last_msg_count: Option<usize>,
    seen_uuids: HashSet<String>,
    prev_reply_uuid: Option<String>,
    tool_load_since_reply: bool,
    mid_edit_since_reply: bool,
    total_input: u64,
    total_weighted: f64,
    total_weighted_usd: f64,
    busts: BTreeMap<(&'static str, BustCause), BustRow>,
}

impl Tracker {
    pub(super) fn begin(&mut self) {
        self.last_shape = None;
        self.last_msg_count = None;
        self.seen_uuids.clear();
        self.prev_reply_uuid = None;
        self.tool_load_since_reply = false;
        self.mid_edit_since_reply = false;
    }

    pub(super) fn on_compaction(&mut self) {
        self.tool_load_since_reply = false;
        self.mid_edit_since_reply = false;
        self.last_msg_count = None;
    }

    /// Inspects non-reply metadata records that explain why a subsequent prefix changed.
    pub(super) fn observe_meta(&mut self, r: &Record<'_>) {
        match r.kind() {
            "api-request-shape" => self.note_shape(r.value.get("id").and_then(Value::as_str)),
            "api-request" => self.note_api_request(r.value),
            "attachment" => {
                let kind = r
                    .value
                    .pointer("/attachment/type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.tool_load_since_reply |= matches!(
                    kind,
                    "deferred_tools_delta" | "mcp_tools_delta" | "tools_delta"
                );
            }
            "system" => {
                let sub = r.value.get("subtype").and_then(Value::as_str).unwrap_or("");
                self.mid_edit_since_reply |= sub.contains("rewind") || sub.contains("microcompact");
            }
            _ => {}
        }
    }

    /// Records tool-load calls on extra blocks of an already-counted reply.
    pub(super) fn observe_tool_load(&mut self, r: &Record<'_>) {
        if has_tool_load_call(r) {
            self.tool_load_since_reply = true;
        }
    }

    pub(super) fn on_reply(&mut self, r: &Record<'_>, ctx: ReplyCtx<'_>) {
        let read_mult = read_multiplier(ctx.model);
        if ctx.in_window {
            self.add_total(&ctx, read_mult);
            if !ctx.fresh
                && ctx.rebuilt > 0
                && ctx.gap.is_some_and(|g| {
                    g >= SignedDuration::ZERO && g < SignedDuration::from_secs(SUB_TTL_SECS)
                })
            {
                let cause = self.classify(&ctx);
                self.busts.entry((ctx.scope, cause)).or_default().add(
                    ctx.rebuilt,
                    ctx.write_mult,
                    read_mult,
                    ctx.price,
                );
            }
        }
        if let Some(prev) = self.prev_reply_uuid.take() {
            self.seen_uuids.insert(prev);
        }
        self.prev_reply_uuid = ctx
            .value
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.tool_load_since_reply = has_tool_load_call(r);
        self.mid_edit_since_reply = false;
    }

    pub(super) fn summary(&self, opts: &Opts) -> Vec<(String, Value)> {
        let count: u64 = self.busts.values().map(|b| b.count).sum();
        let rebuilt: u64 = self
            .busts
            .values()
            .map(|b| b.rebuilt)
            .fold(0, u64::saturating_add);
        let write_usd: f64 = self
            .busts
            .values()
            .map(|b| b.write_usd(opts.usd_per_mtok))
            .sum();
        let penalty_usd: f64 = self
            .busts
            .values()
            .map(|b| b.penalty_usd(opts.usd_per_mtok))
            .sum();
        let total_usd = self.total_usd(opts.usd_per_mtok);
        vec![
            ("sub_ttl_busts".to_owned(), json!(count)),
            ("sub_ttl_bust_tokens".to_owned(), json!(rebuilt)),
            (
                "sub_ttl_bust_token_share".to_owned(),
                ratio(rebuilt, self.total_input),
            ),
            (
                "sub_ttl_bust_usd_share".to_owned(),
                float_ratio(write_usd, total_usd),
            ),
            (
                "sub_ttl_bust_penalty_usd_est".to_owned(),
                json!(round_cents(penalty_usd)),
            ),
        ]
    }

    pub(super) fn table(&self, opts: &Opts) -> Table {
        let total_usd = self.total_usd(opts.usd_per_mtok);
        let mut rows: Vec<_> = self
            .busts
            .iter()
            .map(|(&key, b)| self.format_row(key, b, opts.usd_per_mtok, total_usd))
            .collect();
        rows.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let mut table = Table::new("Sub-TTL cache busts (< 5 min)", BUST_COLUMNS.to_vec());
        table.rows = rows.into_iter().map(|(_, _, _, r)| r).collect();
        table.truncate(opts.limit);
        table
    }

    fn format_row(
        &self,
        key: (&'static str, BustCause),
        b: &BustRow,
        rate: Option<f64>,
        total_usd: f64,
    ) -> (f64, u64, (&'static str, BustCause), Vec<Value>) {
        let write_usd = b.write_usd(rate);
        let penalty_usd = b.penalty_usd(rate);
        let row = vec![
            json!(key.0),
            json!(key.1.label()),
            json!(b.count),
            json!(b.rebuilt),
            ratio(b.rebuilt, self.total_input),
            json!(round_cents(write_usd)),
            json!(round_cents(penalty_usd)),
            float_ratio(write_usd, total_usd),
            divergence(b.rebuilt, self.total_input, write_usd, total_usd),
            json!(b.unpriced_for(rate)),
        ];
        (penalty_usd, b.rebuilt, key, row)
    }

    fn note_shape(&mut self, id: Option<&str>) {
        if self.last_shape.is_some() && id.is_none_or(|s| self.last_shape.as_deref() != Some(s)) {
            self.tool_load_since_reply = true;
        }
        if let Some(id) = id {
            self.last_shape = Some(id.to_owned());
        }
    }

    fn note_api_request(&mut self, v: &Value) {
        if let Some(shape) = v.get("shape").and_then(Value::as_str) {
            self.note_shape(Some(shape));
        }
        if let Some(keep) = v
            .get("keep")
            .and_then(Value::as_u64)
            .and_then(|k| usize::try_from(k).ok())
        {
            if self.last_msg_count.is_some_and(|prev| keep < prev) {
                self.mid_edit_since_reply = true;
            }
            let tail = v.get("tail").and_then(Value::as_array).map_or(0, Vec::len);
            self.last_msg_count = Some(keep.saturating_add(tail));
        }
    }

    fn classify(&self, ctx: &ReplyCtx<'_>) -> BustCause {
        if ctx.prev_model.is_some_and(|pm| pm != ctx.model) {
            BustCause::ModelSwap
        } else if self.tool_load_since_reply {
            BustCause::ToolLoad
        } else if self.mid_edit_since_reply || is_rewind_reply(ctx.value, &self.seen_uuids) {
            BustCause::MidHistoryEdit
        } else {
            BustCause::ProxyOrPrefix
        }
    }

    fn add_total(&mut self, ctx: &ReplyCtx<'_>, read_mult: f64) {
        let sum = ctx
            .uncached_input
            .saturating_add(ctx.read)
            .saturating_add(ctx.write);
        self.total_input = self.total_input.saturating_add(sum);
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts stay far below 2^52"
        )]
        let weighted = ctx.uncached_input as f64
            + ctx.write as f64 * ctx.write_mult
            + ctx.read as f64 * read_mult;
        self.total_weighted += weighted;
        if let Some(usd) = ctx.price {
            self.total_weighted_usd += weighted * usd;
        }
    }

    fn total_usd(&self, rate: Option<f64>) -> f64 {
        rate.map_or(self.total_weighted_usd, |r| self.total_weighted * r) / 1e6
    }
}

fn has_tool_load_call(r: &Record<'_>) -> bool {
    tool_uses(r).any(|b| TOOL_LOAD_CALLS.contains(&name_of(b)))
}

fn is_rewind_reply(v: &Value, seen_uuids: &HashSet<String>) -> bool {
    v.get("parentUuid")
        .and_then(Value::as_str)
        .is_some_and(|p| seen_uuids.contains(p))
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

fn float_ratio(num: f64, den: f64) -> Value {
    if den <= 0.0 {
        return Value::Null;
    }
    json!(((num / den) * 100.0).round() / 100.0)
}

fn divergence(rebuilt: u64, total_input: u64, write_usd: f64, total_usd: f64) -> Value {
    if rebuilt == 0 || total_input == 0 || write_usd <= 0.0 || total_usd <= 0.0 {
        return Value::Null;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "token counts stay far below 2^52"
    )]
    let token_share = rebuilt as f64 / total_input as f64;
    let usd_share = write_usd / total_usd;
    json!(((usd_share / token_share) * 100.0).round() / 100.0)
}

/// Note explaining the sub-TTL cache-bust table and token-vs-dollar divergence.
pub(super) const BUST_NOTE: &str = "Sub-TTL cache busts (< 5 min) flag replies that rebuilt cached tokens less than 5 minutes after the previous reply, before the shortest cache lifetime can expire, classified from transcript metadata into model swap, tool load / schema change (changed request shape, deferred tool delta, ToolSearch or plan-mode switch), mid-history edit / rewind (reduced api-request keep, rewind/microcompact system record or earlier parentUuid) or proxy or prefix change. Penalty usd est subtracts what warm cache reads would have cost at each model's cache read multiplier from write usd est; bust token share vs bust usd share and usd to token ratio show how cache-weighted dollars diverge from raw token share.";

/// Extra transcript lines needed for sub-TTL cache-bust attribution.
pub(super) fn wants_meta(line: &str) -> bool {
    line.contains("\"api-request\"")
        || line.contains("\"api-request-shape\"")
        || line.contains("\"deferred_tools_delta\"")
        || line.contains("\"mcp_tools_delta\"")
        || line.contains("\"tools_delta\"")
        || line.contains("\"rewind")
        || line.contains("\"microcompact")
}
