//! Codex `exec_command` items tied to the commands of the cell that ran them.

use serde_json::{Value, json};

use super::codex_tests::{
    commands, counts, exec, function_call, in_window, not_paired, output, polls, printed, replay,
    summary,
};
use super::pipes::Pipes;

/// A time, in milliseconds after the epoch, for records that carry one.
const T: i64 = 1_762_560_000_000;

/// An `exec` call recorded at `ms`.
fn exec_at(id: &str, input: &str, ms: i64) -> String {
    let mut v: Value = serde_json::from_str(&exec(id, input)).expect("record");
    let at = jiff::Timestamp::from_millisecond(ms).expect("time");
    v["timestamp"] = json!(format!("{at:.3}"));
    v.to_string()
}

/// The `CommandExecution` item Codex records when a command started at `ms` ends.
fn item(args: &[&str], code: i64, out: &str, ms: i64) -> String {
    json!({"type": "event_msg", "payload": {
        "type": "item_completed", "started_at_ms": ms, "completed_at_ms": ms + 5,
        "item": {"type": "CommandExecution", "command": args, "exit_code": code,
            "aggregated_output": out, "status": "completed"}
    }})
    .to_string()
}

/// The item for `cmd` run by `bash -lc`.
fn ran(cmd: &str, code: i64, out: &str, ms: i64) -> String {
    item(&["bash", "-lc", cmd], code, out, ms)
}

/// An `exec` call's output that prints no result.
fn done(id: &str) -> String {
    printed(id, &[json!("done")])
}

#[test]
fn items_credit_results_printed_in_completion_order_to_their_commands() {
    // git diff ends first and the cell prints results as they end.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff"]), T),
        ran("git diff", 1, "d", T + 1),
        ran("git status", 0, "s", T + 2),
        output("a", &[(1, json!("d")), (0, json!("s"))]),
        exec_at("b", &commands(&["git diff"]), T + 10),
        ran("git diff", 1, "d", T + 11),
        output("b", &[(1, json!("d"))]),
    ]);
    // git diff's poll follows its failure.
    assert_eq!(polls(&lines), (counts(1, 1, 1), json!(0)));
    assert_eq!(not_paired(&lines), (json!(0), json!(0)));
}

#[test]
fn a_cell_that_prints_no_result_pairs_its_commands_by_their_items() {
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff"]), T),
        ran("git status", 1, "e", T + 1),
        ran("git diff", 0, "x", T + 2),
        done("a"),
        exec_at("b", &commands(&["git status"]), T + 10),
        ran("git status", 0, "ok", T + 11),
        output("b", &[(0, json!("ok"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 1, 2), json!(0)));
    assert_eq!(not_paired(&lines), (json!(0), json!(0)));
}

#[test]
fn an_item_started_before_the_cell_ties_to_none_of_its_commands() {
    // A command an earlier cell left running ends whilst this one runs the same text.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status"]), T),
        ran("git status", 1, "e", T - 5),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));
}

#[test]
fn an_item_whose_text_more_than_one_running_command_has_ties_to_neither() {
    // 2 open cells run git status.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status"]), T),
        exec_at("b", &commands(&["git status"]), T + 1),
        ran("git status", 1, "e", T + 2),
        done("a"),
        done("b"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));

    // 2 items for the one git status of a cell: git diff still pairs.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff"]), T),
        ran("git status", 0, "s", T + 1),
        ran("git status", 1, "e", T + 2),
        ran("git diff", 0, "d", T + 3),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));

    // A function_call running git status is open too.
    let mut call: Value = serde_json::from_str(&function_call("f", "git status")).expect("record");
    call["timestamp"] = json!("2025-11-08T00:00:00.000Z");
    let lines = in_window(&[
        call.to_string(),
        exec_at("a", &commands(&["git status"]), T),
        ran("git status", 1, "e", T + 2),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));
}

#[test]
fn an_item_from_a_cell_that_closed_whilst_another_ran_ties_to_neither() {
    // a leaves git status running and ends whilst b runs; the item that comes
    // during b may be a's.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status"]), T),
        exec_at("b", &commands(&["git status"]), T + 1),
        done("a"),
        ran("git status", 1, "e", T + 2),
        done("b"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));
}

#[test]
fn an_item_after_the_cells_output_ties_to_nothing() {
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff"]), T),
        ran("git status", 0, "s", T + 1),
        done("a"),
        ran("git diff", 0, "d", T + 2),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));
}

#[test]
fn commands_rtk_rewrote_tie_unless_2_commands_then_read_alike() {
    let pwsh = |cmd: &str, code: i64, ms: i64| item(&["pwsh.exe", "-Command", cmd], code, "ok", ms);
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff | rg x"]), T),
        pwsh("rtk git status", 1, T + 1),
        pwsh("rtk git diff | rtk rg x", 0, T + 2),
        done("a"),
        exec_at("b", &commands(&["git status"]), T + 10),
        pwsh("rtk git status", 0, T + 11),
        output("b", &[(0, json!("ok"))]),
        // Without the rtk word, both read git log, so its one item ties to neither.
        exec_at("c", &commands(&["rtk git log", "git log"]), T + 20),
        ran("rtk git log", 0, "l", T + 21),
        done("c"),
    ]);
    assert_eq!(polls(&lines), (counts(1, 1, 2), json!(1)));
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));
}

#[test]
fn pipes_judges_the_output_codex_recorded_for_the_command() {
    let test = "cargo test 2>&1 | tail -1";
    let mut d = Pipes::default();
    replay(
        &mut d,
        &in_window(&[
            exec_at("a", &commands(&[test]), T),
            ran(test, 0, "test result: FAILED. 1 passed; 1 failed", T + 1),
            done("a"),
        ]),
    );
    assert_eq!(summary(&d, "unguarded_results_found"), 1);
    assert_eq!(summary(&d, "showed_a_failure_but_reported_success"), 1);
    assert_eq!(summary(&d, "codex_outputs_not_paired"), 0);
}

#[test]
fn poll_characters_count_only_what_the_cell_printed_of_a_result() {
    let both = commands(&["git status", "git diff"]);
    let lines = in_window(&[
        exec_at("a", &commands(&["git status"]), T),
        ran("git status", 0, "abc", T + 1),
        output("a", &[(0, json!("abc"))]),
        // Polled, but not printed: no characters.
        exec_at("b", &both, T + 10),
        ran("git status", 0, "abcd", T + 11),
        ran("git diff", 0, "x", T + 12),
        done("b"),
        // Printed in another order: each result's own characters.
        exec_at("c", &both, T + 20),
        ran("git status", 0, "ab", T + 21),
        ran("git diff", 0, "xyz", T + 22),
        output("c", &[(0, json!("xyz")), (0, json!("ab"))]),
        // git status printed its output's end, as exec_command keeps of a long one.
        exec_at("d", &both, T + 30),
        ran("git status", 0, "abcdef", T + 31),
        ran("git diff", 0, "x", T + 32),
        output("d", &[(0, json!("x")), (0, json!("def"))]),
    ]);
    assert_eq!(polls(&lines), (counts(5, 0, 9), json!(0)));
}

#[test]
fn a_command_without_an_item_pairs_by_position_only_where_items_show_the_order() {
    // git diff has no item, as a command whose text a cell builds would not.
    let both = commands(&["git log", "git diff"]);
    let lines = in_window(&[
        // git log's result is printed in its place, so git diff's is too.
        exec_at("a", &both, T),
        ran("git log", 0, "l", T + 1),
        output("a", &[(0, json!("l")), (0, json!("d"))]),
        // git log's result is printed in git diff's place.
        exec_at("b", &both, T + 10),
        ran("git log", 0, "l", T + 11),
        output("b", &[(0, json!("d")), (0, json!("l"))]),
        // One command and one printed result still pair.
        exec_at("c", &commands(&["git diff"]), T + 20),
        output("c", &[(0, json!("d"))]),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));
}

#[test]
fn a_text_a_cell_runs_twice_ties_to_neither_command() {
    // Code need not run the 2 in the order it writes them, and a cell that prints
    // results as they end can print them in start order.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git status"]), T),
        ran("git status", 1, "e", T + 2),
        ran("git status", 0, "s", T + 1),
        output("a", &[(0, json!("s")), (1, json!("e"))]),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));

    // Nor do 2 commands that read alike once rtk is dropped.
    let lines = in_window(&[
        exec_at("a", &commands(&["rtk git log", "git log"]), T),
        ran("rtk git log", 0, "l", T + 1),
        ran("rtk git log", 1, "m", T + 2),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));
}

#[test]
fn an_item_a_call_without_an_id_or_a_time_could_have_run_ties_to_neither() {
    // A cell without an id also runs git status, and its output cannot be found.
    let lines = in_window(&[
        exec_at("", &commands(&["git status"]), T),
        exec_at("a", &commands(&["git status"]), T + 1),
        ran("git status", 1, "e", T + 2),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));

    // A cell whose record has no time could have started before the item.
    let lines = in_window(&[
        exec("b", &commands(&["git status"])),
        exec_at("a", &commands(&["git status"]), T + 1),
        ran("git status", 1, "e", T + 2),
        done("a"),
        done("b"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));
}

#[test]
fn untied_commands_pair_by_position_only_when_one_is_left() {
    // git log's result is in its place, but git diff and git show have no item and
    // may be printed in each other's places.
    let lines = in_window(&[
        exec_at("a", &commands(&["git log", "git diff", "git show"]), T),
        ran("git log", 0, "l", T + 1),
        output("a", &[(0, json!("l")), (0, json!("s")), (0, json!("d"))]),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));
}

#[test]
fn a_printed_result_counts_as_poll_characters_only_for_the_one_command_it_shows() {
    let both = commands(&["git status", "git log"]);
    let lines = in_window(&[
        exec_at("a", &commands(&["git status"]), T),
        ran("git status", 0, "x", T + 1),
        output("a", &[(0, json!("x"))]),
        // Both end with the same output, which the cell printed once: it could be
        // either's, so it counts for neither.
        exec_at("b", &both, T + 10),
        ran("git status", 0, "x", T + 11),
        ran("git log", 0, "x", T + 12),
        output("b", &[(0, json!("x"))]),
        // The one result printed does not show the item's output.
        exec_at("c", &commands(&["git status"]), T + 20),
        ran("git status", 0, "abc", T + 21),
        output("c", &[(0, json!("zzzz"))]),
    ]);
    assert_eq!(polls(&lines), (counts(2, 0, 0), json!(0)));
}

#[test]
fn a_printed_result_an_untied_command_could_own_counts_for_no_tied_one() {
    let lines = in_window(&[
        exec_at("a", &commands(&["git log"]), T),
        ran("git log", 0, "x", T + 1),
        output("a", &[(0, json!("x"))]),
        // git diff has no item and the one result printed may be its own.
        exec_at("b", &commands(&["git log", "git diff"]), T + 10),
        ran("git log", 0, "x", T + 11),
        output("b", &[(0, json!("x"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 0), json!(1)));
}

#[test]
fn an_untied_result_equal_to_a_tied_one_does_not_place_the_untied_command() {
    let lines = in_window(&[
        exec_at("a", &commands(&["git show"]), T),
        ran("git show", 0, "XYZ", T + 1),
        output("a", &[(0, json!("XYZ"))]),
        // git diff has no item and printed what git show did, in git show's place;
        // git show's own result, cut to its end, is in git diff's place.
        exec_at("b", &commands(&["git log", "git show", "git diff"]), T + 10),
        ran("git log", 0, "prefixXYZ", T + 11),
        ran("git show", 0, "XYZ", T + 12),
        output(
            "b",
            &[(0, json!("prefixXYZ")), (0, json!("XYZ")), (0, json!("YZ"))],
        ),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 0), json!(1)));
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));
}

#[test]
fn an_untied_result_equal_to_a_tied_item_places_the_untied_command() {
    let lines = in_window(&[
        exec_at("a", &commands(&["git show"]), T),
        ran("git show", 0, "XYZ", T + 1),
        output("a", &[(0, json!("XYZ"))]),
        // git diff has no item and printed what git show did. Whichever of the 2 is in
        // git show's place, each command's result is XYZ, so the cell pairs by position.
        exec_at("b", &commands(&["git log", "git show", "git diff"]), T + 10),
        ran("git log", 0, "ABC", T + 11),
        ran("git show", 0, "XYZ", T + 12),
        output(
            "b",
            &[(0, json!("ABC")), (0, json!("XYZ")), (0, json!("XYZ"))],
        ),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 3), json!(0)));
    assert_eq!(not_paired(&lines), (json!(0), json!(0)));
}

#[test]
fn a_tied_result_whose_end_is_printed_does_not_place_an_untied_one() {
    // git log's output ends with git diff's, and the cell prints git diff's first.
    let lines = in_window(&[
        exec_at("a", &commands(&["git log", "git diff"]), T),
        ran("git log", 0, "prefixXYZ", T + 1),
        output("a", &[(0, json!("XYZ")), (0, json!("prefixXYZ"))]),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(1)));
}

#[test]
fn commands_built_from_values_are_not_polls_of_each_other() {
    // Each cell runs `cat` on a different file; before the fix both read as `cat `.
    let cat = |id: &str, file: &str| {
        let cell = format!(
            "const f = {}; text(await tools.exec_command({{cmd: 'cat ' + f}}));",
            json!(file)
        );
        exec(id, &cell)
    };
    let lines = in_window(&[
        exec("s", &commands(&["git status"])),
        output("s", &[(0, json!("clean"))]),
        cat("a", "a.md"),
        output("a", &[(1, json!("no such file"))]),
        cat("b", "b.md"),
        output("b", &[(0, json!("text"))]),
    ]);
    assert_eq!(polls(&lines), (counts(0, 0, 0), json!(0)));
}

#[test]
fn an_item_naming_no_command_still_stops_results_pairing_by_position() {
    // Codex recorded an item whilst the cell ran, so the order its results are printed
    // in is not known, even though the item names no command to tie.
    let lines = in_window(&[
        exec_at("a", &commands(&["git status", "git diff"]), T),
        json!({"type": "event_msg", "payload": {
            "type": "item_completed", "started_at_ms": T + 1,
            "item": {"type": "CommandExecution", "exit_code": 0, "aggregated_output": "s"}
        }})
        .to_string(),
        output("a", &[(0, json!("s")), (0, json!("d"))]),
    ]);
    assert_eq!(not_paired(&lines), (json!(1), json!(2)));
}

#[test]
fn an_output_with_an_empty_call_id_leaves_a_call_without_an_id_open() {
    // A malformed output names no call, so the cell without an id stays open and could
    // still have run git status: the later item ties to neither cell.
    let no_id = json!({"type": "response_item", "payload": {
        "type": "function_call_output", "call_id": "", "output": "b"
    }});
    let lines = in_window(&[
        exec_at("b", &commands(&["git log"]), T),
        exec_at("", &commands(&["git status"]), T + 1),
        no_id.to_string(),
        output("b", &[(0, json!("l"))]),
        exec_at("a", &commands(&["git status"]), T + 3),
        ran("git status", 1, "e", T + 4),
        done("a"),
    ]);
    assert_eq!(not_paired(&lines), (json!(2), json!(2)));
}

#[test]
fn a_blank_command_keeps_its_place_when_results_pair_by_position() {
    // The blank command's failure is printed first; git status passed, so its poll
    // does not follow a failure.
    let lines = in_window(&[
        exec("a", &commands(&["  ", "git status"])),
        output("a", &[(1, json!("bad")), (0, json!("clean"))]),
        exec("b", &commands(&["git status"])),
        output("b", &[(0, json!("clean"))]),
    ]);
    assert_eq!(polls(&lines), (counts(1, 0, 5), json!(0)));
}
