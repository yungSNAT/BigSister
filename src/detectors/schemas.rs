//! Tool schemas loaded into Claude requests against the tools called, and skills
//! listed against the skills used.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::startup::loaded_tools;
use super::tools::printed;
use super::{EST_NOTE, chars, est_tokens, input_str, name_of, tool_uses};
use crate::prices::{self, Priced};
use crate::report::{Opts, Section, Table, median};
use crate::scan::{Agent, Detector, Record, Session};

const COLUMNS: [&str; 11] = [
    "scope",
    "tool",
    "tools",
    "sessions_loaded",
    "sessions_called",
    "calls",
    "median_chars",
    "idle_chars",
    "idle_tokens_est",
    "idle_usd_est",
    "unpriced_tokens_est",
];

const SKILL_COLUMNS: [&str; 10] = [
    "scope",
    "skill",
    "sessions_listed",
    "sessions_used",
    "uses",
    "median_chars",
    "idle_chars",
    "idle_tokens_est",
    "idle_usd_est",
    "unpriced_tokens_est",
];

#[derive(Default)]
struct Current {
    loaded: BTreeMap<String, u64>,
    called: BTreeMap<String, u64>,
    in_window: bool,
    /// Each skill a listing in the window named, with its listing characters.
    listed: BTreeMap<String, u64>,
    /// Each skill a `Skill` call or slash command in the window used, with its uses.
    used: BTreeMap<String, u64>,
    /// The price of the models of the session's replies, once one is seen: `None`
    /// inside when a model has no price or 2 models' prices differ.
    model: Option<Option<f64>>,
}

#[derive(Default)]
struct Group {
    tools: BTreeSet<String>,
    sessions_loaded: u64,
    sessions_called: u64,
    calls: u64,
    chars: Vec<u64>,
    idle_chars: u64,
    /// The part of `idle_chars` a priced model read.
    idle_priced: Priced,
}

/// Loaded-against-called counts, by tool or MCP server, and listed-against-used counts,
/// by skill.
#[derive(Default)]
pub struct Schemas {
    cur: Current,
    groups: BTreeMap<(&'static str, String), Group>,
    /// By scope and skill as printed; `sessions_loaded` counts sessions that listed it,
    /// `sessions_called` those that used it, `calls` its uses.
    skills: BTreeMap<(&'static str, String), Group>,
    sessions: BTreeMap<&'static str, u64>,
    /// Skill listings in the window without a `names` array, which list no skill.
    listings_skipped: u64,
    /// `Skill` calls in the window without a string skill, which use no skill.
    calls_skipped: u64,
}

/// A built-in tool's own name, or `mcp__<server>__*` for an MCP tool.
#[must_use]
pub fn group_of(tool: &str) -> String {
    match tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
    {
        Some(server) => format!("mcp__{server}__*"),
        None => tool.to_owned(),
    }
}

fn schemas_wants(_s: &Session, line: &str) -> bool {
    line.contains("\"type\":\"api-request-shape\"")
        || line.contains("\"tool_use\"")
        || line.contains("\"type\":\"skill_listing\"")
        || line.contains(SLASH)
}

impl Detector for Schemas {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        schemas_wants(s, line)
    }

    fn scans_whole_file(&self) -> bool {
        true
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Exact(schemas_wants))
    }

    fn begin(&mut self, _s: &Session) {
        self.cur = Current::default();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>, in_window: bool) {
        if !in_window {
            return;
        }
        let same = self.cur.model.is_none_or(|m| m == usd_per_mtok);
        self.cur.model = Some(usd_per_mtok.filter(|_| same));
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        if !r.in_window {
            return;
        }
        let cur = &mut self.cur;
        match r.kind() {
            "api-request-shape" => {
                cur.in_window = true;
                for (name, n) in loaded_tools(r.value) {
                    let e = cur.loaded.entry(name.to_owned()).or_default();
                    *e = (*e).max(n);
                }
            }
            "attachment" => {
                let listing = r
                    .value
                    .get("attachment")
                    .filter(|a| a.get("type").and_then(Value::as_str) == Some("skill_listing"));
                if listing.is_some_and(|l| l.get("names").and_then(Value::as_array).is_none()) {
                    self.listings_skipped += 1;
                }
                for (name, n) in listing.map(listed_skills).unwrap_or_default() {
                    let e = cur.listed.entry(name.to_owned()).or_default();
                    *e = (*e).max(n);
                }
            }
            "user" => {
                if let Some(name) = slash_command(r.value) {
                    *cur.used.entry(name.to_owned()).or_default() += 1;
                }
            }
            _ => {}
        }
        for b in tool_uses(r) {
            *cur.called.entry(name_of(b).to_owned()).or_default() += 1;
            if name_of(b) == "Skill" {
                match input_str(b, "skill") {
                    Some(skill) => *cur.used.entry(skill.to_owned()).or_default() += 1,
                    None => self.calls_skipped += 1,
                }
            }
        }
    }

    fn end(&mut self, s: &Session) {
        let cur = std::mem::take(&mut self.cur);
        self.end_skills(s, &cur);
        if !cur.in_window {
            return;
        }
        *self.sessions.entry(s.scope()).or_default() += 1;
        let mut per_group: BTreeMap<String, (u64, u64, Vec<&String>)> = BTreeMap::new();
        for (tool, n) in &cur.loaded {
            let g = per_group.entry(group_of(tool)).or_default();
            g.0 += n;
            g.2.push(tool);
        }
        for (tool, calls) in &cur.called {
            if let Some(g) = per_group.get_mut(&group_of(tool)) {
                g.1 += calls;
            }
        }
        for (name, (chars, calls, tools)) in per_group {
            let g = self.groups.entry((s.scope(), name)).or_default();
            g.tools.extend(tools.into_iter().cloned());
            g.sessions_loaded += 1;
            g.calls += calls;
            g.chars.push(chars);
            if calls > 0 {
                g.sessions_called += 1;
            } else {
                g.idle_chars += chars;
                g.idle_priced.add(chars, cur.model.flatten());
            }
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .groups
            .iter()
            .map(|((scope, name), g)| {
                (
                    g.idle_chars,
                    vec![
                        json!(scope),
                        json!(name),
                        json!(g.tools.len()),
                        json!(g.sessions_loaded),
                        json!(g.sessions_called),
                        json!(g.calls),
                        json!(median(&g.chars)),
                        json!(g.idle_chars),
                        json!(est_tokens(g.idle_chars)),
                        json!(g.idle_priced.usd()),
                        json!(g.idle_priced.unpriced_tokens(g.idle_chars)),
                    ],
                )
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new(
            "By tool or MCP server, most idle characters first",
            COLUMNS.to_vec(),
        );
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "schemas",
            title: "tool schemas loaded against tools called, and skills listed against skills used (Claude)",
            summary: vec![
                ("main_sessions".to_owned(), json!(self.sessions.get("main").copied().unwrap_or(0))),
                ("subagent_sessions".to_owned(), json!(self.sessions.get("subagent").copied().unwrap_or(0))),
                ("skill_listings_skipped".to_owned(), json!(self.listings_skipped)),
                ("skill_calls_skipped".to_owned(), json!(self.calls_skipped)),
            ],
            tables: vec![table, self.skill_table(opts)],
            notes: vec![
                "Idle characters are schema characters loaded in sessions that never called the tool.".to_owned(),
                "A skill is listed when a skill listing in the window names it, which costs its listing characters: its line and the lines after it up to the next skill's, each with a newline. It is used when a Skill call names it or a slash command runs it. Its idle characters are those listed in sessions that never used it. A listing without a names array and a Skill call without a string skill are counted as skipped.".to_owned(),
                EST_NOTE.to_owned(),
                "Idle characters, of schemas and skills alike, are priced at the model of the session's replies, and stay unpriced when its replies' models differ in price.".to_owned(),
                prices::NOTE.to_owned(),
                "Tools sent with defer_loading are not counted as loaded, and a tool only ever sent that way gets no row.".to_owned(),
            ],
        }
    }
}

impl Schemas {
    /// The table by scope and skill, most idle characters first.
    fn skill_table(&self, opts: &Opts) -> Table {
        let mut skills: Vec<(u64, Vec<Value>)> = self
            .skills
            .iter()
            .map(|((scope, name), g)| {
                (
                    g.idle_chars,
                    vec![
                        json!(scope),
                        json!(name),
                        json!(g.sessions_loaded),
                        json!(g.sessions_called),
                        json!(g.calls),
                        json!(median(&g.chars)),
                        json!(g.idle_chars),
                        json!(est_tokens(g.idle_chars)),
                        json!(g.idle_priced.usd()),
                        json!(g.idle_priced.unpriced_tokens(g.idle_chars)),
                    ],
                )
            })
            .collect();
        skills.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut skill_table = Table::new(
            "By skill, most idle characters first",
            SKILL_COLUMNS.to_vec(),
        );
        skill_table.rows = skills.into_iter().map(|(_, r)| r).collect();
        skill_table.truncate(opts.limit);
        skill_table
    }

    /// Adds a session's skills, listed in the window, against its uses.
    fn end_skills(&mut self, s: &Session, cur: &Current) {
        // Names that print alike, as `(other)` does, count as one skill of the session.
        let mut per_skill: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for (name, n) in &cur.listed {
            let e = per_skill.entry(printed(name)).or_default();
            e.0 += n;
            e.1 += cur.used.get(name).copied().unwrap_or(0);
        }
        for (name, (chars, uses)) in per_skill {
            let g = self.skills.entry((s.scope(), name)).or_default();
            g.sessions_loaded += 1;
            g.calls += uses;
            g.chars.push(chars);
            if uses > 0 {
                g.sessions_called += 1;
            } else {
                g.idle_chars += chars;
                g.idle_priced.add(chars, cur.model.flatten());
            }
        }
    }
}

/// What starts a slash command's name in the user message Claude Code writes for it.
const SLASH: &str = "<command-name>/";

/// The command a user's slash command ran, from a user message that is text, as
/// Claude Code writes `<command-name>/name</command-name>` there; a tool result, which
/// can quote such text, is not read.
fn slash_command(user: &Value) -> Option<&str> {
    let text = user.pointer("/message/content")?.as_str()?;
    let rest = &text[text.find(SLASH)? + SLASH.len()..];
    rest.get(..rest.find("</command-name>")?)
}

/// Each skill a `skill_listing` attachment names, with its characters in the listing:
/// the line `- name`, or a line starting `- name:`, and the lines after it up to the next
/// such line, each with a newline. A name without such a line has 0; a listing without
/// `names` lists none.
fn listed_skills(listing: &Value) -> Vec<(&str, u64)> {
    let names: Vec<&str> = listing
        .get("names")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut sizes: Vec<(&str, u64)> = names.iter().map(|n| (*n, 0)).collect();
    let content = listing.get("content").and_then(Value::as_str).unwrap_or("");
    let mut at = None;
    for line in content.split('\n') {
        if let Some(rest) = line.strip_prefix("- ") {
            // The longest name the line starts, as one name can start another (`a`, `a:b`).
            let starts = (0..names.len())
                .filter(|&i| {
                    rest.strip_prefix(names[i])
                        .is_some_and(|after| after.is_empty() || after.starts_with(':'))
                })
                .max_by_key(|&i| names[i].len());
            at = starts.or(at);
        }
        if let Some(i) = at {
            sizes[i].1 += chars(line) + 1;
        }
    }
    sizes
}
