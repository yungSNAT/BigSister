//! Tool calls by tool: what their results added to context, their errors, and runs of
//! errors (cascades) and calls repeated whilst they keep failing (retry churn).

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::{ClaudeCalls, EST_NOTE, est_tokens, name_of, result_chars, tool_results, tool_uses};
use crate::prices::{self, Priced};
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};

/// Consecutive erroring results that make a cascade, and erroring results of one call
/// that make retry churn: Token Optimizer's threshold.
const AT_LEAST: u64 = 3;

#[derive(Default, Clone, Copy)]
struct Counts {
    calls: u64,
    result_chars: u64,
    /// The part of `result_chars` a priced model read.
    result_priced: Priced,
    errors: u64,
    error_chars: u64,
    images: u64,
    documents: u64,
}

/// A call whose result is awaited: its tool, as printed, and its input as compact JSON.
struct Call {
    tool: String,
    input: String,
}

/// Tool calls and their results, by agent and tool.
#[derive(Default)]
pub struct Tools {
    claude: ClaudeCalls<Call>,
    /// Erroring results in a row so far in the session.
    run: u64,
    /// By tool and input, the erroring results of that call since it last succeeded.
    failing: HashMap<(String, String), u64>,
    /// Result characters of this file that no model has read yet, with their rows.
    unread: Vec<(Agent, String, u64)>,
    tools: BTreeMap<(Agent, String), Counts>,
    cascades: u64,
    calls_in_cascades: u64,
    retry_churn_calls: u64,
}

impl Tools {
    /// Ends a run of erroring results, counting it when it is a cascade.
    fn end_run(&mut self) {
        if self.run >= AT_LEAST {
            self.cascades += 1;
            self.calls_in_cascades += self.run;
        }
        self.run = 0;
    }

    fn result(&mut self, agent: Agent, call: Call, b: &Value) {
        let content = b.get("content").unwrap_or(&Value::Null);
        let chars = result_chars(content);
        let blocks = |kind: &str| {
            content.as_array().map_or(0, |items| {
                items
                    .iter()
                    .filter(|x| x.get("type").and_then(Value::as_str) == Some(kind))
                    .count() as u64
            })
        };
        let c = self.tools.entry((agent, call.tool.clone())).or_default();
        c.result_chars += chars;
        c.images += blocks("image");
        c.documents += blocks("document");
        let key = (call.tool, call.input);
        if b.get("is_error").and_then(Value::as_bool) == Some(true) {
            c.errors += 1;
            c.error_chars += chars;
            self.run += 1;
            let failed = self.failing.entry(key.clone()).or_default();
            *failed += 1;
            if *failed >= AT_LEAST {
                self.retry_churn_calls += 1;
            }
        } else {
            self.end_run();
            self.failing.remove(&key);
        }
        self.unread.push((agent, key.0, chars));
    }
}

/// The columns of the table, in order.
const COLUMNS: [&str; 11] = [
    "agent",
    "tool",
    "calls",
    "result_chars",
    "result_tokens_est",
    "result_usd_est",
    "unpriced_tokens_est",
    "errors",
    "error_chars",
    "images",
    "documents",
];

/// The section's notes.
fn notes() -> Vec<String> {
    vec![
        "Result chars are the characters of text in each call's result, which the agent sends to the model; errors are results the agent marked as an error, which a hook's denial also is. Web search and fetch rows measure what their results added to context, not what could have been avoided.".to_owned(),
        "Images and documents are counted, not sized: their tokens depend on pixels and pages, which the transcript does not record.".to_owned(),
        "A cascade is a run of 3 or more erroring results in a row within a session, counted once with its calls. Retry churn counts a call whose tool and exact input erred twice before without succeeding in between, from its third erroring result on. A call can be in both. Results outside the window, and copies of an earlier session's records, count nowhere and neither extend nor end a run.".to_owned(),
        EST_NOTE.to_owned(),
        prices::NOTE.to_owned(),
    ]
}

/// A tool or skill name as printed: a name of letters, digits and `_`, `-`, `.` or `:`,
/// up to 128 characters, as tool and skill names are; anything else is `(other)`, so
/// that no other text reaches the output.
pub(super) fn printed(name: &str) -> String {
    let fits = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b));
    if fits { name } else { "(other)" }.to_owned()
}

impl Detector for Tools {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Pi]
    }

    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"tool_use\"") || self.claude.wants(line)
    }

    fn begin(&mut self, _s: &Session) {
        self.end_run();
        self.failing.clear();
        self.claude.clear();
        self.unread.clear();
    }

    fn end(&mut self, _s: &Session) {
        self.end_run();
    }

    fn read_by(&mut self, _s: &Session, usd_per_mtok: Option<f64>) {
        for (agent, tool, n) in self.unread.drain(..) {
            let c = self.tools.entry((agent, tool)).or_default();
            c.result_priced.add(n, usd_per_mtok);
        }
    }

    fn record(&mut self, s: &Session, r: &Record<'_>) {
        for b in tool_uses(r) {
            let id = b.get("id").and_then(Value::as_str);
            let fresh = self.claude.note(id);
            if !r.in_window {
                continue;
            }
            let tool = printed(name_of(b));
            self.tools.entry((s.agent, tool.clone())).or_default().calls += 1;
            if fresh && let Some(id) = id {
                let input = b.get("input").map_or_else(String::new, Value::to_string);
                self.claude.wait(id, Call { tool, input });
            }
        }
        for b in tool_results(r) {
            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
            if let Some(call) = self.claude.take(id)
                && r.in_window
            {
                self.result(s.agent, call, b);
            }
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut rows: Vec<(u64, Vec<Value>)> = self
            .tools
            .iter()
            .map(|((agent, tool), c)| {
                let row = vec![
                    json!(agent.name()),
                    json!(tool),
                    json!(c.calls),
                    json!(c.result_chars),
                    json!(est_tokens(c.result_chars)),
                    json!(c.result_priced.usd()),
                    json!(c.result_priced.unpriced_tokens(c.result_chars)),
                    json!(c.errors),
                    json!(c.error_chars),
                    json!(c.images),
                    json!(c.documents),
                ];
                (c.result_chars, row)
            })
            .collect();
        rows.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
        let mut table = Table::new("By agent and tool, largest results first", COLUMNS.to_vec());
        table.rows = rows.into_iter().map(|(_, r)| r).collect();
        table.truncate(opts.limit);
        Section {
            name: "tools",
            title: "tool calls and their results, by tool",
            summary: vec![
                ("cascades".to_owned(), json!(self.cascades)),
                (
                    "calls_in_cascades".to_owned(),
                    json!(self.calls_in_cascades),
                ),
                (
                    "retry_churn_calls".to_owned(),
                    json!(self.retry_churn_calls),
                ),
            ],
            tables: vec![table],
            notes: notes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::printed;

    #[test]
    fn only_tool_like_names_are_printed() {
        assert_eq!(
            printed("mcp__server__do-it.v2:x"),
            "mcp__server__do-it.v2:x"
        );
        for other in ["", "two words", "a\"b", "é", &"x".repeat(129)] {
            assert_eq!(printed(other), "(other)", "{other}");
        }
    }
}
