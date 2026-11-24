//! Diff snapshot model, delta formatting and section construction for `bigsister diff`.

use serde_json::{Value, json};

use crate::report::{Section, Table};

/// Extracted totals for one side of the comparison.
#[derive(Default)]
pub(super) struct Snapshot {
    pub turns: u64,
    pub input_total: u64,
    pub output_total: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cache_hit_pct: Option<f64>,
    pub sub_ttl_busts: u64,
    pub sub_ttl_bust_usd: Option<f64>,
    pub input_usd: f64,
    pub unpriced_replies: u64,
    pub skipped_replies: u64,
    pub compactions: u64,
    pub extreme_compactions: u64,
    pub compaction_thrashing: u64,
    pub reread_est: u64,
    pub poll_est: u64,
    pub truncation_reruns: u64,
    pub idle_schema_est: u64,
    pub waste_est: u64,
    pub waste_usd: f64,
}

pub(super) fn build_section(
    before_label: &str,
    after_label: &str,
    a: &Snapshot,
    b: &Snapshot,
    basis: &str,
    limit: usize,
) -> Section {
    let tok_pct = u64_pct(a.input_total, b.input_total);
    let has_unpriced = a.unpriced_replies > 0 || b.unpriced_replies > 0;
    let usd_pct = (!has_unpriced)
        .then(|| f64_pct(a.input_usd, b.input_usd))
        .flatten();
    let divergence_pp = tok_pct
        .zip(usd_pct)
        .map_or(Value::Null, |(t, u)| json!(round2(u - t)));
    let mut table = Table::new(
        "Before vs after",
        vec!["metric", "before", "after", "delta", "delta_pct"],
    );
    table.rows = diff_rows(a, b, usd_pct);
    table.truncate(limit);
    Section {
        name: "diff",
        title: "before vs after comparison of tokens, cache hit %, cost, turns, compactions and waste",
        summary: vec![
            ("before".to_owned(), json!(before_label)),
            ("after".to_owned(), json!(after_label)),
            ("usd_basis".to_owned(), json!(basis)),
            (
                "before_unpriced_replies".to_owned(),
                json!(a.unpriced_replies),
            ),
            (
                "after_unpriced_replies".to_owned(),
                json!(b.unpriced_replies),
            ),
            (
                "before_skipped_replies".to_owned(),
                json!(a.skipped_replies),
            ),
            ("after_skipped_replies".to_owned(), json!(b.skipped_replies)),
            ("input_tokens_delta_pct".to_owned(), opt_json(tok_pct)),
            ("input_usd_delta_pct".to_owned(), opt_json(usd_pct)),
            ("usd_token_divergence_pp".to_owned(), divergence_pp),
        ],
        tables: vec![table],
        notes: build_notes(has_unpriced || a.skipped_replies > 0 || b.skipped_replies > 0),
    }
}

fn build_notes(floor: bool) -> Vec<String> {
    let mut notes = vec![
        "Compares 2 session folders or half-open time windows [START, END) side by side. Delta is after minus before; delta pct is percentage change relative to before (null when before is 0, or for input usd when either side has unpriced replies).".to_owned(),
        "Usd token divergence pp is input usd delta pct minus input tokens delta pct: a positive gap means dollars rose faster (or fell less) than raw tokens, such as after sub-TTL cache busts. Turns, token counts, cache hit %, sub-TTL cache busts, input usd est and idle schema tokens come from Claude detectors; compactions, re-reads, polls and truncation re-runs cover every scanned agent those detectors read. Waste tokens est and waste usd est sum polls, compaction re-reads and idle schemas/skills (which can overlap, as in ranked). Before/after unpriced replies and skipped replies report Claude replies left out of input usd est; an illustration, not a bill.".to_owned(),
    ];
    if floor {
        notes.push(
            "Floor total: unpriced or skipped Claude replies are omitted from input usd est on one or both sides, so input usd percentage change and usd token divergence pp are null when replies are unpriced.".to_owned(),
        );
    }
    notes
}

fn diff_rows(a: &Snapshot, b: &Snapshot, usd_pct: Option<f64>) -> Vec<Vec<Value>> {
    vec![
        row_u64("turns", a.turns, b.turns),
        row_u64("input_tokens", a.input_total, b.input_total),
        row_u64("output_tokens", a.output_total, b.output_total),
        row_u64("cache_read_tokens", a.cache_read, b.cache_read),
        row_u64("cache_write_tokens", a.cache_write, b.cache_write),
        row_opt_f64("cache_hit_pct", a.cache_hit_pct, b.cache_hit_pct),
        row_u64("sub_ttl_cache_busts", a.sub_ttl_busts, b.sub_ttl_busts),
        row_opt_usd(
            "sub_ttl_bust_usd_est",
            a.sub_ttl_bust_usd,
            b.sub_ttl_bust_usd,
        ),
        row_f64_pct("input_usd_est", a.input_usd, b.input_usd, usd_pct),
        row_u64("compactions", a.compactions, b.compactions),
        row_u64(
            "extreme_compactions",
            a.extreme_compactions,
            b.extreme_compactions,
        ),
        row_u64(
            "compaction_thrashing",
            a.compaction_thrashing,
            b.compaction_thrashing,
        ),
        row_u64("reread_tokens_est", a.reread_est, b.reread_est),
        row_u64("poll_tokens_est", a.poll_est, b.poll_est),
        row_u64(
            "truncation_reruns",
            a.truncation_reruns,
            b.truncation_reruns,
        ),
        row_u64(
            "idle_schema_tokens_est",
            a.idle_schema_est,
            b.idle_schema_est,
        ),
        row_u64("waste_tokens_est", a.waste_est, b.waste_est),
        row_f64_pct(
            "waste_usd_est",
            a.waste_usd,
            b.waste_usd,
            f64_pct(a.waste_usd, b.waste_usd),
        ),
    ]
}

fn delta_value(delta: i128) -> Value {
    if let Ok(n) = i64::try_from(delta) {
        Value::from(n)
    } else if let Ok(n) = u64::try_from(delta) {
        Value::from(n)
    } else {
        Value::String(delta.to_string())
    }
}

fn row_u64(metric: &str, before: u64, after: u64) -> Vec<Value> {
    let delta = i128::from(after) - i128::from(before);
    vec![
        json!(metric),
        json!(before),
        json!(after),
        delta_value(delta),
        opt_json(u64_pct(before, after)),
    ]
}

fn row_f64_pct(metric: &str, before: f64, after: f64, pct: Option<f64>) -> Vec<Value> {
    vec![
        json!(metric),
        round2_json(before),
        round2_json(after),
        round2_json(after - before),
        opt_json(pct),
    ]
}

fn row_opt_usd(metric: &str, before: Option<f64>, after: Option<f64>) -> Vec<Value> {
    let delta = before.zip(after).map(|(b, a)| a - b);
    let pct = before.zip(after).and_then(|(b, a)| f64_pct(b, a));
    vec![
        json!(metric),
        before.map_or(Value::Null, round2_json),
        after.map_or(Value::Null, round2_json),
        delta.map_or(Value::Null, round2_json),
        opt_json(pct),
    ]
}

fn row_opt_f64(metric: &str, before: Option<f64>, after: Option<f64>) -> Vec<Value> {
    let delta = before.zip(after).map(|(b, a)| round2(a - b));
    vec![
        json!(metric),
        opt_json(before),
        opt_json(after),
        opt_json(delta),
        Value::Null,
    ]
}

fn u64_pct(before: u64, after: u64) -> Option<f64> {
    if before == 0 {
        return None;
    }
    #[expect(clippy::cast_precision_loss, reason = "counts stay far below 2^52")]
    let pct = ((after as f64 - before as f64) / before as f64) * 100.0;
    Some(round2(pct))
}

fn f64_pct(before: f64, after: f64) -> Option<f64> {
    if before <= 0.0 || !before.is_finite() || !after.is_finite() {
        return None;
    }
    let pct = ((after - before) / before) * 100.0;
    (pct.is_finite() && (pct * 100.0).is_finite()).then(|| round2(pct))
}

fn opt_json(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |x| json!(x))
}

fn round2_json(v: f64) -> Value {
    if v.is_finite() && (v * 100.0).is_finite() {
        json!(round2(v))
    } else {
        Value::Null
    }
}

pub(super) fn round2(v: f64) -> f64 {
    let r = (v * 100.0).round() / 100.0;
    if r == 0.0 { 0.0 } else { r }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_u64_converts_extreme_deltas_without_panicking() {
        let neg = row_u64("x", u64::MAX, 0);
        assert_eq!(neg[3], json!("-18446744073709551615"));

        let pos = row_u64("x", 0, u64::MAX);
        assert_eq!(pos[3], json!(u64::MAX));

        let mid = row_u64("x", 10, 4);
        assert_eq!(mid[3], json!(-6));
    }

    #[test]
    fn non_finite_usd_cells_render_as_null() {
        let row = row_opt_usd("u", Some(f64::INFINITY), None);
        assert_eq!(row[1], Value::Null);
        assert_eq!(row[2], Value::Null);
        assert_eq!(row[3], Value::Null);
        assert_eq!(row[4], Value::Null);
    }
}
