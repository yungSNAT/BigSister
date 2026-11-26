//! Reading transcripts ahead in parallel batches, and the in-memory and OpenCode scans.

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use rayon::prelude::*;
use serde_json::Value;

use super::feed::{
    Earlier, Feed, Window, claude_model_name, codex_model_line, is_subagent, raw_timestamp,
    scan_file,
};
use super::files::{Lines, Open, one_line, open, read_error};
use super::{Agent, CLAUDE_BLOB_PREFIX, Detector, Prefilter, RootStats, Session};
use crate::antigravity::Antigravity;
use crate::gemini::{Gemini, Rewrite};
use crate::prices::Prices;

/// Files prepared in parallel per batch, bounding peak memory on large corpora.
const PARALLEL_BATCH: usize = 16;
/// Maximum prepared events or raw-buffer bytes per file before falling back to streaming.
const MAX_PREP_EVENTS: usize = 65_536;
pub(super) const MAX_PREP_RAW_BYTES: usize = 32 << 20;

/// Why opening or indexing a transcript failed before lines were read.
pub(super) enum OpenError {
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
        Agent::Gemini => indexed(reader, path, Gemini::index)
            .map(|(r, index)| (r, Some(Rewrite::Gemini(index))))
            .map_err(OpenError::Unreadable),
        Agent::Antigravity => indexed(reader, path, Antigravity::index)
            .map(|(r, index)| (r, Some(Rewrite::Antigravity(index))))
            .map_err(OpenError::Unreadable),
        Agent::Qwen => Ok((reader, Some(Rewrite::Qwen))),
        Agent::Otlp if path.extension().is_some_and(|x| x == "json") => Ok((
            one_line(reader),
            Some(Rewrite::Otlp(crate::otlp::Otlp::default())),
        )),
        Agent::Otlp => Ok((reader, Some(Rewrite::Otlp(crate::otlp::Otlp::default())))),
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

pub(super) struct PrepareOpts<'a> {
    pub(super) agent: Agent,
    pub(super) prefilters: Option<&'a [Prefilter]>,
    pub(super) prices: &'a Prices,
    pub(super) max_events: usize,
    pub(super) max_raw_bytes: usize,
}

pub(super) enum PreparedFile {
    OpenErr(OpenError),
    Empty(u64),
    TooLarge,
    Ready(ReadyFile),
}

pub(super) struct ReadyFile {
    session: Session,
    bad_lines: u64,
    raw_buf: String,
    events: Vec<PreparedEvent>,
    read_err: Option<String>,
}

pub(super) enum PreparedEvent {
    PiParent(Option<String>),
    Branch(Vec<PreparedEvent>),
    Timestamp(Timestamp),
    Candidate(Candidate),
}

pub(super) struct Candidate {
    pub(super) raw: std::ops::Range<usize>,
    pub(super) blob: bool,
    pub(super) codex_model: bool,
    pub(super) exact_mask: u64,
    pub(super) check_mask: u64,
    pub(super) reply: Option<Option<f64>>,
    pub(super) parsed: Option<Value>,
}

pub(super) struct Prep<'a> {
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
    pub(super) fn new(
        session: Session,
        opts: &'a PrepareOpts<'a>,
        rewrite: Option<Rewrite>,
    ) -> Self {
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

    pub(super) fn drain_into(&mut self, line: &str, feed: &mut Feed<'_, '_>) {
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

pub(super) fn prepare_file(path: &Path, opts: &PrepareOpts<'_>) -> PreparedFile {
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
    dets: &mut [&mut (dyn Detector + 'static)],
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
    dets: &mut [&mut (dyn Detector + 'static)],
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
pub(super) fn scan_files(
    files: Vec<PathBuf>,
    window: &Window<'_>,
    dets: &mut [&mut (dyn Detector + 'static)],
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
pub(super) fn scan_files_with_budget(
    files: Vec<PathBuf>,
    window: &Window<'_>,
    dets: &mut [&mut (dyn Detector + 'static)],
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

/// Indexes Gemini CLI or Antigravity file `path` from `reader` with `note`
/// ([`Gemini::index`], [`Antigravity::index`]), then opens it again for the second pass,
/// or says why either cannot be read.
pub(super) fn indexed<T: Default>(
    reader: Box<dyn BufRead>,
    path: &Path,
    note: fn(&mut T, &str),
) -> Result<(Box<dyn BufRead>, T), String> {
    let mut lines = Lines::new(reader);
    let mut index = T::default();
    while let Some(line) = lines.next_line().map_err(|e| read_error(&e))? {
        note(&mut index, &line);
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
pub(super) fn scan_opencode(
    root: &Path,
    window: &Window<'_>,
    dets: &mut [&mut (dyn Detector + 'static)],
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
