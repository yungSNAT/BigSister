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
    ];
    for (d, want) in cases {
        assert_eq!(cause(&d), want, "{d}");
    }
}

#[test]
fn rules_modes_and_the_rest_take_the_causes_after_safety_checks() {
    let cases = [
        (
            json!({"decision": "reject", "source": "config", "reasonType": "classifier"}),
            "auto_mode",
        ),
        (
            json!({"decision": "reject", "source": "config", "reasonType": "rule"}),
            "rule",
        ),
        (
            json!({"decision": "accept", "source": "user_permanent", "ruleSource": "userSettings"}),
            "you",
        ),
        (
            json!({"decision": "reject", "source": "config", "reasonType": "subcommandResults", "ruleSource": "userSettings"}),
            "rule",
        ),
        (
            json!({"decision": "reject", "source": "config", "reasonType": "other", "circuitBreaker": false}),
            "other",
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
        // 2 results with no decision, as older versions wrote, in one record.
        claude_tool_use("e", "Read", &json!({"file_path": "/w/b"})),
        claude_tool_use("f", "Read", &json!({"file_path": "/w/c"})),
        json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "e", "content": "x"},
            {"type": "tool_result", "tool_use_id": "f", "content": "x"}]}}).to_string(),
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
        ("blocked_by_other", 0),
        ("tool_results_without_decision", 2),
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
