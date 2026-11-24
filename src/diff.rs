//! Side-by-side comparison (`bigsister diff <before> <after>`) across 2 session folders
//! or time windows so configuration and hook changes can be verified in numbers.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use jiff::Timestamp;
use serde_json::{Value, json};

use crate::cli::Cli;
use crate::detectors::{cache, models, polls, rereads, schemas};
use crate::prices::Prices;
use crate::programs::Programs;
use crate::report::{Meta, Opts, Section, Table, display_path, rfc3339};
use crate::scan::{Agent, Detector, Memory, RootStats, scan_memory_range, scan_range};
use crate::{
    chosen, folder_problem, price, read_prices, render_report, resolve_roots, sample, time,
};

/// Parsed `<before>` or `<after>` target.
enum Target {
    Window {
        since: Timestamp,
        until: Timestamp,
        label: String,
    },
    Dir {
        path: PathBuf,
        label: String,
    },
}

struct Measured {
    snap: Snapshot,
    stats: Vec<RootStats>,
    skipped: Vec<String>,
    basis: &'static str,
}

/// Extracted totals for one side of the comparison.
#[derive(Default)]
struct Snapshot {
    turns: u64,
    input_total: u64,
    output_total: u64,
    cache_read: u64,
    cache_write: u64,
    cache_hit_pct: Option<f64>,
    sub_ttl_busts: u64,
    sub_ttl_bust_usd: f64,
    input_usd: f64,
    compactions: u64,
    extreme_compactions: u64,
    compaction_thrashing: u64,
    reread_est: u64,
    poll_est: u64,
    truncation_reruns: u64,
    idle_schema_est: u64,
    waste_est: u64,
    waste_usd: f64,
}

/// Runs `bigsister diff <before> <after>` and returns the rendered report.
///
/// # Errors
/// Returns a message when a target window or folder, or `--prices`, is invalid.
pub fn run(cli: &Cli, before: &str, after: &str) -> Result<String, String> {
    let now = Timestamp::now();
    let home = std::env::home_dir().map(|h| display_path(&h.display().to_string(), None));
    let left = parse_target(before, now, home.as_deref())?;
    let right = parse_target(after, now, home.as_deref())?;
    if cli.sample && (matches!(left, Target::Dir { .. }) || matches!(right, Target::Dir { .. })) {
        return Err(
            "--sample cannot be used with session folders; pass START..END time windows instead"
                .to_owned(),
        );
    }
    let prices = Rc::new(read_prices(cli, home.as_deref())?);
    let programs = Rc::new(Programs::from_env());
    let opts = Opts {
        limit: usize::MAX,
        paths: false,
        by_branch: false,
        home: home.clone(),
        usd_per_mtok: cli.usd_per_mtok,
        prices_file: prices.file().map(str::to_owned),
    };
    let mut a = measure(cli, &left, home.as_deref(), &prices, &programs, &opts)?;
    let b = measure(cli, &right, home.as_deref(), &prices, &programs, &opts)?;
    a.stats.extend(b.stats);
    let extra: Vec<String> = b
        .skipped
        .into_iter()
        .filter(|n| !a.skipped.contains(n))
        .collect();
    a.skipped.extend(extra);
    let section = build_section(
        left.label(),
        right.label(),
        &a.snap,
        &b.snap,
        a.basis,
        cli.limit,
    );
    let meta = Meta {
        since: left.since().min(right.since()),
        roots: &a.stats,
        skipped_roots: &a.skipped,
        home: home.as_deref(),
    };
    Ok(render_report(cli, &meta, &[section]))
}

impl Target {
    fn label(&self) -> &str {
        match self {
            Target::Window { label, .. } | Target::Dir { label, .. } => label,
        }
    }

    fn since(&self) -> Timestamp {
        match self {
            Target::Window { since, .. } => *since,
            Target::Dir { .. } => Timestamp::UNIX_EPOCH,
        }
    }
}

fn parse_target(spec: &str, now: Timestamp, home: Option<&str>) -> Result<Target, String> {
    let path = PathBuf::from(spec);
    if spec != ".."
        && !spec.contains(['/', '\\'])
        && !path.is_dir()
        && let Some((raw_start, raw_end)) = spec.split_once("..")
    {
        let start = parse_bound(raw_start, now, spec)?;
        let end = parse_bound(raw_end, now, spec)?;
        if start >= end {
            return Err(format!("diff window '{spec}': start must be before end"));
        }
        return Ok(Target::Window {
            since: start,
            until: end,
            label: format!("{}..{}", rfc3339(start), rfc3339(end)),
        });
    }
    let label = display_path(&path.display().to_string(), home);
    if let Some(problem) = folder_problem(&path) {
        return Err(format!("diff folder {problem}: {label}"));
    }
    Ok(Target::Dir { path, label })
}

fn parse_bound(raw: &str, now: Timestamp, spec: &str) -> Result<Timestamp, String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    time::parse_since(trimmed, now)
        .map_err(|_| {
            format!(
                "diff window '{spec}': expected START..END using 7d, 24h, YYYY-MM-DD, RFC 3339 or now"
            )
        })?
        .ok_or_else(|| format!("diff window '{spec}': bound '{trimmed}' is out of range"))
}

fn measure(
    cli: &Cli,
    target: &Target,
    home: Option<&str>,
    prices: &Rc<Prices>,
    programs: &Rc<Programs>,
    opts: &Opts,
) -> Result<Measured, String> {
    let mut dets: Vec<Box<dyn Detector>> = vec![
        Box::new(models::Models::new(Rc::clone(prices))),
        Box::<cache::Cache>::default(),
        Box::<rereads::Rereads>::default(),
        Box::new(polls::Polls::new(Rc::clone(programs))),
        Box::<schemas::Schemas>::default(),
    ];
    let (stats, skipped) = scan_target(cli, target, home, prices, &mut dets)?;
    let sections: Vec<Section> = dets
        .iter()
        .map(|d| {
            let mut s = d.section(opts);
            price(cli, prices, &mut s);
            s
        })
        .collect();
    let basis = if cli.usd_per_mtok.is_some() {
        "usd_per_mtok"
    } else if cli.prices.is_some() {
        "prices_file"
    } else {
        "bundled"
    };
    Ok(Measured {
        snap: extract(&sections),
        stats,
        skipped,
        basis,
    })
}

fn scan_target(
    cli: &Cli,
    target: &Target,
    home: Option<&str>,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Result<(Vec<RootStats>, Vec<String>), String> {
    let read: Vec<Agent> = dets.iter().flat_map(|d| d.agents()).copied().collect();
    match target {
        Target::Window { since, until, .. } => {
            if cli.sample {
                let files: Vec<Memory> = sample::FILES
                    .iter()
                    .filter(|f| read.contains(&f.agent) && chosen(cli).is_none_or(|c| c == f.agent))
                    .copied()
                    .collect();
                return Ok((
                    scan_memory_range(
                        &files,
                        sample::ROOT,
                        Some(*since),
                        Some(*until),
                        prices,
                        dets,
                    ),
                    Vec::new(),
                ));
            }
            let (roots, skipped) = resolve_roots(cli, &read, home)?;
            Ok((
                scan_range(&roots, Some(*since), Some(*until), prices, dets),
                skipped,
            ))
        }
        Target::Dir { path, .. } => {
            let roots = dir_roots(cli, path);
            Ok((
                scan_range(&roots, Some(Timestamp::MIN), None, prices, dets),
                Vec::new(),
            ))
        }
    }
}

fn dir_roots(cli: &Cli, path: &Path) -> Vec<(Agent, PathBuf)> {
    if let Some(agent) = chosen(cli) {
        return vec![(agent, path.to_path_buf())];
    }
    let subdirs = [
        (Agent::Claude, "claude"),
        (Agent::Codex, "codex"),
        (Agent::Pi, "pi"),
        (Agent::OpenCode, "opencode"),
        (Agent::Gemini, "gemini"),
        (Agent::Qwen, "qwen"),
    ];
    let found: Vec<(Agent, PathBuf)> = subdirs
        .into_iter()
        .filter_map(|(agent, name)| {
            let sub = path.join(name);
            sub.is_dir().then_some((agent, sub))
        })
        .collect();
    if !found.is_empty() {
        return found;
    }
    if path.join("opencode.db").is_file() {
        return vec![(Agent::OpenCode, path.to_path_buf())];
    }
    vec![(Agent::Claude, path.to_path_buf())]
}

fn extract(sections: &[Section]) -> Snapshot {
    let mut s = Snapshot::default();
    let models = &sections[0];
    s.turns = sum_u64(models, 0, "replies");
    let uncached = sum_u64(models, 0, "input_tokens");
    s.cache_write = sum_u64(models, 0, "cache_write_tokens");
    s.cache_read = sum_u64(models, 0, "cache_read_tokens");
    s.input_total = uncached
        .saturating_add(s.cache_write)
        .saturating_add(s.cache_read);
    s.output_total = sum_u64(models, 0, "output_tokens");
    s.input_usd = sum_f64(models, 0, "input_usd_est");
    if s.input_total > 0 {
        #[expect(clippy::cast_precision_loss, reason = "counts stay far below 2^52")]
        let pct = (s.cache_read as f64 / s.input_total as f64) * 100.0;
        s.cache_hit_pct = Some(round2(pct));
    }

    let cache = &sections[1];
    s.sub_ttl_busts = summary_u64(cache, "sub_ttl_busts");
    s.sub_ttl_bust_usd = summary_f64(cache, "sub_ttl_bust_penalty_usd_est");

    let rereads = &sections[2];
    s.compactions = sum_u64(rereads, 0, "compactions");
    s.extreme_compactions = summary_u64(rereads, "extreme_loss_compactions");
    s.compaction_thrashing = summary_u64(rereads, "compaction_thrashing_passes");
    s.reread_est = sum_u64(rereads, 0, "reread_tokens_est");
    let reread_usd = sum_f64(rereads, 0, "reread_usd_est");

    let polls = &sections[3];
    s.poll_est = sum_u64(polls, 0, "poll_tokens_est");
    let poll_usd = sum_f64(polls, 0, "poll_usd_est");
    s.truncation_reruns = summary_u64(polls, "truncation_reruns");

    let schemas = &sections[4];
    s.idle_schema_est = sum_u64(schemas, 0, "idle_tokens_est").saturating_add(sum_u64(
        schemas,
        1,
        "idle_tokens_est",
    ));
    let schema_usd = sum_f64(schemas, 0, "idle_usd_est") + sum_f64(schemas, 1, "idle_usd_est");

    s.waste_est = s
        .poll_est
        .saturating_add(s.reread_est)
        .saturating_add(s.idle_schema_est);
    s.waste_usd = round2(poll_usd + reread_usd + schema_usd);
    s
}

fn build_section(
    before_label: &str,
    after_label: &str,
    a: &Snapshot,
    b: &Snapshot,
    basis: &str,
    limit: usize,
) -> Section {
    let tok_pct = u64_pct(a.input_total, b.input_total);
    let usd_pct = f64_pct(a.input_usd, b.input_usd);
    let divergence_pp = tok_pct
        .zip(usd_pct)
        .map_or(Value::Null, |(t, u)| json!(round2(u - t)));
    let mut table = Table::new(
        "Before vs after",
        vec!["metric", "before", "after", "delta", "delta_pct"],
    );
    table.rows = diff_rows(a, b);
    table.truncate(limit);
    Section {
        name: "diff",
        title: "before vs after comparison of tokens, cache hit %, cost, turns, compactions and waste",
        summary: vec![
            ("before".to_owned(), json!(before_label)),
            ("after".to_owned(), json!(after_label)),
            ("usd_basis".to_owned(), json!(basis)),
            ("input_tokens_delta_pct".to_owned(), opt_json(tok_pct)),
            ("input_usd_delta_pct".to_owned(), opt_json(usd_pct)),
            ("usd_token_divergence_pp".to_owned(), divergence_pp),
        ],
        tables: vec![table],
        notes: vec![
            "Compares 2 session folders or half-open time windows [START, END) side by side. Delta is after minus before; delta pct is percentage change relative to before (null when before is 0).".to_owned(),
            "Usd token divergence pp is input usd delta pct minus input tokens delta pct: a positive gap means dollars rose faster (or fell less) than raw tokens, such as after sub-TTL cache busts. Turns, token counts, cache hit %, sub-TTL cache busts, input usd est and idle schema tokens come from Claude detectors; compactions, re-reads, polls and truncation re-runs cover every scanned agent those detectors read. Waste tokens est and waste usd est sum polls, compaction re-reads and idle schemas/skills (which can overlap, as in ranked). An illustration, not a bill.".to_owned(),
        ],
    }
}

fn diff_rows(a: &Snapshot, b: &Snapshot) -> Vec<Vec<Value>> {
    vec![
        row_u64("turns", a.turns, b.turns),
        row_u64("input_tokens", a.input_total, b.input_total),
        row_u64("output_tokens", a.output_total, b.output_total),
        row_u64("cache_read_tokens", a.cache_read, b.cache_read),
        row_u64("cache_write_tokens", a.cache_write, b.cache_write),
        row_opt_f64("cache_hit_pct", a.cache_hit_pct, b.cache_hit_pct),
        row_u64("sub_ttl_cache_busts", a.sub_ttl_busts, b.sub_ttl_busts),
        row_f64(
            "sub_ttl_bust_usd_est",
            a.sub_ttl_bust_usd,
            b.sub_ttl_bust_usd,
        ),
        row_f64("input_usd_est", a.input_usd, b.input_usd),
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
        row_f64("waste_usd_est", a.waste_usd, b.waste_usd),
    ]
}

fn sum_u64(s: &Section, table: usize, col: &str) -> u64 {
    let t = &s.tables[table];
    let at = t
        .columns
        .iter()
        .position(|c| *c == col)
        .unwrap_or(usize::MAX);
    t.rows
        .iter()
        .filter_map(|r| r.get(at)?.as_u64())
        .fold(0, u64::saturating_add)
}

fn sum_f64(s: &Section, table: usize, col: &str) -> f64 {
    let t = &s.tables[table];
    let at = t
        .columns
        .iter()
        .position(|c| *c == col)
        .unwrap_or(usize::MAX);
    round2(t.rows.iter().filter_map(|r| r.get(at)?.as_f64()).sum())
}

fn summary_u64(s: &Section, key: &str) -> u64 {
    s.summary
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(0)
}

fn summary_f64(s: &Section, key: &str) -> f64 {
    s.summary
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_f64())
        .unwrap_or(0.0)
}

fn row_u64(metric: &str, before: u64, after: u64) -> Vec<Value> {
    let delta = i128::from(after) - i128::from(before);
    vec![
        json!(metric),
        json!(before),
        json!(after),
        json!(delta),
        opt_json(u64_pct(before, after)),
    ]
}

fn row_f64(metric: &str, before: f64, after: f64) -> Vec<Value> {
    let b = round2(before);
    let a = round2(after);
    vec![
        json!(metric),
        json!(b),
        json!(a),
        json!(round2(a - b)),
        opt_json(f64_pct(b, a)),
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
    if before <= 0.0 {
        return None;
    }
    Some(round2(((after - before) / before) * 100.0))
}

fn opt_json(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |x| json!(x))
}

fn round2(v: f64) -> f64 {
    let r = (v * 100.0).round() / 100.0;
    if r == 0.0 { 0.0 } else { r }
}
