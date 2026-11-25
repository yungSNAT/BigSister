//! Side-by-side comparison (`bigsister diff <before> <after>`) across 2 session folders
//! or time windows so configuration and hook changes can be verified in numbers.

mod table;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use jiff::Timestamp;

use crate::cli::Cli;
use crate::detectors::{cache, models, polls, rereads, schemas};
use crate::prices::Prices;
use crate::programs::Programs;
use crate::report::{Meta, Opts, Section, display_path, rfc3339};
use crate::scan::{Agent, Detector, Memory, RootStats, scan_memory_range_dyn, scan_range_dyn};
use crate::{
    chosen, folder_problem, price, read_prices, render_report, resolve_roots, sample, time,
};
use table::{Snapshot, build_section, round2};

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
        by_day: false,
        home: home.clone(),
        usd_per_mtok: cli.usd_per_mtok,
        prices_file: prices.file().map(str::to_owned),
    };
    let mut a = measure(cli, &left, home.as_deref(), &prices, &programs, &opts)?;
    let b = measure(cli, &right, home.as_deref(), &prices, &programs, &opts)?;
    note_empty(
        &mut a.skipped,
        [(left.label(), &a.stats), (right.label(), &b.stats)],
    );
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
        records_scanned: a.stats.iter().any(|r| r.files_with_records > 0),
    };
    Ok(render_report(cli, &meta, &[section]))
}

/// Adds a note for each side, `(label, stats)`, with no record in the window, as the
/// report's own line weighs both sides together.
fn note_empty(notes: &mut Vec<String>, sides: [(&str, &[RootStats]); 2]) {
    for (side, stats) in sides {
        if !stats.iter().any(|r| r.files_with_records > 0) {
            notes.push(format!("{side}: {}", crate::report::NOTHING_SCANNED));
        }
    }
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
    let mut models_det = models::Models::new(Rc::clone(prices));
    let mut cache_det = cache::Cache::default();
    let mut rereads_det = rereads::Rereads::default();
    let mut polls_det = polls::Polls::new(Rc::clone(programs));
    let mut schemas_det = schemas::Schemas::default();
    let (stats, skipped) = {
        let mut dets: [&mut (dyn Detector + 'static); 5] = [
            &mut models_det,
            &mut cache_det,
            &mut rereads_det,
            &mut polls_det,
            &mut schemas_det,
        ];
        scan_target(cli, target, home, prices, &mut dets)?
    };
    let exact_models = models_det.diff_totals(opts.usd_per_mtok);
    let exact_bust_usd = cache_det.exact_sub_ttl_penalty_usd(opts.usd_per_mtok);
    let exact_waste_usd = polls_det.exact_poll_usd(opts.usd_per_mtok)
        + rereads_det.exact_reread_usd(opts.usd_per_mtok)
        + schemas_det.exact_idle_usd(opts.usd_per_mtok);
    let sections: Vec<Section> = [
        &models_det as &dyn Detector,
        &cache_det,
        &rereads_det,
        &polls_det,
        &schemas_det,
    ]
    .into_iter()
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
        snap: extract(&sections, exact_models, exact_bust_usd, exact_waste_usd),
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
    dets: &mut [&mut (dyn Detector + 'static)],
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
                    scan_memory_range_dyn(
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
                scan_range_dyn(&roots, Some(*since), Some(*until), prices, dets),
                skipped,
            ))
        }
        Target::Dir { path, .. } => {
            let roots = dir_roots(cli, path);
            Ok((
                scan_range_dyn(&roots, Some(Timestamp::MIN), None, prices, dets),
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

fn extract(
    sections: &[Section],
    exact_models: models::DiffTotals,
    exact_bust_usd: Option<f64>,
    exact_waste_usd: f64,
) -> Snapshot {
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
    s.input_usd = exact_models.input_usd;
    s.unpriced_replies = exact_models.unpriced_replies;
    s.skipped_replies = exact_models.skipped_replies;
    if s.input_total > 0 {
        #[expect(clippy::cast_precision_loss, reason = "counts stay far below 2^52")]
        let pct = (s.cache_read as f64 / s.input_total as f64) * 100.0;
        s.cache_hit_pct = Some(round2(pct));
    }

    let cache = &sections[1];
    s.sub_ttl_busts = summary_u64(cache, "sub_ttl_busts");
    s.sub_ttl_bust_usd = exact_bust_usd;

    let rereads = &sections[2];
    s.compactions = sum_u64(rereads, 0, "compactions");
    s.extreme_compactions = summary_u64(rereads, "extreme_loss_compactions");
    s.compaction_thrashing = summary_u64(rereads, "compaction_thrashing_passes");
    s.reread_est = sum_u64(rereads, 0, "reread_tokens_est");

    let polls = &sections[3];
    s.poll_est = sum_u64(polls, 0, "poll_tokens_est");
    s.truncation_reruns = summary_u64(polls, "truncation_reruns");

    let schemas = &sections[4];
    s.idle_schema_est = sum_u64(schemas, 0, "idle_tokens_est").saturating_add(sum_u64(
        schemas,
        1,
        "idle_tokens_est",
    ));

    s.waste_est = s
        .poll_est
        .saturating_add(s.reread_est)
        .saturating_add(s.idle_schema_est);
    s.waste_usd = exact_waste_usd;
    s
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

fn summary_u64(s: &Section, key: &str) -> u64 {
    s.summary
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(0)
}
