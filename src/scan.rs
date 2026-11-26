//! One pass over transcript files, feeding each wanted record to the detectors.

mod feed;
mod files;
mod prep;
#[cfg(test)]
mod tests;

use feed::{Earlier, Window, scan_file};
use files::{antigravity_logs, first_time, first_time_in, list_files};
use prep::{scan_files, scan_opencode};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use rayon::prelude::*;
use serde_json::Value;

use crate::prices::Prices;
use crate::report::{Opts, Section};

/// Which agent wrote a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Claude,
    Codex,
    Pi,
    OpenCode,
    Gemini,
    Qwen,
    Antigravity,
    /// GenAI spans in OTLP/JSON trace files ([`crate::otlp`]).
    Otlp,
}

impl Agent {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
            Agent::OpenCode => "opencode",
            Agent::Gemini => "gemini",
            Agent::Qwen => "qwen",
            Agent::Antigravity => "antigravity",
            Agent::Otlp => "otlp",
        }
    }

    /// Whether the agent's records reach the detectors as Claude Code records: pi's,
    /// OpenCode's, Gemini CLI's, Qwen Code's and OTLP spans are rewritten into them
    /// ([`crate::pi`], [`crate::opencode`], [`crate::gemini`], [`crate::otlp`]).
    #[must_use]
    pub fn claude_shaped(self) -> bool {
        matches!(
            self,
            Agent::Claude
                | Agent::Pi
                | Agent::OpenCode
                | Agent::Gemini
                | Agent::Qwen
                | Agent::Antigravity
                | Agent::Otlp
        )
    }
}

/// One transcript file.
#[derive(Debug, Clone, Copy)]
pub struct Session {
    pub agent: Agent,
    pub subagent: bool,
}

impl Session {
    #[must_use]
    pub fn scope(self) -> &'static str {
        if self.subagent { "subagent" } else { "main" }
    }
}

/// A parsed line and whether its time falls in the window.
pub struct Record<'a> {
    pub value: &'a Value,
    pub in_window: bool,
}

impl Record<'_> {
    /// The record's `type` field, or "" when it has none.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.value.get("type").and_then(Value::as_str).unwrap_or("")
    }
}

/// A waste detector. It sees only lines it wants, parsed once per line.
pub trait Detector {
    fn agents(&self) -> &'static [Agent];
    /// A cheap test on the raw line; only lines that pass are parsed.
    ///
    /// shortcut: the tests look for compact JSON such as `"type":"attachment"`, as
    /// both agents write it. A writer that adds spaces would make every line fail
    /// them; parsing each line first would be the upgrade, at several times the cost.
    fn wants(&self, s: &Session, line: &str) -> bool;
    /// A stateless line filter for parallel pre-parsing, or `None` when any line may be
    /// wanted.
    fn prefilter(&self) -> Option<Prefilter> {
        None
    }
    fn begin(&mut self, _s: &Session) {}
    /// A pi session continues from an earlier entry than the last: the state of the path
    /// left behind is dropped, and the path to the branch point is replayed as copies.
    /// Counts already made stand.
    fn branch(&mut self, s: &Session) {
        self.begin(s);
    }
    fn record(&mut self, s: &Session, r: &Record<'_>);
    /// Whether the rest of this file can change nothing the detector reports.
    fn done(&self, _s: &Session) -> bool {
        false
    }
    /// Whether `done` always returns `false`, so the whole file can be prepared in parallel.
    fn scans_whole_file(&self) -> bool {
        false
    }
    fn end(&mut self, _s: &Session) {}
    /// A model has read the session's context so far: its uncached input price per
    /// million tokens, or `None` when it has no price or no model did. `in_window` is
    /// whether the reading reply itself falls inside the scan window.
    fn read_by(&mut self, _s: &Session, _usd_per_mtok: Option<f64>, _in_window: bool) {}
    /// Whether the detector reads Claude Code's request blobs, lines that can be large and
    /// that every other detector is spared.
    fn reads_request_blobs(&self) -> bool {
        false
    }
    fn section(&self, opts: &Opts) -> Section;
}

/// What the scan saw under one root.
#[derive(Debug, Clone)]
pub struct RootStats {
    pub agent: Agent,
    pub root: PathBuf,
    pub files: u64,
    pub unreadable: u64,
    pub bad_lines: u64,
    /// `*.jsonl.zst` transcripts in the window that do not start with the zstd magic
    /// number, so nothing in them was read.
    pub compressed: u64,
    /// Files holding at least one record whose time falls in the window. With none in
    /// any root, nothing was scanned, whatever `files` says.
    pub files_with_records: u64,
    /// Why files or folders were unreadable, each reason with how many, summing to
    /// `unreadable`. A reason holds no file or table content.
    pub unreadable_reasons: BTreeMap<String, u64>,
}

impl RootStats {
    fn new(agent: Agent, root: PathBuf) -> Self {
        RootStats {
            agent,
            root,
            files: 0,
            unreadable: 0,
            bad_lines: 0,
            compressed: 0,
            files_with_records: 0,
            unreadable_reasons: BTreeMap::new(),
        }
    }

    /// Counts one unreadable file or folder, and why.
    fn count_unreadable(&mut self, reason: String) {
        self.unreadable += 1;
        *self.unreadable_reasons.entry(reason).or_default() += 1;
    }

    /// The value read, or `None` once a failure to read it is counted as `what`.
    fn readable<T>(&mut self, read: std::io::Result<T>, what: &str) -> Option<T> {
        match read {
            Ok(value) => Some(value),
            Err(e) => {
                self.count_unreadable(format!("{what}: {e}"));
                None
            }
        }
    }
}

/// The start of a Claude Code request blob: a message as a request sent it, which only
/// the request log's reader needs; to the rest it repeats transcript content.
const CLAUDE_BLOB_PREFIX: &str = "{\"type\":\"api-request-blob\"";

/// Scans every transcript under each root that a chosen detector reads and that
/// was modified at or after `since`, pricing what each model reads at `prices`. With no
/// `since`, the window starts after every instant: no record is in it, but what cannot
/// be read is still counted.
pub fn scan(
    roots: &[(Agent, PathBuf)],
    since: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    scan_range(roots, since, None, prices, dets)
}

/// Scans every transcript under each root for records in `[since, until)`.
pub fn scan_range(
    roots: &[(Agent, PathBuf)],
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    let mut refs: Vec<&mut (dyn Detector + 'static)> = dets.iter_mut().map(Box::as_mut).collect();
    scan_range_dyn(roots, since, until, prices, &mut refs)
}

/// Scans every transcript under each root for records in `[since, until)` using borrowed detectors.
pub(crate) fn scan_range_dyn(
    roots: &[(Agent, PathBuf)],
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [&mut (dyn Detector + 'static)],
) -> Vec<RootStats> {
    let window = Window {
        since,
        until,
        prices,
    };
    let cutoff = since.map_or(Timestamp::MAX.as_second() + 1, Timestamp::as_second);
    let cutoff = u64::try_from(cutoff)
        .ok()
        .and_then(|s| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(s)))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut out = Vec::new();
    let mut earlier = Earlier::default();
    for (agent, root) in roots {
        let active: Vec<usize> = (0..dets.len())
            .filter(|&i| dets[i].agents().contains(agent))
            .collect();
        if active.is_empty() {
            continue;
        }
        let mut st = RootStats::new(*agent, root.clone());
        if *agent == Agent::OpenCode {
            scan_opencode(root, &window, dets, &active, &mut st, &mut earlier);
            out.push(st);
            continue;
        }
        let mut files = if *agent == Agent::Antigravity {
            antigravity_logs(root, cutoff, &mut st)
        } else {
            list_files(root, cutoff, *agent == Agent::Otlp, &mut st)
        };
        // Antigravity's steps record `created_at`, not `timestamp`, and an OTLP line has
        // no top-level time; neither copies another file, so their files are not sorted.
        if agent.claude_shaped() && !matches!(agent, Agent::Antigravity | Agent::Otlp) {
            // A copied record keeps its time, so reading by each file's first time reads
            // an original before a copy of part of it, whose first record is later. A pi
            // fork's header is written when it is made, after the entries it copies.
            let mut timed: Vec<(Option<Timestamp>, PathBuf)> =
                files.into_par_iter().map(|p| (first_time(&p), p)).collect();
            timed.sort_by_key(|(t, _)| *t);
            files = timed.into_iter().map(|(_, p)| p).collect();
        }
        scan_files(files, &window, dets, &active, &mut st, &mut earlier);
        out.push(st);
    }
    out
}

/// A transcript held in memory: the agent that wrote it, its path below its root and its
/// text.
#[derive(Clone, Copy)]
pub struct Memory {
    pub agent: Agent,
    pub path: &'static str,
    pub text: &'static str,
}

/// Scans `files` as `scan` scans transcript folders, each agent's files as one root shown
/// as `root`, in the order the agents first appear. A file's modified time plays no part.
pub fn scan_memory(
    files: &[Memory],
    root: &str,
    since: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    scan_memory_range(files, root, since, None, prices, dets)
}

/// Scans in-memory transcripts for records in `[since, until)`.
pub fn scan_memory_range(
    files: &[Memory],
    root: &str,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    let mut refs: Vec<&mut (dyn Detector + 'static)> = dets.iter_mut().map(Box::as_mut).collect();
    scan_memory_range_dyn(files, root, since, until, prices, &mut refs)
}

/// Scans in-memory transcripts for records in `[since, until)` using borrowed detectors.
pub(crate) fn scan_memory_range_dyn(
    files: &[Memory],
    root: &str,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    prices: &Prices,
    dets: &mut [&mut (dyn Detector + 'static)],
) -> Vec<RootStats> {
    let window = Window {
        since,
        until,
        prices,
    };
    let mut agents: Vec<Agent> = Vec::new();
    for f in files {
        if !agents.contains(&f.agent) {
            agents.push(f.agent);
        }
    }
    let mut out = Vec::new();
    let mut earlier = Earlier::default();
    for agent in agents {
        let active: Vec<usize> = (0..dets.len())
            .filter(|&i| dets[i].agents().contains(&agent))
            .collect();
        if active.is_empty() {
            continue;
        }
        let mut st = RootStats::new(agent, PathBuf::from(root));
        // In the order `scan` reads a folder's files.
        let mut mine: Vec<&Memory> = files.iter().filter(|f| f.agent == agent).collect();
        mine.sort_by_key(|f| Path::new(f.path));
        if agent.claude_shaped() && agent != Agent::Antigravity {
            mine.sort_by_cached_key(|f| first_time_in(Box::new(f.text.as_bytes())));
        }
        for f in mine {
            st.files += 1;
            // An in-memory transcript cannot fail to read.
            let _ = scan_file(
                Box::new(f.text.as_bytes()),
                None,
                Path::new(f.path),
                &window,
                dets,
                &active,
                &mut st,
                &mut earlier,
            );
        }
        out.push(st);
    }
    out
}

/// Stateless line filter a detector can offer for parallel pre-parsing.
#[derive(Clone, Copy)]
pub enum Prefilter {
    /// Identical to `wants`, so workers can decide membership without re-scanning.
    Exact(fn(&Session, &str) -> bool),
    /// A superset of `wants`: matching lines are parsed in parallel, then checked with `wants`.
    Superset(fn(&Session, &str) -> bool),
}
