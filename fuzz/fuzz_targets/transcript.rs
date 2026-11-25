#![no_main]
//! Any bytes as Claude, Codex, pi, Gemini, Qwen, Antigravity and OpenCode transcripts, `diff`, and all
//! report formats (text, JSON and CSV).

use std::path::PathBuf;
use std::rc::Rc;

use bigsister::cli::{AgentChoice, Cli, Command};
use bigsister::detectors::build;
use bigsister::diff;
use bigsister::prices::Prices;
use bigsister::programs::Programs;
use bigsister::report::{self, Meta, Opts};
use bigsister::scan::{Agent, scan};
use jiff::Timestamp;
use libfuzzer_sys::fuzz_target;

thread_local! {
    static PROGRAMS: Rc<Programs> = Rc::new(Programs::from_env());
    /// A folder of its own for this process's transcripts and `diff` trees.
    static DIR: PathBuf = {
        let dir = std::env::temp_dir().join(format!("bigsister-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("scan")).expect("fuzz scan folder");
        std::fs::create_dir_all(dir.join("antigravity/c/.system_generated/logs"))
            .expect("fuzz antigravity folder");
        for sub in ["before", "after"] {
            for agent in ["claude", "codex", "pi", "gemini", "qwen", "opencode"] {
                std::fs::create_dir_all(dir.join(sub).join(agent)).expect("fuzz diff folder");
            }
        }
        dir
    };
}

fn diff_cli(before: String, after: String, json: bool, csv: bool) -> Cli {
    Cli {
        command: Command::Diff { before, after },
        sample: false,
        since: "7d".to_owned(),
        agent: AgentChoice::All,
        claude_root: None,
        codex_root: None,
        pi_root: None,
        opencode_root: None,
        gemini_root: None,
        qwen_root: None,
        antigravity_root: None,
        json,
        csv,
        limit: 20,
        paths: false,
        by_branch: false,
        usd_per_mtok: None,
        prices: None,
    }
}

fuzz_target!(|data: &[u8]| {
    let dir = DIR.with(Clone::clone);
    let scan_dir = dir.join("scan");
    for name in ["s.jsonl", "s.jsonl.zst"] {
        std::fs::write(scan_dir.join(name), data).expect("transcript");
    }
    std::fs::write(scan_dir.join("opencode.db"), data).expect("opencode db");
    let antigravity = dir.join("antigravity");
    std::fs::write(
        antigravity.join("c/.system_generated/logs/transcript_full.jsonl"),
        data,
    )
    .expect("antigravity log");
    let prices = Rc::new(Prices::bundled());
    let mut dets = PROGRAMS.with(|p| build(Command::All, p, &prices));
    let roots = [
        (Agent::Claude, scan_dir.clone()),
        (Agent::Codex, scan_dir.clone()),
        (Agent::Pi, scan_dir.clone()),
        (Agent::Gemini, scan_dir.clone()),
        (Agent::Qwen, scan_dir.clone()),
        (Agent::Antigravity, antigravity),
        (Agent::OpenCode, scan_dir),
    ];
    let since = Timestamp::UNIX_EPOCH;
    let stats = scan(&roots, Some(since), &prices, &mut dets);
    // File-based roots list both files, the compressed one counted as read or skipped.
    // Antigravity's root holds one conversation's log.
    for st in &stats {
        match st.agent {
            Agent::OpenCode => {}
            Agent::Antigravity => assert_eq!(st.files, 1, "{st:?}"),
            _ => assert_eq!(st.files + st.compressed, 2, "{st:?}"),
        }
    }
    let opts = Opts {
        limit: 20,
        paths: true,
        by_branch: true,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    let sections: Vec<_> = dets.iter().map(|d| d.section(&opts)).collect();
    let meta = Meta {
        since,
        roots: &stats,
        skipped_roots: &[],
        home: None,
    };
    let _ = report::text(&meta, &sections);
    let json = report::json(&meta, &sections);
    assert!(
        serde_json::from_str::<serde_json::Value>(&json).is_ok(),
        "{json}"
    );
    let csv = report::csv(&sections);
    assert!(
        csv.starts_with("section,kind,table,row,column,value\r\n"),
        "{csv}"
    );

    let (left, right) = data.split_at(data.len() / 2);
    for (sub, bytes) in [("before", left), ("after", right)] {
        let base = dir.join(sub);
        for agent in ["claude", "codex", "pi", "gemini", "qwen"] {
            std::fs::write(base.join(agent).join("s.jsonl"), bytes).expect("diff transcript");
        }
        std::fs::write(base.join("opencode").join("opencode.db"), bytes).expect("diff opencode");
    }
    let before = dir.join("before").to_string_lossy().into_owned();
    let after = dir.join("after").to_string_lossy().into_owned();
    let d_json = diff::run(&diff_cli(before.clone(), after.clone(), true, false), &before, &after)
        .expect("diff json");
    assert!(
        serde_json::from_str::<serde_json::Value>(&d_json).is_ok(),
        "{d_json}"
    );
    let d_csv = diff::run(&diff_cli(before.clone(), after.clone(), false, true), &before, &after)
        .expect("diff csv");
    assert!(
        d_csv.starts_with("section,kind,table,row,column,value\r\n"),
        "{d_csv}"
    );
});
