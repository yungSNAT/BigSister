//! BigSister: where a coding agent's tokens and checks go, from its own transcripts.

pub mod cli;
pub mod detectors;
pub mod opencode;
pub mod pi;
pub mod prices;
pub mod programs;
pub mod report;
pub mod sample;
pub mod scan;
pub mod shell;
pub mod time;

use std::path::PathBuf;

use cli::{AgentChoice, Cli};
use report::{Meta, Opts};
use scan::Agent;

/// Runs the chosen detectors and returns the rendered report.
///
/// # Errors
/// Returns a message when `--since` is invalid or no transcript folder can be read.
pub fn run(cli: &Cli) -> Result<String, String> {
    let since = window_start(cli)?;
    let home = std::env::home_dir().map(|h| report::display_path(&h.display().to_string(), None));
    let prices = std::rc::Rc::new(read_prices(cli, home.as_deref())?);
    let mut dets = detectors::build(
        cli.command,
        &std::rc::Rc::new(programs::Programs::from_env()),
        &prices,
    );
    let (stats, skipped) = read_transcripts(cli, home.as_deref(), since, &prices, &mut dets)?;
    let opts = Opts {
        limit: cli.limit,
        paths: cli.paths,
        home: home.clone(),
        usd_per_mtok: cli.usd_per_mtok,
        prices_file: prices.file().map(str::to_owned),
    };
    let scanned: Vec<Agent> = stats.iter().map(|s| s.agent).collect();
    let sections: Vec<_> = dets
        .iter()
        .map(|d| {
            let mut s = d.section(&opts);
            price(cli, &prices, &mut s);
            if !d.agents().iter().any(|a| scanned.contains(a)) {
                let names: Vec<&str> = d.agents().iter().map(|a| a.name()).collect();
                s.notes.insert(
                    0,
                    format!(
                        "Reads {} transcripts only, and none were scanned.",
                        names.join(" and ")
                    ),
                );
            }
            s
        })
        .collect();
    let meta = Meta {
        since: since.unwrap_or(jiff::Timestamp::MAX),
        roots: &stats,
        skipped_roots: &skipped,
        home: home.as_deref(),
    };
    Ok(if cli.json {
        report::json(&meta, &sections)
    } else if cli.csv {
        report::csv(&sections)
    } else {
        report::text(&meta, &sections)
    })
}

/// Where the window starts: `--since`, or the sample's own start with `--sample`.
fn window_start(cli: &Cli) -> Result<Option<jiff::Timestamp>, String> {
    let when = if cli.sample {
        sample::SINCE
    } else {
        &cli.since
    };
    time::parse_since(when, jiff::Timestamp::now())
}

/// Scans the transcripts the chosen detectors read: the sample's with `--sample`, else
/// those in the transcript folders, with notes on default folders that are missing.
fn read_transcripts(
    cli: &Cli,
    home: Option<&str>,
    since: Option<jiff::Timestamp>,
    prices: &prices::Prices,
    dets: &mut [Box<dyn scan::Detector>],
) -> Result<(Vec<scan::RootStats>, Vec<String>), String> {
    let read: Vec<Agent> = dets.iter().flat_map(|d| d.agents()).copied().collect();
    if cli.sample {
        let files: Vec<scan::Memory> = sample::FILES
            .iter()
            .filter(|f| read.contains(&f.agent) && chosen(cli).is_none_or(|c| c == f.agent))
            .copied()
            .collect();
        return Ok((
            scan::scan_memory(&files, sample::ROOT, since, prices, dets),
            Vec::new(),
        ));
    }
    let (roots, skipped) = resolve_roots(cli, &read, home)?;
    Ok((scan::scan(&roots, since, prices, dets), skipped))
}

/// The bundled prices, or those of the price file `--prices` names.
fn read_prices(cli: &Cli, home: Option<&str>) -> Result<prices::Prices, String> {
    let Some(path) = &cli.prices else {
        return Ok(prices::Prices::bundled());
    };
    let shown = report::display_path(&path.display().to_string(), home);
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read price file {shown}: {e}"))?;
    prices::Prices::from_file(&text, &shown)
}

/// Prices a section at `--usd-per-mtok`'s one rate, or names the price file in its note.
fn price(cli: &Cli, prices: &prices::Prices, s: &mut report::Section) {
    if let Some(usd) = cli.usd_per_mtok {
        report::price(s, usd);
    } else if cli.prices.is_some() {
        for note in s.notes.iter_mut().filter(|n| *n == prices::NOTE) {
            *note = prices.note();
        }
    }
}

type Roots = (Vec<(Agent, PathBuf)>, Vec<String>);

/// The one agent `--agent` names, or `None` for all.
fn chosen(cli: &Cli) -> Option<Agent> {
    match cli.agent {
        AgentChoice::All => None,
        AgentChoice::Claude => Some(Agent::Claude),
        AgentChoice::Codex => Some(Agent::Codex),
        AgentChoice::Pi => Some(Agent::Pi),
        AgentChoice::Opencode => Some(Agent::OpenCode),
    }
}

/// The transcript folders to scan for agents that `--agent` allows and the chosen
/// detectors read, and notes on default folders that are missing.
fn resolve_roots(cli: &Cli, read: &[Agent], home: Option<&str>) -> Result<Roots, String> {
    let chosen = chosen(cli);
    let wanted = |a: Agent| read.contains(&a) && chosen.is_none_or(|c| c == a);
    let candidates = [
        (Agent::Claude, cli.claude_root.clone(), ".claude/projects"),
        (Agent::Codex, cli.codex_root.clone(), ".codex/sessions"),
        (Agent::Pi, cli.pi_root.clone(), ".pi/agent/sessions"),
        (
            Agent::OpenCode,
            cli.opencode_root.clone(),
            ".local/share/opencode",
        ),
    ];
    let mut roots = Vec::new();
    let mut skipped = Vec::new();
    for (agent, explicit, default) in candidates {
        if !wanted(agent) {
            continue;
        }
        let given = explicit.is_some();
        let path = match explicit {
            Some(p) => p,
            None => PathBuf::from(home.ok_or("cannot find the home folder")?).join(default),
        };
        let Some(problem) = folder_problem(&path) else {
            roots.push((agent, path));
            continue;
        };
        let shown = report::display_path(&path.display().to_string(), home);
        let message = format!("{} transcript folder {problem}: {shown}", agent.name());
        if given || cli.agent != AgentChoice::All {
            return Err(message);
        }
        skipped.push(message);
    }
    if wanted(Agent::Codex)
        && cli.codex_root.is_none()
        && let Some(h) = home
    {
        add_codex_archive(h, &mut roots, &mut skipped);
    }
    // With no folder tried, as for `hooks --agent codex`, each section notes that
    // none of its transcripts were scanned.
    if roots.is_empty() && !skipped.is_empty() {
        return Err(format!(
            "{}. To see a report on sample transcripts, run: bigsister all --sample",
            skipped.join("; ")
        ));
    }
    Ok((roots, skipped))
}

/// Why a transcript folder cannot be scanned, if it cannot.
fn folder_problem(path: &std::path::Path) -> Option<&'static str> {
    if !path.is_dir() {
        Some("not found")
    } else if std::fs::read_dir(path).is_err() {
        Some("unreadable")
    } else {
        None
    }
}

/// Codex moves older sessions out of `sessions` into `archived_sessions`, so the
/// default reads both. A missing archive folder is normal and needs no note.
fn add_codex_archive(home: &str, roots: &mut Vec<(Agent, PathBuf)>, skipped: &mut Vec<String>) {
    let path = PathBuf::from(home).join(".codex/archived_sessions");
    if !path.is_dir() {
        return;
    }
    if std::fs::read_dir(&path).is_ok() {
        roots.push((Agent::Codex, path));
    } else {
        let shown = report::display_path(&path.display().to_string(), Some(home));
        skipped.push(format!("codex transcript folder unreadable: {shown}"));
    }
}
