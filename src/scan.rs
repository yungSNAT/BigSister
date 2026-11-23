//! One pass over transcript files, feeding each wanted record to the detectors.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use rayon::prelude::*;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use serde_json::Value;

use crate::gemini::{Gemini, Rewrite};
use crate::prices::Prices;
use crate::report::{Opts, Section};
use crate::time::parse_ts;

/// Which agent wrote a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Agent {
    Claude,
    Codex,
    Pi,
    OpenCode,
    Gemini,
    Qwen,
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
        }
    }

    /// Whether the agent's records reach the detectors as Claude Code records: pi's,
    /// OpenCode's, Gemini CLI's and Qwen Code's are rewritten into them ([`crate::pi`],
    /// [`crate::opencode`], [`crate::gemini`]).
    #[must_use]
    pub fn claude_shaped(self) -> bool {
        matches!(
            self,
            Agent::Claude | Agent::Pi | Agent::OpenCode | Agent::Gemini | Agent::Qwen
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
    /// million tokens, or `None` when it has no price or no model did.
    fn read_by(&mut self, _s: &Session, _usd_per_mtok: Option<f64>) {}
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
    let window = Window { since, prices };
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
        let mut files = list_files(root, cutoff, &mut st);
        if agent.claude_shaped() {
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
    let window = Window { since, prices };
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
        if agent.claude_shaped() {
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

/// Files prepared in parallel per batch, bounding peak memory on large corpora.
const PARALLEL_BATCH: usize = 16;
/// Maximum prepared events or raw-buffer bytes per file before falling back to streaming.
const MAX_PREP_EVENTS: usize = 65_536;
const MAX_PREP_RAW_BYTES: usize = 32 << 20;

/// Why opening or indexing a transcript failed before lines were read.
enum OpenError {
    NotZstd,
    Unreadable(String),
}

fn open_for_agent(
    path: &Path,
    agent: Agent,
) -> Result<(Box<dyn BufRead>, Option<Rewrite>), OpenError> {
    let reader = match open(path) {
        Ok(r) => r,
        Err(Open::NotZstd) => return Err(OpenError::NotZstd),
        Err(Open::Io(reason)) => return Err(OpenError::Unreadable(reason)),
    };
    match agent {
        Agent::Gemini => indexed(reader, path)
            .map(|(r, index)| (r, Some(Rewrite::Gemini(index))))
            .map_err(OpenError::Unreadable),
        Agent::Qwen => Ok((reader, Some(Rewrite::Qwen))),
        _ => Ok((reader, None)),
    }
}

fn record_open_error(st: &mut RootStats, err: &OpenError) {
    match err {
        OpenError::NotZstd => st.compressed += 1,
        OpenError::Unreadable(reason) => {
            st.files += 1;
            st.count_unreadable(reason.clone());
        }
    }
}

struct PrepareOpts<'a> {
    agent: Agent,
    prefilters: Option<&'a [Prefilter]>,
    prices: &'a Prices,
    max_events: usize,
    max_raw_bytes: usize,
}

enum PreparedFile {
    OpenErr(OpenError),
    Empty(u64),
    TooLarge,
    Ready(ReadyFile),
}

struct ReadyFile {
    session: Session,
    bad_lines: u64,
    raw_buf: String,
    events: Vec<PreparedEvent>,
    read_err: Option<String>,
}

enum PreparedEvent {
    PiParent(Option<String>),
    Branch(Vec<PreparedEvent>),
    Timestamp(Timestamp),
    Candidate(Candidate),
}

struct Candidate {
    raw: std::ops::Range<usize>,
    blob: bool,
    codex_model: bool,
    exact_mask: u64,
    check_mask: u64,
    reply: Option<Option<f64>>,
    parsed: Option<Value>,
}

struct Prep<'a> {
    session: Session,
    opts: &'a PrepareOpts<'a>,
    pi: Option<crate::pi::Tree>,
    rewrite: Option<Rewrite>,
    bad_lines: u64,
    raw_buf: String,
    events: Vec<PreparedEvent>,
    event_count: usize,
}

impl<'a> Prep<'a> {
    fn new(session: Session, opts: &'a PrepareOpts<'a>, rewrite: Option<Rewrite>) -> Self {
        Prep {
            session,
            opts,
            pi: (session.agent == Agent::Pi).then(crate::pi::Tree::default),
            rewrite,
            bad_lines: 0,
            raw_buf: String::new(),
            events: Vec::new(),
            event_count: 0,
        }
    }

    fn over_budget(&self) -> bool {
        self.event_count > self.opts.max_events || self.raw_buf.len() > self.opts.max_raw_bytes
    }

    fn drain_into(&mut self, line: &str, feed: &mut Feed<'_>) {
        self.raw_buf.clear();
        self.take(line);
        feed.st.bad_lines += std::mem::take(&mut self.bad_lines);
        for event in &self.events {
            feed.apply_event(event, &self.raw_buf);
        }
        self.events.clear();
        self.event_count = 0;
    }

    fn take(&mut self, line: &str) {
        if let Some(rewrite) = self.rewrite.as_mut() {
            let mut out = Vec::new();
            self.bad_lines += rewrite.read(line, &mut out);
            for l in &out {
                let mut events = std::mem::take(&mut self.events);
                self.push_line(l, &mut events);
                self.events = events;
            }
            return;
        }
        let Some(tree) = self.pi.as_mut() else {
            let mut events = std::mem::take(&mut self.events);
            self.push_line(line, &mut events);
            self.events = events;
            return;
        };
        if line.is_empty() {
            return;
        }
        let prev_parent = tree.parent.clone();
        let Some(read) = tree.read(line) else {
            self.bad_lines += 1;
            return;
        };
        if tree.parent != prev_parent {
            self.events
                .push(PreparedEvent::PiParent(tree.parent.clone()));
            self.event_count += 1;
        }
        if let Some(path) = read.branch {
            let mut branch = Vec::new();
            for old in &path {
                if self.over_budget() {
                    return;
                }
                self.push_line(old, &mut branch);
            }
            self.events.push(PreparedEvent::Branch(branch));
            self.event_count += 1;
        }
        let mut events = std::mem::take(&mut self.events);
        self.push_line(&read.line, &mut events);
        self.events = events;
    }

    fn match_masks(&self, line: &str, blob: bool) -> (u64, u64) {
        let Some(prefilters) = self.opts.prefilters else {
            return (0, u64::MAX);
        };
        let (mut exact, mut check) = (0u64, 0u64);
        for (slot, pf) in prefilters.iter().enumerate() {
            let bit = 1u64 << slot;
            match *pf {
                Prefilter::Exact(f) => {
                    if f(&self.session, line) {
                        if blob {
                            check |= bit;
                        } else {
                            exact |= bit;
                        }
                    }
                }
                Prefilter::Superset(f) => {
                    if f(&self.session, line) {
                        check |= bit;
                    }
                }
            }
        }
        (exact, check)
    }

    fn push_line(&mut self, line: &str, out: &mut Vec<PreparedEvent>) {
        if line.is_empty() {
            return;
        }
        let blob = self.session.agent == Agent::Claude && line.starts_with(CLAUDE_BLOB_PREFIX);
        if !blob && !line.ends_with('}') {
            self.bad_lines += 1;
            return;
        }
        let (exact_mask, check_mask) = self.match_masks(line, blob);
        let codex_model = self.session.agent == Agent::Codex && codex_model_line(line);
        let claude_model = if !blob && self.session.agent.claude_shaped() {
            claude_model_name(line)
        } else {
            None
        };
        if exact_mask == 0 && check_mask == 0 && !codex_model && claude_model.is_none() {
            if !blob && push_timestamp(out, line) {
                self.event_count += 1;
            }
            return;
        }
        let parsed = line
            .ends_with('}')
            .then(|| serde_json::from_str::<Value>(line).ok())
            .flatten();
        let reply = claude_model.map(|model| {
            self.opts.prices.usd_per_mtok(model, || {
                parsed
                    .as_ref()
                    .and_then(crate::detectors::startup::input_tokens)
            })
        });
        let raw = if check_mask != 0 || (exact_mask == 0 && reply.is_none()) {
            let start = self.raw_buf.len();
            self.raw_buf.push_str(line);
            start..self.raw_buf.len()
        } else {
            0..0
        };
        out.push(PreparedEvent::Candidate(Candidate {
            raw,
            blob,
            codex_model,
            exact_mask,
            check_mask,
            reply,
            parsed,
        }));
        self.event_count += 1;
    }
}

fn push_timestamp(out: &mut Vec<PreparedEvent>, line: &str) -> bool {
    let Some(t) = raw_timestamp(line) else {
        return false;
    };
    match out.last_mut() {
        Some(PreparedEvent::Timestamp(prev)) => {
            *prev = t;
            false
        }
        _ => {
            out.push(PreparedEvent::Timestamp(t));
            true
        }
    }
}

fn prepare_file(path: &Path, opts: &PrepareOpts<'_>) -> PreparedFile {
    let (reader, rewrite) = match open_for_agent(path, opts.agent) {
        Ok(pair) => pair,
        Err(err) => return PreparedFile::OpenErr(err),
    };
    let mut lines = Lines::new(reader);
    let first = match lines.next_line() {
        Ok(Some(line)) => line.trim_start_matches('\u{feff}').trim_end().to_owned(),
        Ok(None) => return PreparedFile::Empty(lines.over_long),
        Err(e) => return PreparedFile::OpenErr(OpenError::Unreadable(read_error(&e))),
    };
    let session = Session {
        agent: opts.agent,
        subagent: is_subagent(opts.agent, path, &first),
    };
    let mut prep = Prep::new(session, opts, rewrite);
    prep.take(&first);
    let read_err = loop {
        if prep.over_budget() {
            return PreparedFile::TooLarge;
        }
        match lines.next_line() {
            Ok(Some(line)) => prep.take(line.trim_end()),
            Ok(None) => break None,
            Err(e) => break Some(read_error(&e)),
        }
    };
    prep.bad_lines += lines.over_long;
    PreparedFile::Ready(ReadyFile {
        session,
        bad_lines: prep.bad_lines,
        raw_buf: prep.raw_buf,
        events: prep.events,
        read_err,
    })
}

fn scan_path(
    path: &Path,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
) {
    match open_for_agent(path, st.agent) {
        Ok((reader, rewrite)) => {
            st.files += 1;
            if let Err(e) = scan_file(reader, rewrite, path, window, dets, active, st, earlier) {
                st.count_unreadable(read_error(&e));
            }
        }
        Err(err) => record_open_error(st, &err),
    }
}

fn apply_prepared(
    path: &Path,
    file: &PreparedFile,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
) {
    let ready = match file {
        PreparedFile::OpenErr(err) => {
            record_open_error(st, err);
            return;
        }
        PreparedFile::Empty(over_long) => {
            st.files += 1;
            st.bad_lines += over_long;
            return;
        }
        PreparedFile::TooLarge => {
            scan_path(path, window, dets, active, st, earlier);
            return;
        }
        PreparedFile::Ready(ready) => ready,
    };
    st.files += 1;
    st.bad_lines += ready.bad_lines;
    let session = ready.session;
    for &i in active {
        dets[i].begin(&session);
    }
    let mut feed = Feed::new(session, window, earlier, dets, active, st);
    for event in &ready.events {
        feed.apply_event(event, &ready.raw_buf);
    }
    let (uuids, replies) = feed.finish(0, ready.read_err.as_deref());
    earlier.add(session.agent, path, uuids, replies);
}

/// Reads each of `files`, a root's transcripts.
fn scan_files(
    files: Vec<PathBuf>,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
) {
    scan_files_with_budget(
        files,
        window,
        dets,
        active,
        st,
        earlier,
        MAX_PREP_EVENTS,
        MAX_PREP_RAW_BYTES,
    );
}

#[expect(
    clippy::too_many_arguments,
    reason = "testable memory budget for parallel prep"
)]
fn scan_files_with_budget(
    files: Vec<PathBuf>,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
    max_events: usize,
    max_raw_bytes: usize,
) {
    if !active.iter().any(|&i| dets[i].scans_whole_file()) {
        for path in files {
            scan_path(&path, window, dets, active, st, earlier);
        }
        return;
    }
    let prefilters: Option<Vec<Prefilter>> = (active.len() <= 64)
        .then(|| active.iter().map(|&i| dets[i].prefilter()).collect())
        .flatten();
    let opts = PrepareOpts {
        agent: st.agent,
        prefilters: prefilters.as_deref(),
        prices: window.prices,
        max_events,
        max_raw_bytes,
    };
    for chunk in files.chunks(PARALLEL_BATCH) {
        let prepared: Vec<_> = chunk
            .par_iter()
            .map(|path| prepare_file(path, &opts))
            .collect();
        for (path, file) in chunk.iter().zip(&prepared) {
            apply_prepared(path, file, window, dets, active, st, earlier);
        }
        prepared.into_par_iter().for_each(drop);
    }
}

/// Indexes Gemini CLI file `path` from `reader` ([`Gemini::index`]), then opens it again
/// for the second pass, or says why either cannot be read.
fn indexed(reader: Box<dyn BufRead>, path: &Path) -> Result<(Box<dyn BufRead>, Gemini), String> {
    let mut lines = Lines::new(reader);
    let mut index = Gemini::default();
    while let Some(line) = lines.next_line().map_err(|e| read_error(&e))? {
        index.index(&line);
    }
    let reader = open(path).map_err(|e| match e {
        Open::Io(reason) => reason,
        Open::NotZstd => "it changed whilst it was read".to_owned(),
    })?;
    Ok((reader, index))
}

/// Reads the OpenCode sessions in `root`'s `opencode.db`, each as one transcript. A
/// folder without the file holds none. A database that cannot be read, or whose
/// presence cannot be checked, counts as one unreadable file, with the reason the reader
/// gives, and none of its sessions are read.
fn scan_opencode(
    root: &Path,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
) {
    let db = root.join("opencode.db");
    if std::fs::metadata(&db).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
        return;
    }
    let sessions = match crate::opencode::read(&db, window.since) {
        Ok(sessions) => sessions,
        Err(reason) => {
            st.files += 1;
            st.count_unreadable(reason);
            return;
        }
    };
    for s in sessions {
        st.files += 1;
        // The first line says whether a subagent's session it is, as a Codex
        // transcript's first record does.
        let mut text =
            serde_json::json!({"type": "opencode_session", "subagent": s.subagent}).to_string();
        for line in s.lines {
            text.push('\n');
            text.push_str(&line);
        }
        let reader = Box::new(io::Cursor::new(text.into_bytes()));
        // An in-memory transcript cannot fail to read.
        let _ = scan_file(reader, None, &db, window, dets, active, st, earlier);
    }
}

fn list_files(root: &Path, cutoff: SystemTime, st: &mut RootStats) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Some(entries) = st.readable(fs::read_dir(&dir), "a folder cannot be listed") else {
            continue;
        };
        // Collected first, so the loop below can count into `st` too.
        let entries: Vec<_> = entries
            .filter_map(|entry| {
                let read = entry.and_then(|e| e.file_type().map(|ft| (e, ft)));
                st.readable(read, "a folder entry cannot be read")
            })
            .collect();
        for (entry, ft) in entries {
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else {
                let compressed = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".jsonl.zst"));
                if !compressed && path.extension().is_none_or(|x| x != "jsonl") {
                    continue;
                }
                match entry.metadata().and_then(|m| m.modified()) {
                    Ok(m) if m >= cutoff => out.push(path),
                    Ok(_) => {}
                    Err(e) => st.count_unreadable(format!("a file's time cannot be read: {e}")),
                }
            }
        }
    }
    out.sort();
    out
}

/// The top-level `timestamp` of a transcript's first record that has one, or `None`
/// when no record has one or the file cannot be read.
fn first_time(path: &Path) -> Option<Timestamp> {
    first_time_in(open(path).ok()?)
}

/// The top-level `timestamp` of the first record in `reader` that has one.
fn first_time_in(reader: Box<dyn BufRead>) -> Option<Timestamp> {
    let mut lines = Lines::new(reader);
    while let Ok(Some(line)) = lines.next_line() {
        let value = serde_json::from_str::<Value>(line.trim_end()).ok();
        let t = value.as_ref().and_then(|v| v.get("timestamp"));
        if let Some(t) = t.and_then(Value::as_str).and_then(parse_ts) {
            return Some(t);
        }
    }
    None
}

/// The longest line read, in bytes. A decoded `*.jsonl.zst` line is not bounded by
/// the file's size: a small frame can decode to gigabytes with no newline.
const MAX_LINE: u64 = 64 << 20;

/// shortcut: a line is held whole up to `MAX_LINE` bytes, far above the few MB of
/// the longest lines seen in real transcripts; a longer one is skipped unread and
/// counted as a skipped line. Parsing records as a stream would remove the cap.
struct Lines {
    reader: Box<dyn BufRead>,
    buf: Vec<u8>,
    /// Lines skipped for being longer than `MAX_LINE`.
    over_long: u64,
}

/// Why a transcript could not be opened.
enum Open {
    /// A `*.jsonl.zst` file that does not start with the zstd magic number, or is
    /// shorter than it.
    NotZstd,
    /// A file that cannot be opened, or a zstd frame header that cannot be decoded,
    /// and why.
    Io(String),
}

/// Why a zstd frame header could not be read. The decoder's message is not given, as
/// it can quote the file's bytes.
const ZSTD_HEADER: &str = "its zstd frame header cannot be read";

/// Why a transcript failed part way. An error the system gives names no file content;
/// any other comes from decoding a `*.jsonl.zst` one, whose message can quote the data,
/// so it is named only as such.
fn read_error(e: &io::Error) -> String {
    if e.raw_os_error().is_some() {
        format!("a file cannot be read: {e}")
    } else {
        "its zstd data cannot be decoded (damaged, cut short or more than one frame)".to_owned()
    }
}

/// Opens a transcript, decoding a `*.jsonl.zst` one as it is read.
fn open(path: &Path) -> Result<Box<dyn BufRead>, Open> {
    let file = BufReader::new(
        File::open(path).map_err(|e| Open::Io(format!("a file cannot be opened: {e}")))?,
    );
    if path.extension().is_none_or(|x| x != "zst") {
        return Ok(Box::new(file));
    }
    let decoder = StreamingDecoder::new(file).map_err(header_error)?;
    Ok(Box::new(BufReader::new(OneFrame(decoder))))
}

/// Why a zstd frame header could not be read: a file that ends before the magic
/// number is not zstd, but one whose read fails is unreadable.
fn header_error(e: FrameDecoderError) -> Open {
    use ReadFrameHeaderError::{BadMagicNumber, MagicNumberReadError};
    let FrameDecoderError::ReadFrameHeaderError(e) = e else {
        return Open::Io(ZSTD_HEADER.to_owned());
    };
    match e {
        BadMagicNumber(_) => Open::NotZstd,
        MagicNumberReadError(e) if e.kind() == io::ErrorKind::UnexpectedEof => Open::NotZstd,
        _ => Open::Io(ZSTD_HEADER.to_owned()),
    }
}

/// Decodes a compressed transcript and fails, rather than stopping quietly, on a bad
/// checksum or anything after the frame.
///
/// shortcut: reads one zstd frame, as Codex writes one per rollout (all 203 on one
/// machine had one, 13 November 2026); a file with more frames counts as unreadable
/// after its first. Decoding each frame in turn would be the upgrade.
struct OneFrame(StreamingDecoder<BufReader<File>, FrameDecoder>);

impl Read for OneFrame {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let frame = &self.0.decoder;
            if let Some(sum) = frame.get_checksum_from_data()
                && frame.get_calculated_checksum() != Some(sum)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd checksum mismatch",
                ));
            }
            if !self.0.get_mut().fill_buf()?.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data after the zstd frame",
                ));
            }
        }
        Ok(n)
    }
}

impl Lines {
    fn new(reader: Box<dyn BufRead>) -> Self {
        Lines {
            reader,
            buf: Vec::new(),
            over_long: 0,
        }
    }

    /// The next line of at most `MAX_LINE` bytes before its newline, skipping and
    /// counting longer ones.
    fn next_line(&mut self) -> io::Result<Option<Cow<'_, str>>> {
        loop {
            self.buf.clear();
            let n = (&mut self.reader)
                .take(MAX_LINE)
                .read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            if n as u64 == MAX_LINE && self.buf.last() != Some(&b'\n') {
                // The cap was reached: the line is longer only if more of it follows.
                match self.reader.fill_buf()?.first() {
                    None => {}
                    Some(b'\n') => self.reader.consume(1),
                    Some(_) => {
                        self.reader.skip_until(b'\n')?;
                        self.over_long += 1;
                        self.buf = Vec::new();
                        continue;
                    }
                }
            }
            return Ok(Some(String::from_utf8_lossy(&self.buf)));
        }
    }
}

/// What every transcript of a scan is read against.
struct Window<'a> {
    /// `None` starts the window after every instant.
    since: Option<Timestamp>,
    prices: &'a Prices,
}

/// The `uuid` of each Claude-shaped record parsed in an earlier file, and the message id
/// of each Claude Code reply.
///
/// shortcut: every uuid and message id of the scan is held as a string, about 100 bytes
/// each (a million uuids added 96 MB of peak memory, 14 November 2026). Holding a uuid as
/// the 128-bit number it spells would cut that to about a quarter.
#[derive(Default)]
struct Earlier {
    /// Claude Code's, all together: a resumed session does not name the file it copies.
    claude: HashSet<String>,
    /// Claude Code's reply message ids: a reply's block can be written again in another
    /// file under a new uuid.
    replies: HashSet<String>,
    /// pi's, by session file name: a fork's header names the file it copies, whose own
    /// uuids include those it copied in turn.
    pi: HashMap<String, HashSet<String>>,
}

impl Earlier {
    /// Holds the `uuids` and reply message ids `replies` of `agent`'s file `path`, once it
    /// is read.
    fn add(&mut self, agent: Agent, path: &Path, uuids: HashSet<String>, replies: HashSet<String>) {
        self.replies.extend(replies);
        if agent == Agent::Pi {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            self.pi.insert(name.into_owned(), uuids);
        } else {
            self.claude.extend(uuids);
        }
    }
}

/// Reads a transcript from `reader`, each line through `rewrite` for an agent whose lines
/// are rewritten into Claude Code records.
#[expect(
    clippy::too_many_arguments,
    reason = "the transcript's reader, rewrite and path, and the scan's state"
)]
fn scan_file(
    reader: Box<dyn BufRead>,
    rewrite: Option<Rewrite>,
    path: &Path,
    window: &Window<'_>,
    dets: &mut [Box<dyn Detector>],
    active: &[usize],
    st: &mut RootStats,
    earlier: &mut Earlier,
) -> io::Result<()> {
    let mut lines = Lines::new(reader);
    let Some(first) = lines.next_line()? else {
        st.bad_lines += lines.over_long;
        return Ok(());
    };
    let first = first.trim_start_matches('\u{feff}').trim_end().to_owned();
    let agent = st.agent;
    let session = Session {
        agent,
        subagent: is_subagent(agent, path, &first),
    };
    let prefilters: Option<Vec<Prefilter>> = (active.len() <= 64)
        .then(|| active.iter().map(|&i| dets[i].prefilter()).collect())
        .flatten();
    for &i in active {
        dets[i].begin(&session);
    }
    let opts = PrepareOpts {
        agent,
        prefilters: prefilters.as_deref(),
        prices: window.prices,
        max_events: usize::MAX,
        max_raw_bytes: usize::MAX,
    };
    let mut prep = Prep::new(session, &opts, rewrite);
    let mut feed = Feed::new(session, window, earlier, dets, active, st);
    prep.drain_into(&first, &mut feed);
    let result = loop {
        if feed.active.iter().all(|&i| feed.dets[i].done(&session)) {
            break Ok(());
        }
        match lines.next_line() {
            Ok(Some(line)) => prep.drain_into(line.trim_end(), &mut feed),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    let (uuids, replies) = feed.finish(lines.over_long, None);
    earlier.add(agent, path, uuids, replies);
    result
}

// shortcut: a Claude subagent is told apart by its `subagents` folder and a Codex one
// by `parent_thread_id` in its first record, as both agents lay them out in November
// 2026. A layout change makes subagents count as main sessions; a record field naming
// the parent would be the upgrade. pi, Gemini CLI and Qwen Code sessions count as main
// sessions: Gemini CLI's subagent header field and Qwen Code's parent record are unread.
fn is_subagent(agent: Agent, path: &Path, first: &str) -> bool {
    match agent {
        Agent::Claude => path.components().any(|c| c.as_os_str() == "subagents"),
        Agent::Codex => serde_json::from_str::<Value>(first).is_ok_and(|v| {
            v.pointer("/payload/parent_thread_id")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
        }),
        Agent::Pi | Agent::Gemini | Agent::Qwen => false,
        Agent::OpenCode => serde_json::from_str::<Value>(first)
            .is_ok_and(|v| v.get("subagent") == Some(&Value::Bool(true))),
    }
}

struct Feed<'a> {
    session: Session,
    since: Option<Timestamp>,
    prices: &'a Prices,
    last_time: Option<Timestamp>,
    /// The model the latest Codex `turn_context` names, if any.
    codex_model: Option<String>,
    /// The `uuid` of each Claude record parsed so far in the file.
    uuids: HashSet<String>,
    /// The message id of each Claude Code reply parsed so far in the file.
    replies: HashSet<String>,
    earlier: &'a Earlier,
    /// A pi session's parent file name, when its header named one.
    pi_parent: Option<String>,
    /// Whether the lines fed are a pi path given again, which count nowhere.
    replaying: bool,
    dets: &'a mut [Box<dyn Detector>],
    active: &'a [usize],
    wanting_buf: Vec<usize>,
    st: &'a mut RootStats,
}

impl<'a> Feed<'a> {
    fn new(
        session: Session,
        window: &Window<'a>,
        earlier: &'a Earlier,
        dets: &'a mut [Box<dyn Detector>],
        active: &'a [usize],
        st: &'a mut RootStats,
    ) -> Self {
        Feed {
            session,
            since: window.since,
            prices: window.prices,
            last_time: None,
            codex_model: None,
            uuids: HashSet::new(),
            replies: HashSet::new(),
            earlier,
            pi_parent: None,
            replaying: false,
            dets,
            active,
            wanting_buf: Vec::with_capacity(active.len()),
            st,
        }
    }

    fn apply_event(&mut self, event: &PreparedEvent, raw_buf: &str) {
        match event {
            PreparedEvent::PiParent(parent) => self.pi_parent = parent.clone(),
            PreparedEvent::Branch(path) => {
                for &i in self.active {
                    self.dets[i].branch(&self.session);
                }
                self.replaying = true;
                for ev in path {
                    self.apply_event(ev, raw_buf);
                }
                self.replaying = false;
            }
            PreparedEvent::Timestamp(t) => self.last_time = Some(*t),
            PreparedEvent::Candidate(c) => {
                let raw = &raw_buf[c.raw.clone()];
                self.fill_wanting(raw, c.blob, c.exact_mask, c.check_mask);
                if c.blob && self.wanting_buf.is_empty() {
                    return;
                }
                if c.codex_model
                    && let Some(value) = c.parsed.as_ref()
                {
                    self.codex_model(value);
                }
                if self.wanting_buf.is_empty() && c.reply.is_none() {
                    if let Some(t) = raw_timestamp(raw) {
                        self.last_time = Some(t);
                    }
                    return;
                }
                let Some(value) = c.parsed.as_ref() else {
                    self.st.bad_lines += 1;
                    return;
                };
                self.dispatch_value(value, c.reply);
            }
        }
    }

    fn dispatch_value(&mut self, value: &Value, reply: Option<Option<f64>>) {
        let Some(copied) = self.copied(value) else {
            return;
        };
        // A reply reads the results before it; a copy of one reads nothing.
        if let Some(price) = reply
            && !copied
        {
            self.read_by(price);
        }
        if let Some(t) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts)
        {
            self.last_time = Some(t);
        }
        let rec = Record {
            value,
            in_window: !copied && self.since.zip(self.last_time).is_some_and(|(s, t)| t >= s),
        };
        for &i in &self.wanting_buf {
            self.dets[i].record(&self.session, &rec);
        }
    }

    fn finish(
        mut self,
        extra_bad: u64,
        read_err: Option<&str>,
    ) -> (HashSet<String>, HashSet<String>) {
        self.st.bad_lines += extra_bad;
        // A Codex turn's model reads its results; a Claude result no reply read stays
        // unpriced.
        if self.session.agent == Agent::Codex {
            self.read_by(self.codex_price(|| None));
        }
        for &i in self.active {
            self.dets[i].end(&self.session);
        }
        if let Some(reason) = read_err {
            self.st.count_unreadable(reason.to_owned());
        }
        (self.uuids, self.replies)
    }

    /// Fills `self.wanting_buf` with the active detectors that want `line`; a request
    /// blob goes only to those that read request blobs.
    fn fill_wanting(&mut self, line: &str, blob: bool, exact_mask: u64, check_mask: u64) {
        self.wanting_buf.clear();
        for (slot, &i) in self.active.iter().enumerate() {
            let exact = slot < 64 && exact_mask & (1u64 << slot) != 0;
            let check = slot >= 64 || check_mask & (1u64 << slot) != 0;
            if exact
                || (check
                    && (!blob || self.dets[i].reads_request_blobs())
                    && self.dets[i].wants(&self.session, line))
            {
                self.wanting_buf.push(i);
            }
        }
    }

    /// Whether a parsed record copies an earlier file's record, or `None` when it
    /// repeats one of this file's. Claude Code can write a record again later in its
    /// file, with the same uuid; such a record counts nowhere. A resumed session's file
    /// starts with copies of the earlier file's records; such a copy still builds the
    /// session's state but counts nowhere, as does a pi path given again at a branch, and
    /// a Claude Code reply's block whose message id an earlier file holds, under any uuid.
    /// A pi session holds copies only of the file its header names.
    fn copied(&mut self, value: &Value) -> Option<bool> {
        if self.replaying {
            return Some(true);
        }
        if !self.session.agent.claude_shaped() {
            return Some(false);
        }
        let Some(uuid) = value.get("uuid").and_then(Value::as_str) else {
            return Some(false);
        };
        if !self.uuids.insert(uuid.to_owned()) {
            return None;
        }
        Some(if self.session.agent == Agent::Pi {
            self.pi_parent
                .as_deref()
                .and_then(|name| self.earlier.pi.get(name))
                .is_some_and(|uuids| uuids.contains(uuid))
        } else if self.session.agent == Agent::Claude {
            let reply = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .filter(|_| value.get("type").and_then(Value::as_str) == Some("assistant"));
            self.replies.extend(reply.map(str::to_owned));
            self.earlier.claude.contains(uuid)
                || reply.is_some_and(|id| self.earlier.replies.contains(id))
        } else {
            self.earlier.claude.contains(uuid)
        })
    }

    fn read_by(&mut self, usd_per_mtok: Option<f64>) {
        for &i in self.active {
            self.dets[i].read_by(&self.session, usd_per_mtok);
        }
    }

    fn codex_model(&mut self, v: &Value) {
        match v.get("type").and_then(Value::as_str) {
            // Codex writes a model call's usage as the call completes, so the call
            // read the results before it, in a prompt of `input_tokens`.
            //
            // shortcut: a tool can finish whilst the call that asked for it is
            // still writing, so its result comes before that call's usage record
            // and is priced at a prompt that lacks it (6 of 92,410 results in
            // 895 real rollouts in November 2026). Only a tier boundary between
            // the 2 prompts changes the price; pairing each result's call_id
            // with the call that made it would be the upgrade.
            Some("token_usage_record") => {
                let price = self.codex_price(|| {
                    v.pointer("/payload/usage/input_tokens")
                        .and_then(Value::as_u64)
                });
                self.read_by(price);
            }
            Some("turn_context") => {
                let ended = self.codex_price(|| None);
                self.read_by(ended);
                self.codex_model = v
                    .pointer("/payload/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ => {}
        }
    }

    /// The price of the current Codex turn's model, for a call whose prompt size
    /// `prompt_tokens` gives.
    fn codex_price(&self, prompt_tokens: impl FnOnce() -> Option<u64>) -> Option<f64> {
        self.prices
            .usd_per_mtok(self.codex_model.as_deref()?, prompt_tokens)
    }
}

fn claude_model_name(line: &str) -> Option<&str> {
    const KEY: &str = "\"message\":{\"model\":\"";
    let at = line.find(KEY)?;
    let rest = &line[at + KEY.len()..];
    let model = rest.split('"').next().unwrap_or("");
    // Claude Code writes a synthetic reply itself; no model read anything.
    (model != "<synthetic>").then_some(model)
}

fn codex_model_line(line: &str) -> bool {
    line.contains("\"type\":\"turn_context\"") || line.contains("\"type\":\"token_usage_record\"")
}

/// A record's top-level `timestamp`, read without parsing the line: Codex writes
/// it as the first key, Claude near the end after the message.
///
/// shortcut: in a Claude line, a nested object after the top-level key that has its
/// own `timestamp` is read instead. Only records without a timestamp of their own
/// take this time; parsing every line would be exact but slower.
fn raw_timestamp(line: &str) -> Option<Timestamp> {
    const KEY: &str = "\"timestamp\":\"";
    let at = if line.strip_prefix('{').is_some_and(|r| r.starts_with(KEY)) {
        1
    } else {
        line.rfind(KEY)?
    };
    let rest = &line[at + KEY.len()..];
    parse_ts(&rest[..rest.find('"')?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts the records it is given, and reports the count.
    #[derive(Default)]
    struct Count(u64);

    impl Detector for Count {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude, Agent::Codex]
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, _r: &Record<'_>) {
            self.0 += 1;
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: vec![("records".to_owned(), self.0.into())],
                ..Section::default()
            }
        }
    }

    /// The records `Count`, the first of `dets`, reports.
    fn counted(dets: &[Box<dyn Detector>]) -> Value {
        let opts = Opts {
            limit: 0,
            paths: false,
            home: None,
            usd_per_mtok: None,
            prices_file: None,
        };
        dets[0].section(&opts).summary.swap_remove(0).1
    }

    #[test]
    fn unselected_agents_are_absent_from_disk_and_sample_reports() {
        let prices = Prices::bundled();
        let since = Some(Timestamp::MIN);
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
        let root = PathBuf::from("unselected-gemini-root");
        let roots = [(Agent::Gemini, root)];
        assert!(scan(&roots, since, &prices, &mut dets).is_empty());

        let files = [Memory {
            agent: Agent::Gemini,
            path: "session.jsonl",
            text: "{}",
        }];
        assert!(scan_memory(&files, "sample", since, &prices, &mut dets).is_empty());
        assert_eq!(counted(&dets), 0);
    }

    #[test]
    fn a_gemini_transcript_that_changes_between_passes_is_reported() {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
        let dir = std::env::temp_dir().join(format!("bigsister-reopen-{}", std::process::id()));
        fs::create_dir(&dir).expect("create test directory");
        let path = dir.join("transcript.jsonl.zst");
        let first_pass = || Box::new(io::Cursor::new(b"{}\n")) as Box<dyn BufRead>;
        fs::write(&path, b"replacement").expect("write changed transcript");
        let changed = indexed(first_pass(), &path)
            .err()
            .expect("reject the replacement");
        fs::remove_file(&path).expect("remove transcript");
        let removed = indexed(first_pass(), &path)
            .err()
            .expect("report a missing transcript");
        fs::remove_dir(&dir).expect("remove test directory");
        assert_eq!(changed, "it changed whilst it was read");
        assert!(removed.starts_with("a file cannot be opened:"), "{removed}");
    }

    #[test]
    fn a_claude_record_written_again_with_its_uuid_is_read_once() {
        let lines = [
            r#"{"type":"assistant","uuid":"u1","message":{}}"#,
            r#"{"type":"user","uuid":"u2","message":{}}"#,
            r#"{"type":"assistant","uuid":"u1","slug":"s","message":{}}"#,
            r#"{"type":"summary"}"#,
            r#"{"type":"summary"}"#,
        ]
        .join("\n");
        // Claude reads u1 once; Codex records carry no such uuid, so all count.
        for (agent, want) in [(Agent::Claude, 4), (Agent::Codex, 5)] {
            let reader = Box::new(io::Cursor::new(lines.clone().into_bytes()));
            assert_eq!(scan_one(agent, reader).0, want, "{agent:?}");
        }
    }

    /// Scans one transcript with `Count`, and gives the records it was given and the
    /// lines skipped.
    fn scan_one(agent: Agent, reader: Box<dyn BufRead>) -> (u64, u64) {
        let mut st = RootStats::new(agent, PathBuf::new());
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
        scan_file(
            reader,
            None,
            Path::new("s.jsonl"),
            &Window {
                since: Some(Timestamp::MIN),
                prices: &Prices::bundled(),
            },
            &mut dets,
            &[0],
            &mut st,
            &mut Earlier::default(),
        )
        .expect("read");
        let n = counted(&dets).as_u64().expect("a count");
        (n, st.bad_lines)
    }

    #[test]
    fn blank_lines_are_passed_over_and_a_record_that_does_not_parse_is_skipped() {
        let lines = [
            r#"{"type":"user","uuid":"u1","message":{}}"#,
            "",
            r#"{"type":"user","uuid":"u2" "message":{}}"#,
            "   ",
            r#"{"type":"summary"}"#,
        ]
        .join("\n");
        let reader = Box::new(io::Cursor::new(lines.into_bytes()));
        assert_eq!(scan_one(Agent::Claude, reader), (2, 1));
    }

    #[test]
    fn blank_lines_in_a_pi_session_are_passed_over() {
        let lines = [
            r#"{"type":"session","id":"s","timestamp":"2026-11-09T06:00:00.000Z"}"#,
            "",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-09T06:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
        ]
        .join("\n");
        let reader = Box::new(io::Cursor::new(lines.into_bytes()));
        let (records, skipped) = scan_one(Agent::Pi, reader);
        assert_eq!(skipped, 0);
        assert!(records > 0);
    }

    #[test]
    fn a_file_s_first_time_is_its_first_record_with_a_valid_timestamp() {
        let lines = [
            "not json",
            r#"{"type":"summary"}"#,
            r#"{"timestamp":"yesterday"}"#,
            r#"{"timestamp":"2026-11-08T01:00:00Z"}"#,
            r#"{"timestamp":"2026-11-08T02:00:00Z"}"#,
        ]
        .join("\n");
        let reader = Box::new(io::Cursor::new(lines.into_bytes()));
        assert_eq!(first_time_in(reader), parse_ts("2026-11-08T01:00:00Z"));
        assert_eq!(
            first_time_in(Box::new(io::Cursor::new(b"{}\n".to_vec()))),
            None
        );
    }

    #[test]
    fn a_transcript_of_one_over_long_line_counts_it_as_skipped() {
        let long = io::repeat(b'a').take(MAX_LINE + 10);
        assert_eq!(
            scan_one(Agent::Claude, Box::new(BufReader::new(long))),
            (0, 1)
        );
    }

    #[test]
    fn only_jsonl_and_compressed_jsonl_files_are_listed() {
        // nosemgrep: rust.lang.security.temp-dir.temp-dir -- a test fixture folder; no security decision rests on it
        let dir = std::env::temp_dir().join(format!("bigsister-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("temp folder");
        for name in [
            "a.jsonl",
            "sub/b.jsonl.zst",
            "notes.txt",
            "c.json",
            "d.zst",
            "jsonl",
        ] {
            fs::write(dir.join(name), "{}\n").expect("stub");
        }
        let mut st = RootStats::new(Agent::Claude, dir.clone());
        let mut got = list_files(&dir, SystemTime::UNIX_EPOCH, &mut st);
        got.sort();
        fs::remove_dir_all(&dir).expect("clean up");
        assert_eq!(
            got,
            [dir.join("a.jsonl"), dir.join("sub").join("b.jsonl.zst")]
        );
        assert_eq!(st.unreadable, 0);
    }

    #[test]
    fn raw_timestamps_for_both_agents() {
        let codex = r#"{"timestamp":"2026-11-08T01:00:00Z","type":"x","payload":{"timestamp":"2020-01-01T00:00:00Z"}}"#;
        assert_eq!(raw_timestamp(codex), parse_ts("2026-11-08T01:00:00Z"));
        let claude = r#"{"type":"user","message":{"content":"a \"timestamp\":\"x\""},"timestamp":"2026-11-08T02:00:00Z"}"#;
        assert_eq!(raw_timestamp(claude), parse_ts("2026-11-08T02:00:00Z"));
        assert_eq!(raw_timestamp(r#"{"type":"summary"}"#), None);
    }

    #[test]
    fn a_line_of_exactly_the_cap_is_read() {
        let record = format!("{{\"a\":\"{}\"}}", "a".repeat(MAX_LINE as usize - 8));
        assert_eq!(record.len() as u64, MAX_LINE);
        // At the end of the file, and before a newline and another line.
        for rest in [&b""[..], b"\n{}"] {
            let bytes = [record.as_bytes(), rest].concat();
            let mut lines = Lines::new(Box::new(io::Cursor::new(bytes)));
            let first = lines.next_line().expect("read").map(|l| l.len() as u64);
            assert_eq!((first, lines.over_long), (Some(MAX_LINE), 0), "{rest:?}");
            let next = lines.next_line().expect("read").map(Cow::into_owned);
            assert_eq!(next.as_deref(), (!rest.is_empty()).then_some("{}"));
        }
    }

    #[test]
    fn a_line_over_the_cap_is_skipped_and_counted() {
        let long = io::repeat(b'a').take(MAX_LINE + 10);
        let rest: &[u8] = b"\n{\"a\":1}\n";
        let mut lines = Lines::new(Box::new(BufReader::new(long.chain(rest))));
        let next = lines.next_line().expect("read").map(Cow::into_owned);
        assert_eq!(next.as_deref(), Some("{\"a\":1}\n"));
        assert_eq!(lines.over_long, 1);
        assert!(lines.next_line().expect("read").is_none());
    }

    fn test_dir(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("bigsister-{name}-{}", std::process::id()))
    }

    #[test]
    fn parallel_scan_preserves_counts_errors_and_order_across_files() {
        let dir = test_dir("par");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        fs::write(
            dir.join("a.jsonl"),
            concat!(
                r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T01:00:00Z","message":{"model":"claude-sonnet-4-6"}}"#,
                "\n\nnot-json\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2026-11-08T01:01:00Z","message":{}}"#,
                "\n",
            ),
        )
        .expect("a.jsonl");
        fs::write(dir.join("b.jsonl"), "").expect("empty file");
        fs::write(dir.join("c.jsonl.zst"), b"not-zstd").expect("non-zstd");
        fs::write(
            dir.join("d.jsonl.zst"),
            [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x01],
        )
        .expect("corrupt zstd");

        let prices = Prices::bundled();
        let since = Some(Timestamp::MIN);
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(WholeCount(0))];
        let stats = scan(&[(Agent::Claude, dir.clone())], since, &prices, &mut dets);
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].files, 3);
        assert_eq!(stats[0].compressed, 1);
        assert_eq!(stats[0].unreadable, 1);
        assert_eq!(stats[0].bad_lines, 1);
        assert_eq!(counted(&dets), 2);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn early_finish_and_pi_parallel_paths_record_errors_and_bad_lines() {
        let dir = test_dir("par-err");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("early")).expect("temp folder");
        fs::create_dir_all(dir.join("pi")).expect("temp folder");
        fs::write(dir.join("early").join("c.jsonl.zst"), b"not-zstd").expect("non-zstd");
        fs::write(
            dir.join("early").join("d.jsonl.zst"),
            [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x01],
        )
        .expect("corrupt zstd");
        fs::write(
            dir.join("pi").join("p.jsonl"),
            concat!(
                r#"{"type":"session","id":"s","timestamp":"2026-11-09T06:00:00.000Z"}"#,
                "\n\n{bad\n",
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-09T06:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .expect("p.jsonl");

        let prices = Prices::bundled();
        let since = Some(Timestamp::MIN);
        let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
        let rc_prices = std::rc::Rc::new(Prices::bundled());
        let mut early =
            crate::detectors::build(crate::cli::Command::Startup, &programs, &rc_prices);
        let early_stats = scan(
            &[(Agent::Claude, dir.join("early"))],
            since,
            &prices,
            &mut early,
        );
        assert_eq!(early_stats[0].files, 1);
        assert_eq!(early_stats[0].compressed, 1);
        assert_eq!(early_stats[0].unreadable, 1);

        let mut pi_dets =
            crate::detectors::build(crate::cli::Command::Tools, &programs, &rc_prices);
        let pi_stats = scan(&[(Agent::Pi, dir.join("pi"))], since, &prices, &mut pi_dets);
        assert_eq!(pi_stats[0].files, 1);
        assert_eq!(pi_stats[0].bad_lines, 1);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn streaming_scan_surfaces_mid_file_read_error() {
        struct FailAfterFirst(bool);
        impl Read for FailAfterFirst {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    let line = b"{\"type\":\"summary\"}\n";
                    buf[..line.len()].copy_from_slice(line);
                    Ok(line.len())
                } else {
                    Err(io::Error::other("broken stream"))
                }
            }
        }
        let mut st = RootStats::new(Agent::Claude, PathBuf::new());
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
        let err = scan_file(
            Box::new(BufReader::new(FailAfterFirst(false))),
            None,
            Path::new("s.jsonl"),
            &Window {
                since: Some(Timestamp::MIN),
                prices: &Prices::bundled(),
            },
            &mut dets,
            &[0],
            &mut st,
            &mut Earlier::default(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn exact_prefilters_match_detector_wants() {
        let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
        let prices = std::rc::Rc::new(Prices::bundled());
        let dets = crate::detectors::build(crate::cli::Command::All, &programs, &prices);
        let session = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let samples = [
            r#"{"type":"assistant","message":{}}"#,
            r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
            r#"{"type":"api-request-shape"}"#,
            r#"{"type":"summary"}"#,
        ];
        for d in &dets {
            if let Some(Prefilter::Exact(f)) = d.prefilter() {
                for line in samples {
                    assert_eq!(f(&session, line), d.wants(&session, line));
                }
            }
        }
    }

    struct WholeCount(u64);
    impl Detector for WholeCount {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude, Agent::Codex]
        }
        fn scans_whole_file(&self) -> bool {
            true
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, _r: &Record<'_>) {
            self.0 += 1;
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: vec![("records".to_owned(), self.0.into())],
                ..Section::default()
            }
        }
    }

    #[test]
    fn parallel_prep_coalesces_timestamps_and_streams_when_over_budget() {
        let dir = test_dir("budget");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        let body = [
            r#"{"type":"progress","timestamp":"2026-11-08T01:00:00Z"}"#,
            r#"{"type":"progress","timestamp":"2026-11-08T02:00:00Z"}"#,
            r#"{"type":"assistant","uuid":"u1","message":{"model":"claude-sonnet-4-6"}}"#,
            r#"{"type":"assistant","uuid":"u2","message":{"model":"claude-sonnet-4-6"}}"#,
        ]
        .join("\n");
        let file = dir.join("a.jsonl");
        fs::write(&file, format!("{body}\n")).expect("write");

        let prices = Prices::bundled();
        let window = Window {
            since: parse_ts("2026-11-08T01:30:00Z"),
            prices: &prices,
        };
        let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
        let rc_prices = std::rc::Rc::new(Prices::bundled());
        let mut dets = crate::detectors::build(crate::cli::Command::Models, &programs, &rc_prices);
        let mut st = RootStats::new(Agent::Claude, dir.clone());
        let mut earlier = Earlier::default();
        scan_files_with_budget(
            vec![file],
            &window,
            &mut dets,
            &[0],
            &mut st,
            &mut earlier,
            1,
            MAX_PREP_RAW_BYTES,
        );
        assert_eq!(st.files, 1);
        assert_eq!(st.unreadable, 0);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn more_than_64_active_detectors_do_not_overflow_bitmasks() {
        let prices = Prices::bundled();
        let mut dets: Vec<Box<dyn Detector>> = (0..65)
            .map(|_| Box::new(WholeCount(0)) as Box<dyn Detector>)
            .collect();
        let files = [Memory {
            agent: Agent::Claude,
            path: "s.jsonl",
            text: r#"{"type":"summary"}"#,
        }];
        let stats = scan_memory(&files, "mem", Some(Timestamp::MIN), &prices, &mut dets);
        assert_eq!(stats[0].files, 1);
        assert_eq!(counted(&dets[64..]), 1);
    }

    struct StopAfterOne(u64);
    impl Detector for StopAfterOne {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude]
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, _r: &Record<'_>) {
            self.0 += 1;
        }
        fn done(&self, _s: &Session) -> bool {
            self.0 >= 1
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: vec![("seen".to_owned(), self.0.into())],
                ..Section::default()
            }
        }
    }

    struct PriceLog(Vec<Option<f64>>);
    impl Detector for PriceLog {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Codex]
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, _r: &Record<'_>) {}
        fn read_by(&mut self, _s: &Session, p: Option<f64>) {
            self.0.push(p);
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: vec![("calls".to_owned(), (self.0.len() as u64).into())],
                ..Section::default()
            }
        }
    }

    #[test]
    fn legacy_done_detector_and_spaced_codex_rules_match_sequential() {
        let dir = test_dir("s3");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        fs::write(dir.join("s.jsonl"), "{\"type\":\"summary\"}\nnot-json\n").expect("write");
        let prices = Prices::bundled();
        let mut stop: Vec<Box<dyn Detector>> = vec![Box::new(StopAfterOne(0))];
        let st = scan(
            &[(Agent::Claude, dir.clone())],
            Some(Timestamp::MIN),
            &prices,
            &mut stop,
        );
        assert_eq!(st[0].bad_lines, 0);
        assert_eq!(counted(&stop), 1);

        let mut pl: Vec<Box<dyn Detector>> = vec![Box::new(PriceLog(Vec::new()))];
        let cm = [Memory {
            agent: Agent::Codex,
            path: "c.jsonl",
            text: "{\"type\": \"turn_context\", \"payload\": {\"model\": \"gpt-5.4\"}}\n",
        }];
        let _ = scan_memory(&cm, "mem", Some(Timestamp::MIN), &prices, &mut pl);
        assert_eq!(counted(&pl), 1);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    struct DynamicBlob {
        enabled: bool,
        seen: Vec<bool>,
    }
    impl Detector for DynamicBlob {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude]
        }
        fn scans_whole_file(&self) -> bool {
            true
        }
        fn prefilter(&self) -> Option<Prefilter> {
            Some(Prefilter::Superset(|_, line| line.contains("\"keep\"")))
        }
        fn reads_request_blobs(&self) -> bool {
            self.enabled
        }
        fn begin(&mut self, _s: &Session) {
            self.enabled = true;
        }
        fn wants(&self, _s: &Session, line: &str) -> bool {
            line.contains("\"keep\"")
        }
        fn record(&mut self, _s: &Session, r: &Record<'_>) {
            self.seen.push(r.in_window);
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: vec![
                    ("seen".to_owned(), (self.seen.len() as u64).into()),
                    (
                        "in_window".to_owned(),
                        (self.seen.iter().filter(|&&w| w).count() as u64).into(),
                    ),
                ],
                ..Section::default()
            }
        }
    }

    fn assert_dynamic_blob(dets: &[Box<dyn Detector>]) {
        let opts = Opts {
            limit: 0,
            paths: false,
            home: None,
            usd_per_mtok: None,
            prices_file: None,
        };
        assert_eq!(
            dets[0].section(&opts).summary,
            vec![
                ("seen".to_owned(), 1u64.into()),
                ("in_window".to_owned(), 0u64.into()),
            ]
        );
    }

    #[test]
    fn dynamic_blob_detector_receives_wanted_blobs_and_ignores_rejected_ones() {
        let blob_text = concat!(
            r#"{"type":"api-request-blob","timestamp":"2026-11-10T00:00:00Z","drop":true}"#,
            "\n",
            r#"{"type":"api-request-blob","broken":true"#,
            "\n",
            r#"{"type":"api-request-blob","keep":true}"#,
            "\n",
        );
        let prices = Prices::bundled();
        let cutoff = parse_ts("2026-11-09T00:00:00Z");
        let mut mem_dets: Vec<Box<dyn Detector>> = vec![Box::new(DynamicBlob {
            enabled: false,
            seen: Vec::new(),
        })];
        let mem = [Memory {
            agent: Agent::Claude,
            path: "b.jsonl",
            text: blob_text,
        }];
        assert_eq!(
            scan_memory(&mem, "mem", cutoff, &prices, &mut mem_dets)[0].bad_lines,
            0
        );
        assert_dynamic_blob(&mem_dets);

        let dir = test_dir("blob");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        fs::write(dir.join("b.jsonl"), blob_text).expect("blob file");
        let mut disk_dets: Vec<Box<dyn Detector>> = vec![Box::new(DynamicBlob {
            enabled: false,
            seen: Vec::new(),
        })];
        let st = scan(
            &[(Agent::Claude, dir.clone())],
            cutoff,
            &prices,
            &mut disk_dets,
        );
        assert_eq!(st[0].bad_lines, 0);
        assert_dynamic_blob(&disk_dets);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn pi_branch_events_count_toward_prep_budget() {
        let dir = test_dir("pi-budget");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        let path = dir.join("p.jsonl");
        let pi_text = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-11-08T01:00:00Z"}"#,
            "\n",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-08T01:01:00Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-11-08T01:02:00Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"content":[{"type":"text","text":"one"}]}}"#,
            "\n",
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-11-08T01:03:00Z","message":{"role":"user","content":[{"type":"text","text":"next"}]}}"#,
            "\n",
            r#"{"type":"message","id":"d","parentId":"b","timestamp":"2026-11-08T01:04:00Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"content":[{"type":"text","text":"two"}]}}"#,
            "\n",
        );
        fs::write(&path, pi_text).expect("pi file");
        let prices = Prices::bundled();
        let opts = PrepareOpts {
            agent: Agent::Pi,
            prefilters: None,
            prices: &prices,
            max_events: 4,
            max_raw_bytes: MAX_PREP_RAW_BYTES,
        };
        assert!(matches!(prepare_file(&path, &opts), PreparedFile::TooLarge));
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(
            crate::detectors::models::Models::new(std::rc::Rc::new(Prices::bundled())),
        )];
        let mut st = RootStats::new(Agent::Pi, dir.clone());
        let mut earlier = Earlier::default();
        let window = Window {
            since: Some(Timestamp::MIN),
            prices: &prices,
        };
        scan_files_with_budget(
            vec![path],
            &window,
            &mut dets,
            &[0],
            &mut st,
            &mut earlier,
            4,
            MAX_PREP_RAW_BYTES,
        );
        assert_eq!(st.files, 1);
        assert_eq!(st.bad_lines, 0);
        fs::remove_dir_all(&dir).expect("clean up");
    }

    struct Trace(Vec<(String, bool)>);
    impl Detector for Trace {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude]
        }
        fn scans_whole_file(&self) -> bool {
            true
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, r: &Record<'_>) {
            let id = r
                .value
                .get("uuid")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            self.0.push((id, r.in_window));
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section {
                summary: self
                    .0
                    .iter()
                    .map(|(id, w)| (id.clone(), (*w).into()))
                    .collect(),
                ..Section::default()
            }
        }
    }

    #[test]
    fn parallel_scan_preserves_cross_file_order_and_deduplication_trace() {
        let dir = test_dir("trace");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp folder");
        fs::write(
            dir.join("a-later.jsonl"),
            concat!(
                r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T02:00:00Z","message":{"id":"m1"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"u3","timestamp":"2026-11-08T02:01:00Z","message":{"id":"m1"}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2026-11-08T02:02:00Z","message":{}}"#,
                "\n",
            ),
        )
        .expect("later");
        fs::write(
            dir.join("z-earlier.jsonl"),
            r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T01:00:00Z","message":{"id":"m1"}}"#,
        )
        .expect("earlier");
        let prices = Prices::bundled();
        let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Trace(Vec::new()))];
        let _ = scan(
            &[(Agent::Claude, dir.clone())],
            Some(Timestamp::MIN),
            &prices,
            &mut dets,
        );
        let opts = Opts {
            limit: 0,
            paths: false,
            home: None,
            usd_per_mtok: None,
            prices_file: None,
        };
        assert_eq!(
            dets[0].section(&opts).summary,
            vec![
                ("u1".to_owned(), true.into()),
                ("u1".to_owned(), false.into()),
                ("u3".to_owned(), false.into()),
                ("u2".to_owned(), true.into()),
            ]
        );
        fs::remove_dir_all(&dir).expect("clean up");
    }
}
