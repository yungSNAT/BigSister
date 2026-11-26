//! Pairing each Codex command with its result.

use std::collections::HashSet;

use serde_json::Value;

use super::codex::function_command;
use super::{CodexCall, CodexOutput, CommandResult, chars, json_escaped};
use crate::scan::Record;

/// A command's result as a detector reads it.
#[derive(Debug)]
pub struct Paired {
    pub exit_code: i64,
    /// What the command wrote, as Codex recorded it or the cell printed it.
    pub output: String,
    /// Characters of the output the cell printed for this command, which reached the
    /// model; 0 when it printed none.
    pub printed_chars: u64,
    /// Whether the model-visible tool response for this command was truncated or stashed.
    pub visible_truncated: bool,
}

/// A `CommandExecution` item kept for the one open call that could have run it.
struct Item {
    key: String,
    result: CommandResult,
}

/// A Codex call whose output has not come.
struct Open<T> {
    id: String,
    /// `id` as JSON writes it, to find its output's line.
    needle: String,
    /// When the call was recorded, in milliseconds since the epoch.
    start: Option<i64>,
    /// Its place amongst the session's calls.
    seq: u64,
    /// Each command's text as `command_key` gives it.
    keys: Vec<String>,
    /// The items whose text no other call's command could have.
    items: Vec<Item>,
    /// Texts that an item could have from this call or another, or that an item
    /// without an exit code had.
    ambiguous: Vec<String>,
    /// Whether Codex recorded any `CommandExecution` item whilst the call was open.
    saw_items: bool,
    /// While a detector awaits the output: what it keeps for each command, and whether
    /// the call was in the window.
    runs: Option<(Vec<T>, bool)>,
}

/// What a call left when its output came.
struct Closed<T> {
    runs: Option<(Vec<T>, bool)>,
    /// Each command's result from its items, if tied.
    ties: Vec<Option<CommandResult>>,
    saw_items: bool,
}

/// A command not tied to an item when its call's output came, which may still be
/// running and so end whilst a call made before that output is open.
struct Lingering {
    key: String,
    start: Option<i64>,
    /// The `seq` of the last call made before the output came.
    closed_at: u64,
}

/// Codex calls whose output a detector awaits, each with what the detector keeps for
/// each of its commands, and how many in the window, and how many of their commands,
/// could not be paired with their results: calls without an id, calls whose id
/// another call in the session also carries, `function_call` calls, whose output is
/// not read, calls whose output never came, and commands neither tied to an item nor
/// paired by position.
///
/// A command ties to a `CommandExecution` item Codex records as a command ends, which
/// carries no call id, when the item's command reads as the command's text, it ended
/// before the call's output, the call's record has a time no later than the item's
/// start, and no other command could have run it: no command of another open call,
/// including a call without an id or time, that could have started first has that
/// text, nor does an untied command of a call whose output came whilst one of those
/// was open; and the call has no other command with that text and no other such item.
/// A command not tied pairs with the result the cell printed at its position when the
/// cell printed one result per command and has one command, or Codex recorded no item
/// whilst the call was open, as older Codex versions record none, or it is the one
/// command left untied, every tied command's result is printed whole in its own place
/// and the result in its place is no tied command's item cut to its end (`in_order`).
pub struct CodexCalls<T> {
    /// The ids of the session's calls so far.
    seen: HashSet<String>,
    open: Vec<Open<T>>,
    lingering: Vec<Lingering>,
    /// Calls made so far in the session.
    seq: u64,
    pub unpaired: u64,
    /// The commands of the `unpaired` calls.
    pub unpaired_commands: u64,
}

impl<T> Default for CodexCalls<T> {
    fn default() -> Self {
        Self {
            seen: HashSet::new(),
            open: Vec::new(),
            lingering: Vec::new(),
            seq: 0,
            unpaired: 0,
            unpaired_commands: 0,
        }
    }
}

impl<T> CodexCalls<T> {
    /// Notes a call, which every detector reading outputs must do for every call so
    /// that a reused id and every running command are seen. `runs` holds what the
    /// detector keeps for each command, by position, or is `None` when it awaits none
    /// of the output. An id carried by more than one call pairs with neither from then
    /// on, as their outputs cannot be told apart.
    pub fn call(&mut self, r: &Record<'_>, call: &CodexCall, runs: Option<Vec<T>>) {
        let id = call.call_id.as_str();
        let fresh = !id.is_empty() && self.seen.insert(id.to_owned());
        if !fresh {
            let earlier: Vec<(Vec<T>, bool)> = self
                .open
                .iter_mut()
                .filter(|o| o.id == id)
                .filter_map(|o| o.runs.take())
                .collect();
            for (earlier, w) in earlier {
                self.not_paired(earlier.len(), w);
            }
        }
        let awaited = call.exec && fresh;
        let runs = match runs {
            Some(runs) if !awaited => {
                self.not_paired(runs.len(), r.in_window);
                None
            }
            runs => runs.map(|runs| (runs, r.in_window)),
        };
        if runs.is_none() && call.commands().next().is_none() {
            return;
        }
        // A call without an id stays open for the rest of the session, as its output
        // cannot be found, so that no other call takes an item it could have run.
        self.seq += 1;
        self.open.push(Open {
            id: id.to_owned(),
            needle: json_escaped(id),
            start: record_ms(r),
            seq: self.seq,
            keys: call.commands().map(command_key).collect(),
            items: Vec::new(),
            ambiguous: Vec::new(),
            saw_items: false,
            runs,
        });
    }

    fn not_paired(&mut self, commands: usize, in_window: bool) {
        if in_window {
            self.unpaired += 1;
            self.unpaired_commands += commands as u64;
        }
    }

    /// True for lines that may hold an open call's output or a command's item.
    #[must_use]
    pub fn wants(&self, line: &str) -> bool {
        if line.contains("\"custom_tool_call_output\"") || line.contains("\"function_call_output\"")
        {
            self.open
                .iter()
                .any(|o| !o.id.is_empty() && line.contains(o.needle.as_str()))
        } else {
            // An item matters only to the commands of an awaited call.
            line.contains("\"CommandExecution\"")
                && self
                    .open
                    .iter()
                    .any(|o| o.runs.as_ref().is_some_and(|(runs, _)| !runs.is_empty()))
        }
    }

    /// Reads a `CommandExecution` item or a `function_call`'s output, and says whether
    /// the record was one.
    pub fn record(&mut self, r: &Record<'_>) -> bool {
        let Some(p) = r.value.get("payload") else {
            return false;
        };
        match (r.kind(), p.get("type").and_then(Value::as_str)) {
            ("response_item", Some("function_call_output")) => {
                if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    self.close(id);
                }
                true
            }
            ("event_msg", Some("item_completed"))
                if p.pointer("/item/type").and_then(Value::as_str) == Some("CommandExecution") =>
            {
                self.item(p);
                true
            }
            _ => false,
        }
    }

    fn item(&mut self, p: &Value) {
        for o in &mut self.open {
            o.saw_items = true;
        }
        let item = &p["item"];
        let Some(key) = item.get("command").and_then(function_command) else {
            return;
        };
        let key = command_key(&key);
        let started = p.get("started_at_ms").and_then(Value::as_i64);
        // A command could have run the item unless it is known to have started later;
        // only a call whose start is known can take it.
        let could = |start: Option<i64>| start.zip(started).is_none_or(|(s, t)| s <= t);
        let lingers = self
            .lingering
            .iter()
            .any(|l| l.key == key && could(l.start));
        let calls: Vec<usize> = (0..self.open.len())
            .filter(|&i| could(self.open[i].start) && self.open[i].keys.contains(&key))
            .collect();
        match (calls.as_slice(), lingers, started.zip(item_result(item))) {
            ([i], false, Some((_, result))) if self.open[*i].start.is_some() => {
                self.open[*i].items.push(Item { key, result });
            }
            _ => {
                for i in calls {
                    self.open[i].ambiguous.push(key.clone());
                }
            }
        }
    }

    /// Ends the calls with id `id`, and gives the last. Their untied commands linger
    /// whilst a call made before them is open.
    fn close(&mut self, id: &str) -> Option<Closed<T>> {
        if id.is_empty() {
            return None;
        }
        let mut last = None;
        while let Some(i) = self.open.iter().position(|o| o.id == id) {
            let Open {
                start,
                keys,
                items,
                ambiguous,
                saw_items,
                runs,
                ..
            } = self.open.remove(i);
            let ties = ties(&keys, items, &ambiguous);
            if !self.open.is_empty() {
                for (key, tie) in keys.into_iter().zip(&ties) {
                    if tie.is_none() {
                        self.lingering.push(Lingering {
                            key,
                            start,
                            closed_at: self.seq,
                        });
                    }
                }
            }
            last = Some(Closed {
                runs,
                ties,
                saw_items,
            });
        }
        let open = &self.open;
        self.lingering
            .retain(|l| open.iter().any(|o| o.seq <= l.closed_at));
        last
    }

    /// Whether the output's call was awaited; it is awaited no longer.
    pub fn take(&mut self, out: &CodexOutput) -> bool {
        self.close(&out.call_id).is_some_and(|c| c.runs.is_some())
    }

    /// For an awaited call's output: what the detector kept for each command that
    /// could be paired, with that command's result: from its item if tied, otherwise
    /// the result printed at its position where that is allowed (`CodexCalls`).
    pub fn pair(&mut self, out: &CodexOutput) -> Vec<(T, Paired)> {
        let Some(Closed {
            runs: Some((runs, in_window)),
            ties,
            saw_items,
        }) = self.close(&out.call_id)
        else {
            return Vec::new();
        };
        let n = runs.len();
        // With every other command tied to the result printed whole in its own place,
        // the one result left is the untied command's.
        let by_position =
            out.results.len() == n && (n == 1 || !saw_items || in_order(&ties, &out.results));
        let shown = printed_for(&out.results, &ties, by_position);
        let mut missing = 0;
        let paired = runs
            .into_iter()
            .zip(ties)
            .enumerate()
            .filter_map(|(k, (run, tie))| {
                let p = match tie {
                    Some(item) => Some(pair_tied(k, n, item, &shown, out)),
                    None if by_position => out.results.get(k).map(|x| Paired {
                        exit_code: x.exit_code,
                        output: x.output.clone(),
                        printed_chars: chars(&x.output),
                        visible_truncated: truncation::text_looks_truncated(&x.output)
                            || (n == 1 && out.visible_truncated),
                    }),
                    None => None,
                };
                missing += usize::from(p.is_none());
                p.map(|p| (run, p))
            })
            .collect();
        if missing > 0 {
            self.not_paired(missing, in_window);
        }
        paired
    }

    /// Ends a session: the calls still awaiting output never get it.
    pub fn end(&mut self) {
        for o in std::mem::take(&mut self.open) {
            if let Some((runs, in_window)) = o.runs {
                self.not_paired(runs.len(), in_window);
            }
        }
        self.lingering.clear();
        self.seen.clear();
        self.seq = 0;
    }
}

/// Builds a [`Paired`] entry for a command tied to a `CommandExecution` item, keeping
/// the archived item output for error analysis whilst judging truncation from the
/// model-visible tool response.
fn pair_tied(
    k: usize,
    n: usize,
    item: CommandResult,
    shown: &[Option<&CommandResult>],
    out: &CodexOutput,
) -> Paired {
    let printed = shown[k];
    let from_result = match printed {
        Some(x) => {
            truncation::text_looks_truncated(&x.output)
                || (!item.output.is_empty() && x.output.len() < item.output.len())
        }
        None => visible_unmatched_truncated(k, n, shown, out),
    };
    Paired {
        printed_chars: printed.map_or(0, |x| chars(&x.output)),
        exit_code: item.exit_code,
        output: item.output,
        visible_truncated: from_result || (n == 1 && out.visible_truncated),
    }
}

fn visible_unmatched_truncated(
    k: usize,
    n: usize,
    shown: &[Option<&CommandResult>],
    out: &CodexOutput,
) -> bool {
    let in_place = out.results.len() == n
        && (n == 1
            || (0..n)
                .all(|j| j == k || shown[j].is_some_and(|r| std::ptr::eq(r, &out.results[j]))));
    in_place && truncation::text_looks_truncated(&out.results[k].output)
}

/// Each command's result from a call's items: the one item with its text, when the
/// text is not `ambiguous` and no other command of the call has it, as code need not
/// run commands in the order it writes them.
fn ties(keys: &[String], items: Vec<Item>, ambiguous: &[String]) -> Vec<Option<CommandResult>> {
    let once = |key: &String| {
        !ambiguous.contains(key)
            && keys.iter().filter(|k| *k == key).count() == 1
            && items.iter().filter(|i| i.key == *key).count() == 1
    };
    let fits: Vec<bool> = keys.iter().map(once).collect();
    let mut items: Vec<Option<Item>> = items.into_iter().map(Some).collect();
    keys.iter()
        .zip(fits)
        .map(|(key, fits)| {
            let item = items
                .iter_mut()
                .find(|i| fits && i.as_ref().is_some_and(|i| i.key == *key))?;
            item.take().map(|i| i.result)
        })
        .collect()
}

/// Whether the one command `ties` leaves untied can take the result printed in its
/// place: every tied command's result is printed whole in its own place, and the one
/// left is no tied command's item cut to its end. Another pairing that gave the untied
/// command another result would be a chain of tied commands, each taking a result that
/// shows its item, the last taking the untied command's; a link that is not whole makes
/// that result a tied item cut to its end, and with every link whole the untied
/// command's result is the same either way.
fn in_order(ties: &[Option<CommandResult>], printed: &[CommandResult]) -> bool {
    let mut untied = (0..ties.len()).filter(|&k| ties[k].is_none());
    let (Some(u), None) = (untied.next(), untied.next()) else {
        return false;
    };
    let tied = || ties.iter().flatten();
    ties.iter().enumerate().all(|(k, tie)| {
        tie.as_ref().is_none_or(|item| {
            printed
                .get(k)
                .is_some_and(|x| x.exit_code == item.exit_code && x.output == item.output)
        })
    }) && printed
        .get(u)
        .is_some_and(|x| !tied().any(|item| shows(x, item) && x.output != item.output))
}

/// Whether printed result `x` shows the item's: the same exit code, and the item's
/// output or, as `exec_command` keeps the end of a long output, its end.
fn shows(x: &CommandResult, item: &CommandResult) -> bool {
    x.exit_code == item.exit_code
        && (x.output == item.output || (!x.output.is_empty() && item.output.ends_with(&x.output)))
}

/// For each tied command, the result the cell printed of it: one that shows its item
/// and no other tied command's, preferring its whole output. A printed result that
/// could be more than one command's counts for none. Where a command is untied, which
/// could own any printed result, a tied command has only the result in its own place,
/// and only when the cell's results pair `by_position`.
fn printed_for<'a>(
    printed: &'a [CommandResult],
    ties: &[Option<CommandResult>],
    by_position: bool,
) -> Vec<Option<&'a CommandResult>> {
    if ties.iter().any(Option::is_none) {
        return (0..ties.len())
            .map(|k| printed.get(k).filter(|_| by_position && ties[k].is_some()))
            .collect();
    }
    let owners: Vec<Option<usize>> = printed
        .iter()
        .map(|x| {
            let mut can =
                (0..ties.len()).filter(|&k| ties[k].as_ref().is_some_and(|item| shows(x, item)));
            match (can.next(), can.next()) {
                (Some(k), None) => Some(k),
                _ => None,
            }
        })
        .collect();
    ties.iter()
        .enumerate()
        .map(|(k, tie)| {
            let item = tie.as_ref()?;
            let mine = || {
                printed
                    .iter()
                    .zip(&owners)
                    .filter(move |(_, o)| **o == Some(k))
                    .map(|(x, _)| x)
            };
            mine()
                .find(|x| x.output == item.output)
                .or_else(|| mine().next())
        })
        .collect()
}

/// An item's exit code and output; a missing output is empty.
pub(super) fn item_result(item: &Value) -> Option<CommandResult> {
    Some(CommandResult {
        exit_code: item.get("exit_code")?.as_i64()?,
        output: match item.get("aggregated_output") {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => String::new(),
            Some(_) => return None,
        },
    })
}

/// When a record was written, in milliseconds since the epoch.
fn record_ms(r: &Record<'_>) -> Option<i64> {
    let t = r.value.get("timestamp")?.as_str()?;
    Some(crate::time::parse_ts(t)?.as_millisecond())
}

/// A command's text as it is matched with Codex's record of what ran: trimmed, and
/// without `rtk ` where a simple command starts.
///
/// shortcut: the RTK hook rewrites the commands Codex runs by putting `rtk ` before
/// each program it filters (38,599 of 104,470 items on one PC, 15 November 2026), so
/// both texts drop the word, and 2 commands of a call that then read alike tie to
/// neither.
/// Commands another hook rewrites stay untied; telling rewrites apart would need the
/// hooks' own records.
fn command_key(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut rest = cmd.trim();
    while let Some(c) = rest.chars().next() {
        let starts = out
            .chars()
            .next_back()
            .is_none_or(|p| p.is_whitespace() || ";&|(".contains(p));
        if starts && let Some(after) = rest.strip_prefix("rtk ") {
            rest = after;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}
