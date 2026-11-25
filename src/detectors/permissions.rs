//! Permission decisions Claude Code records on tool results: what was allowed and
//! blocked, by cause and by tool.
//!
//! Since 2.1.299 or earlier, Claude Code writes a `permissionDecision` object beside the
//! `tool_result` block of a `user` record: `decision` (`accept` or `reject`), `source`,
//! and optionally `reasonType`, `reasonCode`, `ruleSource`, `classifierCategory` and
//! `circuitBreaker`. `reasonCode` can hold the text of a rule, so it is never read.
//! Every other field prints only as one of the values below, else `(other)`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::tools::printed;
use super::{ClaudeCalls, name_of, tool_results, tool_uses};
use crate::report::{Opts, Section, Table};
use crate::scan::{Agent, Detector, Record, Session};

/// Values of each field that print as themselves; any other prints as `(other)`.
const SOURCES: &[&str] = &[
    "config",
    "hook",
    "user_permanent",
    "user_temporary",
    "user_reject",
];
const REASON_TYPES: &[&str] = &[
    "mode",
    "classifier",
    "hook",
    "rule",
    "safetyCheck",
    "subcommandResults",
    "other",
];
const RULE_SOURCES: &[&str] = &[
    "userSettings",
    "projectSettings",
    "localSettings",
    "policySettings",
    "flagSettings",
    "cliArg",
    "command",
    "session",
    "mcpServerPolicy",
    "mcpMount",
];
const CATEGORIES: &[&str] = &[
    "security_weaken",
    "permission_grant",
    "external_system_writes",
    "bot_check_circumvention",
    "irreversible_local_destruction",
];

/// Causes, in the order of precedence a decision takes the first that applies.
const CAUSES: [&str; 7] = [
    "you",
    "hook",
    "safety_check",
    "auto_mode",
    "rule",
    "mode",
    "other",
];

/// A field of `d` as printed: `(none)` when absent, itself when `allowed` lists it,
/// else `(other)`.
fn field(d: &Value, key: &str, allowed: &[&'static str]) -> &'static str {
    match d.get(key) {
        None | Some(Value::Null) => "(none)",
        Some(v) => v
            .as_str()
            .and_then(|s| allowed.iter().find(|a| **a == s))
            .copied()
            .unwrap_or("(other)"),
    }
}

/// `allowed`, `blocked` or `(other)`.
fn decision(d: &Value) -> &'static str {
    match d.get("decision").and_then(Value::as_str) {
        Some("accept") => "allowed",
        Some("reject") => "blocked",
        _ => "(other)",
    }
}

/// The one cause of a decision, the first of [`CAUSES`] that applies: `you` when you
/// answered a prompt for it, `hook` when a hook decided, `safety_check` for Claude Code's
/// own safety check, `auto_mode` for auto mode's classifier, `rule` for a permission rule
/// (one you saved by answering 'always' included), `mode` for the permission mode, else
/// `other`.
fn cause(d: &Value) -> &'static str {
    let source = d.get("source").and_then(Value::as_str).unwrap_or("");
    let reason = d.get("reasonType").and_then(Value::as_str).unwrap_or("");
    if matches!(source, "user_temporary" | "user_reject") {
        "you"
    } else if source == "hook" || reason == "hook" {
        "hook"
    } else if reason == "safetyCheck" || d.get("circuitBreaker").is_some_and(|v| !v.is_null()) {
        "safety_check"
    } else if reason == "classifier" {
        "auto_mode"
    } else if reason == "rule" || source == "user_permanent" {
        "rule"
    } else if reason == "mode" {
        "mode"
    } else {
        "other"
    }
}

#[derive(Default)]
struct ToolCounts {
    decisions: u64,
    allowed: u64,
    blocked: u64,
    answered_by_you: u64,
}

/// Permission decisions on Claude tool results.
#[derive(Default)]
pub struct Permissions {
    calls: ClaudeCalls<String>,
    by_cause: BTreeMap<(&'static str, &'static str), u64>,
    raw: BTreeMap<[&'static str; 4], u64>,
    tools: BTreeMap<String, ToolCounts>,
    categories: BTreeMap<&'static str, u64>,
    results_without: u64,
}

impl Permissions {
    fn count(&mut self, d: &Value, tool: String) {
        let decided = decision(d);
        let why = cause(d);
        *self.by_cause.entry((decided, why)).or_default() += 1;
        let raw = [
            decided,
            field(d, "source", SOURCES),
            field(d, "reasonType", REASON_TYPES),
            field(d, "ruleSource", RULE_SOURCES),
        ];
        *self.raw.entry(raw).or_default() += 1;
        if decided == "blocked" && why == "auto_mode" {
            *self
                .categories
                .entry(field(d, "classifierCategory", CATEGORIES))
                .or_default() += 1;
        }
        let t = self.tools.entry(tool).or_default();
        t.decisions += 1;
        match decided {
            "allowed" => t.allowed += 1,
            "blocked" => t.blocked += 1,
            _ => {}
        }
        if why == "you" {
            t.answered_by_you += 1;
        }
    }

    fn total(&self, decided: &str) -> u64 {
        self.by_cause
            .iter()
            .filter(|((d, _), _)| *d == decided)
            .map(|(_, n)| n)
            .sum()
    }
}

impl Detector for Permissions {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }

    fn wants(&self, s: &Session, line: &str) -> bool {
        s.agent == Agent::Claude
            && (line.contains("\"tool_use\"") || line.contains("\"tool_result\""))
    }

    fn scans_whole_file(&self) -> bool {
        true
    }

    fn prefilter(&self) -> Option<crate::scan::Prefilter> {
        Some(crate::scan::Prefilter::Superset(|_, line| {
            line.contains("\"tool_use\"") || line.contains("\"tool_result\"")
        }))
    }

    fn begin(&mut self, _s: &Session) {
        self.calls.clear();
    }

    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        for b in tool_uses(r) {
            let id = b.get("id").and_then(Value::as_str);
            if self.calls.note(id)
                && let Some(id) = id
            {
                self.calls.wait(id, printed(name_of(b)));
            }
        }
        // A record carries one decision; it answers its first tool result.
        let first = tool_results(r).next().map(|b| {
            let id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
            self.calls.take(id)
        });
        let Some(tool) = first else {
            return;
        };
        if !r.in_window {
            return;
        }
        match r.value.get("permissionDecision").filter(|d| d.is_object()) {
            Some(d) => self.count(d, tool.unwrap_or_else(|| "(unpaired)".to_owned())),
            None => self.results_without += 1,
        }
    }

    fn section(&self, opts: &Opts) -> Section {
        let mut tables = vec![
            self.cause_table(),
            self.field_table(),
            self.tool_table(),
            self.category_table(),
        ];
        for t in &mut tables {
            t.truncate(opts.limit);
        }
        Section {
            name: "permissions",
            title: "permission decisions on tool calls, allowed and blocked, by cause (Claude)",
            summary: self.summary(),
            tables,
            notes: notes(),
        }
    }
}

impl Permissions {
    fn summary(&self) -> Vec<(String, Value)> {
        let answered: u64 = self
            .by_cause
            .iter()
            .filter(|((_, c), _)| *c == "you")
            .map(|(_, n)| n)
            .sum();
        let mut summary = vec![
            (
                "decisions".to_owned(),
                json!(self.by_cause.values().sum::<u64>()),
            ),
            ("allowed".to_owned(), json!(self.total("allowed"))),
            ("blocked".to_owned(), json!(self.total("blocked"))),
            ("answered_by_you".to_owned(), json!(answered)),
        ];
        for c in CAUSES {
            let n = self.by_cause.get(&("blocked", c)).copied().unwrap_or(0);
            let key = if c == "other" {
                "blocked_other".to_owned()
            } else {
                format!("blocked_by_{c}")
            };
            summary.push((key, json!(n)));
        }
        summary.push((
            "tool_results_without_decision".to_owned(),
            json!(self.results_without),
        ));
        summary
    }

    fn cause_table(&self) -> Table {
        let mut t = Table::new(
            "By decision and cause",
            vec!["decision", "cause", "decisions"],
        );
        for d in ["blocked", "allowed", "(other)"] {
            for c in CAUSES {
                if let Some(n) = self.by_cause.get(&(d, c)) {
                    t.rows.push(vec![json!(d), json!(c), json!(n)]);
                }
            }
        }
        t
    }

    fn field_table(&self) -> Table {
        let mut raw: Vec<(&[&str; 4], &u64)> = self.raw.iter().collect();
        raw.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        let mut t = Table::new(
            "By recorded fields, most first",
            vec![
                "decision",
                "source",
                "reason_type",
                "rule_source",
                "decisions",
            ],
        );
        t.rows = raw
            .into_iter()
            .map(|(k, n)| vec![json!(k[0]), json!(k[1]), json!(k[2]), json!(k[3]), json!(n)])
            .collect();
        t
    }

    fn tool_table(&self) -> Table {
        let mut tools: Vec<(&String, &ToolCounts)> = self.tools.iter().collect();
        tools.sort_by_key(|(_, t)| std::cmp::Reverse((t.blocked, t.decisions)));
        let mut t = Table::new(
            "By tool, most blocked first",
            vec!["tool", "decisions", "allowed", "blocked", "answered_by_you"],
        );
        t.rows = tools
            .into_iter()
            .map(|(name, c)| {
                vec![
                    json!(name),
                    json!(c.decisions),
                    json!(c.allowed),
                    json!(c.blocked),
                    json!(c.answered_by_you),
                ]
            })
            .collect();
        t
    }

    fn category_table(&self) -> Table {
        let mut cats: Vec<(&&str, &u64)> = self.categories.iter().collect();
        cats.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        let mut t = Table::new(
            "Auto mode blocks by category",
            vec!["classifier_category", "blocks"],
        );
        t.rows = cats
            .into_iter()
            .map(|(c, n)| vec![json!(c), json!(n)])
            .collect();
        t
    }
}

fn notes() -> Vec<String> {
    vec![
        "Claude Code records a permission decision on each tool result, from about version 2.1.299: tool results without decision counts the results in the window that carry none, as older versions wrote, so a 0 there means every tool result in the window carried one.".to_owned(),
        "Each decision has one cause, the first that applies of: you (you answered a prompt for it), hook, safety_check (Claude Code's own safety check), auto_mode (auto mode's classifier), rule (a permission rule, including one you saved by answering 'always'), mode (the permission mode, such as bypassPermissions or dontAsk) and other. The recorded fields table gives the values each decision recorded, which the causes are read from.".to_owned(),
        "The rule's text and reason code are never printed. A recorded value not known to bigsister prints as (other); (none) means the field was not recorded. A tool is (unpaired) when the call the result answers was not read.".to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detectors::{claude_tool_use, replay};

    fn result(id: &str, decision: &Value) -> String {
        json!({"type": "user", "permissionDecision": decision, "message": {"content": [
            {"type": "tool_result", "tool_use_id": id, "content": "x"}
        ]}})
        .to_string()
    }

    fn run(lines: &[(bool, String)]) -> Section {
        let s = Session {
            agent: Agent::Claude,
            subagent: false,
        };
        let mut d = Permissions::default();
        replay(&mut d, &s, lines);
        d.section(&Opts {
            limit: 100,
            paths: false,
            by_branch: false,
            home: None,
            usd_per_mtok: None,
            prices_file: None,
        })
    }

    fn summary(s: &Section, key: &str) -> Value {
        s.summary
            .iter()
            .find(|(k, _)| k == key)
            .map_or(Value::Null, |(_, v)| v.clone())
    }

    #[test]
    fn each_decision_takes_the_first_cause_that_applies() {
        let cases = [
            (
                json!({"decision": "reject", "source": "user_reject", "reasonType": "rule"}),
                "you",
            ),
            (
                json!({"decision": "reject", "source": "hook", "reasonType": "hook"}),
                "hook",
            ),
            (
                json!({"decision": "reject", "source": "config", "reasonType": "safetyCheck", "circuitBreaker": "x"}),
                "safety_check",
            ),
            (
                json!({"decision": "reject", "source": "config", "reasonType": "other", "circuitBreaker": "x"}),
                "safety_check",
            ),
            (
                json!({"decision": "reject", "source": "config", "reasonType": "classifier"}),
                "auto_mode",
            ),
            (
                json!({"decision": "reject", "source": "config", "reasonType": "rule"}),
                "rule",
            ),
            (
                json!({"decision": "accept", "source": "user_permanent"}),
                "rule",
            ),
            (
                json!({"decision": "accept", "source": "config", "reasonType": "mode"}),
                "mode",
            ),
            (
                json!({"decision": "accept", "source": "config", "reasonType": "subcommandResults"}),
                "other",
            ),
            (
                json!({"decision": "accept", "source": "user_temporary", "reasonType": "hook"}),
                "you",
            ),
        ];
        for (d, want) in cases {
            assert_eq!(cause(&d), want, "{d}");
        }
    }

    #[test]
    fn decisions_count_by_cause_and_tool_and_sum_to_their_totals() {
        let lines: Vec<(bool, String)> = [
            claude_tool_use("a", "Bash", &json!({"command": "rm -rf /"})),
            result("a", &json!({"decision": "reject", "source": "config", "reasonType": "classifier", "classifierCategory": "irreversible_local_destruction"})),
            claude_tool_use("b", "Bash", &json!({"command": "ls"})),
            result("b", &json!({"decision": "reject", "source": "hook", "reasonType": "hook", "reasonCode": "hook:PreToolUse"})),
            claude_tool_use("c", "Read", &json!({"file_path": "/w/a"})),
            result("c", &json!({"decision": "accept", "source": "config", "reasonType": "mode"})),
            claude_tool_use("d", "Edit", &json!({"file_path": "/w/a"})),
            result("d", &json!({"decision": "accept", "source": "user_temporary", "grantScope": "once"})),
            // A result with no decision, as older versions wrote.
            claude_tool_use("e", "Read", &json!({"file_path": "/w/b"})),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "e", "content": "x"}]}}).to_string(),
        ]
        .into_iter()
        .map(|l| (true, l))
        .collect();
        let s = run(&lines);
        for (k, v) in [
            ("decisions", 4),
            ("allowed", 2),
            ("blocked", 2),
            ("answered_by_you", 1),
            ("blocked_by_auto_mode", 1),
            ("blocked_by_hook", 1),
            ("blocked_by_you", 0),
            ("blocked_other", 0),
            ("tool_results_without_decision", 1),
        ] {
            assert_eq!(summary(&s, k), json!(v), "{k}");
        }
        let blocked_causes = s.summary.iter().filter(|(k, _)| k.starts_with("blocked_"));
        assert_eq!(
            blocked_causes.filter_map(|(_, v)| v.as_u64()).sum::<u64>(),
            2
        );
        assert_eq!(
            json!(s.tables[2].rows),
            json!([
                ["Bash", 2, 0, 2, 0],
                ["Edit", 1, 1, 0, 1],
                ["Read", 1, 1, 0, 0]
            ])
        );
        assert_eq!(
            json!(s.tables[3].rows),
            json!([["irreversible_local_destruction", 1]])
        );
    }

    #[test]
    fn only_known_values_print_and_the_reason_code_never_does() {
        let secret = "Bash_git_push_secret-token";
        let lines: Vec<(bool, String)> = [
            claude_tool_use("a", "Bash", &json!({"command": "x"})),
            result(
                "a",
                &json!({"decision": secret, "source": secret, "reasonType": secret,
                                "ruleSource": secret, "reasonCode": secret,
                                "classifierCategory": secret}),
            ),
            claude_tool_use("b", "Bash", &json!({"command": "x"})),
            result(
                "b",
                &json!({"decision": "reject", "source": "config", "reasonType": "classifier",
                                "ruleSource": secret, "reasonCode": secret,
                                "classifierCategory": secret}),
            ),
        ]
        .into_iter()
        .map(|l| (true, l))
        .collect();
        let s = run(&lines);
        let text = format!("{:?}{:?}{:?}", s.summary, s.tables, s.notes);
        assert!(!text.contains(secret), "{text}");
        assert_eq!(
            json!(s.tables[1].rows),
            json!([
                ["(other)", "(other)", "(other)", "(other)", 1],
                ["blocked", "config", "classifier", "(other)", 1]
            ])
        );
        assert_eq!(json!(s.tables[3].rows), json!([["(other)", 1]]));
    }

    #[test]
    fn decisions_outside_the_window_count_nowhere() {
        let lines = vec![
            (
                false,
                claude_tool_use("a", "Bash", &json!({"command": "x"})),
            ),
            (
                false,
                result("a", &json!({"decision": "reject", "source": "hook"})),
            ),
        ];
        let s = run(&lines);
        assert_eq!(summary(&s, "decisions"), json!(0));
        assert_eq!(summary(&s, "tool_results_without_decision"), json!(0));
    }
}
