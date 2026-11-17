//! One pass over transcript files, feeding each wanted record to the detectors.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use jiff::Timestamp;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use serde_json::Value;

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
}

impl Agent {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Pi => "pi",
            Agent::OpenCode => "opencode",
        }
    }

    /// Whether the agent's records reach the detectors as Claude Code records: pi's and
    /// OpenCode's are rewritten into them ([`crate::pi`], [`crate::opencode`]).
    #[must_use]
    pub fn claude_shaped(self) -> bool {
        matches!(self, Agent::Claude | Agent::Pi | Agent::OpenCode)
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
    fn end(&mut self, _s: &Session) {}
    /// A model has read the session's context so far: its uncached input price per
    /// million tokens, or `None` when it has no price or no model did.
    fn read_by(&mut self, _s: &Session, _usd_per_mtok: Option<f64>) {}
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
    /// Counts one unreadable file or folder, and why.
    fn count_unreadable(&mut self, reason: String) {
        self.unreadable += 1;
        *self.unreadable_reasons.entry(reason).or_default() += 1;
    }
}

/// Claude's request blobs repeat message content already in the transcript.
const CLAUDE_SKIP_PREFIX: &str = "{\"type\":\"api-request-blob\"";

/// Scans every transcript under each root that a chosen detector reads and that
/// was modified at or after `since`, pricing what each model reads at `prices`.
pub fn scan(
    roots: &[(Agent, PathBuf)],
    since: Timestamp,
    prices: &Prices,
    dets: &mut [Box<dyn Detector>],
) -> Vec<RootStats> {
    let window = Window { since, prices };
    let cutoff = u64::try_from(since.as_second())
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
        let mut st = RootStats {
            agent: *agent,
            root: root.clone(),
            files: 0,
            unreadable: 0,
            bad_lines: 0,
            compressed: 0,
            unreadable_reasons: BTreeMap::new(),
        };
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
            files.sort_by_cached_key(|p| first_time(p));
        }
        scan_files(files, &window, dets, &active, &mut st, &mut earlier);
        out.push(st);
    }
    out
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
    for path in files {
        let reader = match open(&path) {
            Ok(r) => r,
            Err(Open::NotZstd) => {
                st.compressed += 1;
                continue;
            }
            Err(Open::Io(reason)) => {
                st.files += 1;
                st.count_unreadable(reason);
                continue;
            }
        };
        st.files += 1;
        if let Err(e) = scan_file(reader, &path, window, dets, active, st, earlier) {
            st.count_unreadable(read_error(&e));
        }
    }
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
        let _ = scan_file(reader, &db, window, dets, active, st, earlier);
    }
}

fn list_files(root: &Path, cutoff: SystemTime, st: &mut RootStats) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                st.count_unreadable(format!("a folder cannot be listed: {e}"));
                continue;
            }
        };
        for entry in entries {
            let (entry, ft) = match entry.and_then(|e| e.file_type().map(|ft| (e, ft))) {
                Ok(read) => read,
                Err(e) => {
                    st.count_unreadable(format!("a folder entry cannot be read: {e}"));
                    continue;
                }
            };
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
    let mut lines = Lines::new(open(path).ok()?);
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
    since: Timestamp,
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

fn scan_file(
    reader: Box<dyn BufRead>,
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
    for &i in active {
        dets[i].begin(&session);
    }
    let mut feed = Feed {
        session,
        since: window.since,
        prices: window.prices,
        last_time: None,
        codex_model: None,
        uuids: HashSet::new(),
        replies: HashSet::new(),
        earlier,
        pi: (agent == Agent::Pi).then(crate::pi::Tree::default),
        replaying: false,
        dets,
        active,
        st,
    };
    feed.take(&first);
    let result = loop {
        if feed.active.iter().all(|&i| feed.dets[i].done(&session)) {
            break Ok(());
        }
        match lines.next_line() {
            Ok(Some(line)) => feed.take(line.trim_end()),
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    feed.st.bad_lines += lines.over_long;
    // A Codex turn's model reads its results; a Claude result no reply read stays
    // unpriced.
    if agent == Agent::Codex {
        let price = feed.codex_price(|| None);
        feed.read_by(price);
    }
    for &i in active {
        feed.dets[i].end(&session);
    }
    earlier.add(agent, path, feed.uuids, feed.replies);
    result
}

// shortcut: a Claude subagent is told apart by its `subagents` folder and a Codex one
// by `parent_thread_id` in its first record, as both agents lay them out in November
// 2026. A layout change makes subagents count as main sessions; a record field naming
// the parent would be the upgrade.
fn is_subagent(agent: Agent, path: &Path, first: &str) -> bool {
    match agent {
        Agent::Claude => path.components().any(|c| c.as_os_str() == "subagents"),
        Agent::Codex => serde_json::from_str::<Value>(first).is_ok_and(|v| {
            v.pointer("/payload/parent_thread_id")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
        }),
        Agent::Pi => false,
        Agent::OpenCode => serde_json::from_str::<Value>(first)
            .is_ok_and(|v| v.get("subagent") == Some(&Value::Bool(true))),
    }
}

struct Feed<'a> {
    session: Session,
    since: Timestamp,
    prices: &'a Prices,
    last_time: Option<Timestamp>,
    /// The model the latest Codex `turn_context` names, if any.
    codex_model: Option<String>,
    /// The `uuid` of each Claude record parsed so far in the file.
    uuids: HashSet<String>,
    /// The message id of each Claude Code reply parsed so far in the file.
    replies: HashSet<String>,
    earlier: &'a Earlier,
    /// A pi session's tree; `None` for other agents.
    pi: Option<crate::pi::Tree>,
    /// Whether the lines fed are a pi path given again, which count nowhere.
    replaying: bool,
    dets: &'a mut [Box<dyn Detector>],
    active: &'a [usize],
    st: &'a mut RootStats,
}

impl Feed<'_> {
    /// Feeds one line, a pi entry as the Claude Code record it is rewritten to, after
    /// the path it branches from.
    fn take(&mut self, line: &str) {
        let Some(tree) = self.pi.as_mut() else {
            return self.line(line);
        };
        if line.is_empty() {
            return;
        }
        let Some(read) = tree.read(line) else {
            self.st.bad_lines += 1;
            return;
        };
        if let Some(path) = read.branch {
            for &i in self.active {
                self.dets[i].branch(&self.session);
            }
            self.replaying = true;
            for old in &path {
                self.line(old);
            }
            self.replaying = false;
        }
        self.line(&read.line);
    }

    fn line(&mut self, line: &str) {
        if line.is_empty()
            || (self.session.agent == Agent::Claude && line.starts_with(CLAUDE_SKIP_PREFIX))
        {
            return;
        }
        if !line.ends_with('}') {
            self.st.bad_lines += 1;
            return;
        }
        let reply = self.model(line);
        let wanting: Vec<usize> = self
            .active
            .iter()
            .copied()
            .filter(|&i| self.dets[i].wants(&self.session, line))
            .collect();
        if wanting.is_empty() && reply.is_none() {
            if let Some(t) = raw_timestamp(line) {
                self.last_time = Some(t);
            }
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.st.bad_lines += 1;
            return;
        };
        let Some(copied) = self.copied(&value) else {
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
            value: &value,
            in_window: !copied && self.last_time.is_some_and(|t| t >= self.since),
        };
        for i in wanting {
            self.dets[i].record(&self.session, &rec);
        }
    }
}

impl Feed<'_> {
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
        Some(match &self.pi {
            None if self.session.agent == Agent::Claude => {
                let reply = value
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .filter(|_| value.get("type").and_then(Value::as_str) == Some("assistant"));
                self.replies.extend(reply.map(str::to_owned));
                self.earlier.claude.contains(uuid)
                    || reply.is_some_and(|id| self.earlier.replies.contains(id))
            }
            None => self.earlier.claude.contains(uuid),
            Some(tree) => tree
                .parent
                .as_ref()
                .and_then(|name| self.earlier.pi.get(name))
                .is_some_and(|uuids| uuids.contains(uuid)),
        })
    }

    fn read_by(&mut self, usd_per_mtok: Option<f64>) {
        for &i in self.active {
            self.dets[i].read_by(&self.session, usd_per_mtok);
        }
    }

    /// Finds where a model reads what came before. A Codex model call's usage record,
    /// and a new turn, which ends the last, are where the turn's model read the results
    /// not yet read; the detectors are told here. A
    /// Claude reply reads the results before it: its model's price, if it has one, is
    /// returned for `line` to pass on once the record is known to be no copy.
    ///
    /// shortcut: a Claude reply is found by `"message":{"model":"`, as Claude Code writes
    /// `model` first in an assistant message, so a reply written otherwise reads nothing
    /// and its results stay unpriced; parsing every line would be the upgrade, at
    /// several times the cost. Each reply's uuid is kept to find copies, which adds
    /// replies no detector reads to the uuids held for the scan.
    fn model(&mut self, line: &str) -> Option<Option<f64>> {
        match self.session.agent {
            Agent::Claude | Agent::Pi | Agent::OpenCode => {
                const KEY: &str = "\"message\":{\"model\":\"";
                let at = line.find(KEY)?;
                let rest = &line[at + KEY.len()..];
                let model = rest.split('"').next().unwrap_or("");
                // Claude Code writes a synthetic reply itself; no model read anything.
                (model != "<synthetic>")
                    .then(|| self.prices.usd_per_mtok(model, || prompt_tokens(line)))
            }
            Agent::Codex => {
                if !line.contains("\"type\":\"turn_context\"")
                    && !line.contains("\"type\":\"token_usage_record\"")
                {
                    return None;
                }
                let v = serde_json::from_str::<Value>(line).ok()?;
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
                None
            }
        }
    }

    /// The price of the current Codex turn's model, for a call whose prompt size
    /// `prompt_tokens` gives.
    fn codex_price(&self, prompt_tokens: impl FnOnce() -> Option<u64>) -> Option<f64> {
        self.prices
            .usd_per_mtok(self.codex_model.as_deref()?, prompt_tokens)
    }
}

/// The prompt a Claude reply was sent with, in tokens, as
/// [`crate::detectors::startup::input_tokens`] counts it. Parses the line, so it is
/// asked for only by a model priced by prompt size.
fn prompt_tokens(line: &str) -> Option<u64> {
    crate::detectors::startup::input_tokens(&serde_json::from_str(line).ok()?)
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

    /// Counts the records it is given.
    struct Count(std::rc::Rc<std::cell::Cell<u64>>);

    impl Detector for Count {
        fn agents(&self) -> &'static [Agent] {
            &[Agent::Claude, Agent::Codex]
        }
        fn wants(&self, _s: &Session, _line: &str) -> bool {
            true
        }
        fn record(&mut self, _s: &Session, _r: &Record<'_>) {
            self.0.set(self.0.get() + 1);
        }
        fn section(&self, _opts: &Opts) -> Section {
            Section::default()
        }
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
            let mut st = RootStats {
                agent,
                root: PathBuf::new(),
                files: 0,
                unreadable: 0,
                bad_lines: 0,
                compressed: 0,
                unreadable_reasons: BTreeMap::new(),
            };
            let n = std::rc::Rc::default();
            let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count(std::rc::Rc::clone(&n)))];
            let reader = Box::new(io::Cursor::new(lines.clone().into_bytes()));
            let path = Path::new("s.jsonl");
            scan_file(
                reader,
                path,
                &Window {
                    since: Timestamp::MIN,
                    prices: &Prices::bundled(),
                },
                &mut dets,
                &[0],
                &mut st,
                &mut Earlier::default(),
            )
            .expect("read");
            assert_eq!(n.get(), want, "{agent:?}");
        }
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
}
