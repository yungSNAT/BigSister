use serde_json::json;

use super::*;
use crate::detectors::{claude_tool_result, claude_tool_use, replay};

#[test]
fn a_reused_claude_tool_use_id_pairs_with_neither_read() {
    let read = |path: &str| json!({"file_path": path});
    let boundary = json!({"type": "system", "subtype": "compact_boundary"});
    let lines: Vec<(bool, String)> = [
        claude_tool_use("a", "Read", &read("/work/y.md")),
        boundary.to_string(),
        // The re-read and a block reusing its id: the result may be either's.
        claude_tool_use("b", "Read", &read("/work/y.md")),
        claude_tool_use("b", "Bash", &json!({"command": "ls"})),
        claude_tool_result("b", "12345"),
    ]
    .into_iter()
    .map(|l| (true, l))
    .collect();
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Rereads::default();
    replay(&mut d, &s, &lines);
    let c = d.totals[&(Agent::Claude, "main")];
    assert_eq!((c.rereads, c.reread_chars), (1, 0));
}

/// The rows of `rereads`' table titled `title`, each with the values of `keys`.
fn table_cells(d: &Rereads, title: &str, keys: &[&str]) -> Value {
    let opts = Opts {
        limit: 100,
        paths: false,
        by_branch: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    let section = d.section(&opts);
    let table = section
        .tables
        .iter()
        .find(|t| t.title == title)
        .unwrap_or_else(|| panic!("no table {title}"));
    let at = |k: &&str| table.columns.iter().position(|c| c == k).expect("a column");
    table
        .rows
        .iter()
        .map(|row| keys.iter().map(|k| row[at(k)].clone()).collect::<Value>())
        .collect()
}

/// A Claude session that compacts by hand twice, automatically once and once
/// without metadata, reading the same file again after each.
fn trigger_session() -> Vec<(bool, String)> {
    let read = |id| claude_tool_use(id, "Read", &json!({"file_path": "/work/a.md"}));
    let boundary = |trigger: &str, tokens: u64| {
        json!({"type": "system", "subtype": "compact_boundary",
               "compactMetadata": {"trigger": trigger, "preTokens": tokens}})
        .to_string()
    };
    vec![
        (true, read("a")),
        // Out of the window: sets the trigger but counts no compaction.
        (false, boundary("manual", 1)),
        (true, boundary("manual", 5000)),
        (true, read("b")),
        (true, claude_tool_result("b", "12345678")),
        (true, boundary("auto", 9000)),
        (true, read("c")),
        (true, boundary("manual", 7000)),
        (true, read("d")),
        // No metadata, as pi's, OpenCode's and Qwen's boundaries have.
        (
            true,
            json!({"type": "system", "subtype": "compact_boundary"}).to_string(),
        ),
        (true, read("e")),
    ]
}

#[test]
fn a_result_counts_toward_the_trigger_before_its_call() {
    let boundary = |trigger: &str| {
        json!({"type": "system", "subtype": "compact_boundary",
               "compactMetadata": {"trigger": trigger}})
        .to_string()
    };
    let read = |id| claude_tool_use(id, "Read", &json!({"file_path": "/work/a.md"}));
    let lines: Vec<(bool, String)> = [
        read("a"),
        boundary("manual"),
        read("b"),
        // A compaction between the re-read and its result.
        boundary("auto"),
        claude_tool_result("b", "1234"),
    ]
    .into_iter()
    .map(|l| (true, l))
    .collect();
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Rereads::default();
    replay(&mut d, &s, &lines);
    let keys = ["trigger", "rereads", "reread_chars"];
    assert_eq!(
        table_cells(&d, "By compaction trigger", &keys),
        json!([["auto", 0, 0], ["manual", 1, 4]])
    );
}

#[test]
fn a_trigger_prints_only_as_a_label() {
    let boundary = json!({"type": "system", "subtype": "compact_boundary",
                          "compactMetadata": {"trigger": "secret: do not print"}});
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Rereads::default();
    replay(&mut d, &s, &[(true, boundary.to_string())]);
    assert_eq!(
        table_cells(&d, "By compaction trigger", &["trigger", "compactions"]),
        json!([["(other)", 1]])
    );
}

#[test]
fn rereads_count_toward_the_trigger_of_the_latest_compaction() {
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Rereads::default();
    replay(&mut d, &s, &trigger_session());
    let keys = [
        "trigger",
        "compactions",
        "median_pre_compaction_tokens",
        "max_pre_compaction_tokens",
        "rereads",
        "reread_chars",
        "rereads_per_compaction",
    ];
    // The median of 2 values is the lower, by nearest rank.
    assert_eq!(
        table_cells(&d, "By compaction trigger", &keys),
        json!([
            ["manual", 2, 5000, 7000, 2, 8, 1.0],
            ["auto", 1, 9000, 9000, 1, 0, 1.0],
            ["not recorded", 1, null, null, 1, 0, 1.0]
        ])
    );
}

#[test]
fn reads_with_no_path_are_not_reads() {
    let read = |id| claude_tool_use(id, "Read", &json!({"file_path": ""}));
    let boundary = json!({"type": "system", "subtype": "compact_boundary"});
    let lines: Vec<(bool, String)> = [read("a"), boundary.to_string(), read("b")]
        .into_iter()
        .map(|l| (true, l))
        .collect();
    let s = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let mut d = Rereads::default();
    replay(&mut d, &s, &lines);
    let counts = d.totals.get(&(Agent::Claude, "main"));
    assert_eq!(counts.map_or((0, 0), |c| (c.reads, c.rereads)), (0, 0));
}
