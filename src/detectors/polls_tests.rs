use super::*;

#[test]
fn only_results_of_awaited_runs_are_parsed() {
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Polls::default();
    let run = d.observe(&s, "git status", true, true).expect("a run");
    d.claude.wait("toolu_a", run);
    let result = |id: &str| {
        format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}"}}]}}}}"#
        )
    };
    assert!(d.wants(&s, &result("toolu_a")));
    assert!(!d.wants(&s, &result("toolu_b")));
}

const CLAUDE: Session = Session {
    agent: Agent::Claude,
    subagent: false,
};

/// A Claude `tool_use` block running `git status` with id `id`.
fn call(id: &str) -> String {
    format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"Bash","input":{{"command":"git status"}}}}]}}}}"#
    )
}

/// The main-scope totals after one Claude session of `lines`, all in the window.
fn claude_totals(lines: &[String]) -> Counts {
    let mut d = Polls::default();
    d.begin(&CLAUDE);
    for line in lines {
        if d.wants(&CLAUDE, line) {
            let value: Value = serde_json::from_str(line).expect("valid JSON");
            d.record(
                &CLAUDE,
                &Record {
                    value: &value,
                    in_window: true,
                },
            );
        }
    }
    d.end(&CLAUDE);
    d.totals[&(Agent::Claude, "main")]
}

#[test]
fn a_claude_run_whose_result_never_comes_hides_an_earlier_failure() {
    let failure = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_a","is_error":true}]}}"#;
    let c = claude_totals(&[
        call("toolu_a"),
        failure.to_owned(),
        call("toolu_b"),
        call("toolu_c"),
    ]);
    // b follows the failure; c follows b, whose result never came.
    assert_eq!((c.polls, c.polls_after_failure), (2, 1));
}

#[test]
fn a_reused_claude_tool_use_id_pairs_with_neither_run() {
    let result = |is_error: bool| {
        format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_a","is_error":{is_error},"content":"clean"}}]}}}}"#
        )
    };
    // The repeat is a poll, but the one result may be the first run's, so its
    // characters are not the poll's.
    let c = claude_totals(&[call("toolu_a"), call("toolu_a"), result(false)]);
    assert_eq!((c.polls, c.poll_chars), (1, 0));
    // Nor does a failure read for the reused id mark the next run.
    let c = claude_totals(&[
        call("toolu_a"),
        call("toolu_a"),
        result(true),
        call("toolu_b"),
    ]);
    assert_eq!((c.polls, c.polls_after_failure), (2, 0));
}

#[test]
fn command_results_count_every_run_in_the_window_by_verb_class() {
    let bash = |id: &str, cmd: &str| {
        crate::detectors::claude_tool_use(id, "Bash", &json!({"command": cmd}))
    };
    let result = crate::detectors::claude_tool_result;
    let lines = [
        (false, bash("toolu_c", "cargo build")),
        (false, result("toolu_c", "1234567")),
        (true, bash("toolu_a", "git status")),
        (true, result("toolu_a", "0123456789")),
        (true, bash("toolu_b", "git status")),
        (true, result("toolu_b", "abcde")),
        (true, bash("toolu_d", "ls")),
    ];
    let mut d = Polls::default();
    crate::detectors::replay(&mut d, &CLAUDE, &lines);
    let r = |verb: &str| {
        d.results
            .get(&(Agent::Claude, verb.to_owned()))
            .map(|r| (r.commands, r.results, r.chars))
    };
    // A run before the window counts nowhere; a run whose result never comes counts
    // as a command only.
    assert_eq!(r("git"), Some((2, 2, 15)));
    assert_eq!(r("ls"), Some((1, 0, 0)));
    assert_eq!(r("cargo"), None);
}
