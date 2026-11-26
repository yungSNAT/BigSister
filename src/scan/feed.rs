//! Feeding one transcript's records to the detectors.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::path::Path;

use jiff::Timestamp;
use serde_json::Value;

use super::files::Lines;
use super::prep::{Prep, PrepareOpts, PreparedEvent};
use super::{Agent, Detector, Prefilter, Record, RootStats, Session};
use crate::gemini::Rewrite;
use crate::prices::Prices;
use crate::time::parse_ts;

/// What every transcript of a scan is read against.
pub(super) struct Window<'a> {
    /// `None` starts the window after every instant.
    pub(super) since: Option<Timestamp>,
    /// Exclusive upper bound, or `None` for open-ended scans.
    pub(super) until: Option<Timestamp>,
    pub(super) prices: &'a Prices,
}

/// The `uuid` of each Claude Code or pi record parsed in an earlier file, and the message
/// id of each Claude Code reply. Gemini CLI, Qwen Code and OpenCode sessions do not copy
/// records across files and use per-session short IDs such as `g-0`, so they are not
/// stored or checked here.
///
/// shortcut: every uuid and message id of the scan is held as a string, about 100 bytes
/// each (a million uuids added 96 MB of peak memory, 14 November 2026). Holding a uuid as
/// the 128-bit number it spells would cut that to about a quarter.
#[derive(Default)]
pub(super) struct Earlier {
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
    pub(super) fn add(
        &mut self,
        agent: Agent,
        path: &Path,
        uuids: HashSet<String>,
        replies: HashSet<String>,
    ) {
        match agent {
            Agent::Claude => {
                self.claude.extend(uuids);
                self.replies.extend(replies);
            }
            Agent::Pi => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                self.pi.insert(name.into_owned(), uuids);
            }
            Agent::Codex
            | Agent::OpenCode
            | Agent::Gemini
            | Agent::Qwen
            | Agent::Antigravity
            | Agent::Otlp => {}
        }
    }
}

/// Reads a transcript from `reader`, each line through `rewrite` for an agent whose lines
/// are rewritten into Claude Code records.
#[expect(
    clippy::too_many_arguments,
    reason = "the transcript's reader, rewrite and path, and the scan's state"
)]
pub(super) fn scan_file(
    reader: Box<dyn BufRead>,
    rewrite: Option<Rewrite>,
    path: &Path,
    window: &Window<'_>,
    dets: &mut [&mut (dyn Detector + 'static)],
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
    let over_long = seek_window(&mut lines, &mut feed, result.is_ok());
    let (uuids, replies) = feed.finish(over_long, None);
    earlier.add(agent, path, uuids, replies);
    result
}

/// Reads the rest of a file its detectors stopped early, such as `startup` at a session
/// that began before the window, only for a record time in the window, so that the file
/// still counts as holding records in the window, when `seek` allows. Returns the lines
/// skipped for length before it, which are the file's.
fn seek_window(lines: &mut Lines, feed: &mut Feed<'_, '_>, seek: bool) -> u64 {
    let over_long = lines.over_long;
    while seek && !feed.saw_record {
        let Ok(Some(line)) = lines.next_line() else {
            break;
        };
        feed.saw_record = raw_timestamp(&line).is_some_and(|t| feed.covers(Some(t)));
    }
    over_long
}

// shortcut: a Claude subagent is told apart by its `subagents` folder and a Codex one
// by `parent_thread_id` in its first record, as both agents lay them out in November
// 2026. A layout change makes subagents count as main sessions; a record field naming
// the parent would be the upgrade. pi, Gemini CLI and Qwen Code sessions count as main
// sessions: Gemini CLI's subagent header field and Qwen Code's parent record are unread.
pub(super) fn is_subagent(agent: Agent, path: &Path, first: &str) -> bool {
    match agent {
        Agent::Claude => path.components().any(|c| c.as_os_str() == "subagents"),
        Agent::Codex => serde_json::from_str::<Value>(first).is_ok_and(|v| {
            v.pointer("/payload/parent_thread_id")
                .and_then(Value::as_str)
                .is_some_and(|p| !p.is_empty())
        }),
        Agent::Pi | Agent::Gemini | Agent::Qwen | Agent::Antigravity | Agent::Otlp => false,
        Agent::OpenCode => serde_json::from_str::<Value>(first)
            .is_ok_and(|v| v.get("subagent") == Some(&Value::Bool(true))),
    }
}

pub(super) struct Feed<'a, 'b> {
    session: Session,
    since: Option<Timestamp>,
    until: Option<Timestamp>,
    prices: &'a Prices,
    last_time: Option<Timestamp>,
    /// Whether a record's time in the file fell in the window.
    saw_record: bool,
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
    dets: &'a mut [&'b mut (dyn Detector + 'static)],
    active: &'a [usize],
    wanting_buf: Vec<usize>,
    pub(super) st: &'a mut RootStats,
}

impl<'a, 'b> Feed<'a, 'b> {
    pub(super) fn new(
        session: Session,
        window: &Window<'a>,
        earlier: &'a Earlier,
        dets: &'a mut [&'b mut (dyn Detector + 'static)],
        active: &'a [usize],
        st: &'a mut RootStats,
    ) -> Self {
        Feed {
            session,
            since: window.since,
            until: window.until,
            prices: window.prices,
            last_time: None,
            saw_record: false,
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

    /// Notes the time of the latest record read.
    fn at(&mut self, t: Timestamp) {
        self.last_time = Some(t);
        self.saw_record |= self.in_window();
    }

    fn in_window(&self) -> bool {
        self.covers(self.last_time)
    }

    /// Whether time `t` falls in the window.
    fn covers(&self, t: Option<Timestamp>) -> bool {
        let after_since = self.since.zip(t).is_some_and(|(s, t)| t >= s);
        let before_until = self.until.zip(t).is_none_or(|(u, t)| t < u);
        after_since && before_until
    }

    pub(super) fn apply_event(&mut self, event: &PreparedEvent, raw_buf: &str) {
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
            PreparedEvent::Timestamp(t) => self.at(*t),
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
                        self.at(t);
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
        if let Some(t) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts)
        {
            self.at(t);
        }
        let in_window = !copied && self.in_window();
        // A reply reads the results before it; a copy of one reads nothing.
        if let Some(price) = reply
            && !copied
        {
            self.read_by(price, in_window);
        }
        let rec = Record { value, in_window };
        for &i in &self.wanting_buf {
            self.dets[i].record(&self.session, &rec);
        }
    }

    pub(super) fn finish(
        mut self,
        extra_bad: u64,
        read_err: Option<&str>,
    ) -> (HashSet<String>, HashSet<String>) {
        self.st.bad_lines += extra_bad;
        self.st.files_with_records += u64::from(self.saw_record);
        // A Codex turn's model reads its results; a Claude result no reply read stays
        // unpriced.
        if self.session.agent == Agent::Codex {
            self.read_by(self.codex_price(|| None), self.in_window());
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
    /// repeats one of this file's. Only Claude Code and pi carry cross-file copies:
    /// Claude Code can write a record again later in its file with the same uuid (skipped),
    /// start a resumed session's file with copies of an earlier file's records, or write a
    /// reply's block in another file under a new uuid; pi forks copy the file named in
    /// their header, or replay a branch path within a file. Gemini CLI, Qwen Code and
    /// OpenCode never copy across files, so their per-session short IDs never collide.
    fn copied(&mut self, value: &Value) -> Option<bool> {
        if self.replaying {
            return Some(true);
        }
        if !matches!(self.session.agent, Agent::Claude | Agent::Pi) {
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
        } else {
            let reply = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .filter(|_| value.get("type").and_then(Value::as_str) == Some("assistant"));
            self.replies.extend(reply.map(str::to_owned));
            self.earlier.claude.contains(uuid)
                || reply.is_some_and(|id| self.earlier.replies.contains(id))
        })
    }

    fn read_by(&mut self, usd_per_mtok: Option<f64>, in_window: bool) {
        for &i in self.active {
            self.dets[i].read_by(&self.session, usd_per_mtok, in_window);
        }
    }

    fn codex_model(&mut self, v: &Value) {
        let in_window = self.in_window();
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
                self.read_by(price, in_window);
            }
            Some("turn_context") => {
                let ended = self.codex_price(|| None);
                self.read_by(ended, in_window);
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

pub(super) fn claude_model_name(line: &str) -> Option<&str> {
    const KEY: &str = "\"message\":{\"model\":\"";
    let at = line.find(KEY)?;
    let rest = &line[at + KEY.len()..];
    let model = rest.split('"').next().unwrap_or("");
    // Claude Code writes a synthetic reply itself; no model read anything.
    (model != "<synthetic>").then_some(model)
}

pub(super) fn codex_model_line(line: &str) -> bool {
    line.contains("\"type\":\"turn_context\"") || line.contains("\"type\":\"token_usage_record\"")
}

/// A record's top-level `timestamp`, read without parsing the line: Codex writes
/// it as the first key, Claude near the end after the message.
///
/// shortcut: in a Claude line, a nested object after the top-level key that has its
/// own `timestamp` is read instead. Only records without a timestamp of their own
/// take this time; parsing every line would be exact but slower.
pub(super) fn raw_timestamp(line: &str) -> Option<Timestamp> {
    const KEY: &str = "\"timestamp\":\"";
    let at = if line.strip_prefix('{').is_some_and(|r| r.starts_with(KEY)) {
        1
    } else {
        line.rfind(KEY)?
    };
    let rest = &line[at + KEY.len()..];
    parse_ts(&rest[..rest.find('"')?])
}
