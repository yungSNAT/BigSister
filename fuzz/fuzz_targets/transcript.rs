#![no_main]
//! Any bytes as a Claude, a Codex and a pi transcript, plain and compressed, through every
//! detector and both report formats.

use std::path::PathBuf;
use std::rc::Rc;

use jiff::Timestamp;
use libfuzzer_sys::fuzz_target;
use bigsister::cli::Command;
use bigsister::detectors::build;
use bigsister::prices::Prices;
use bigsister::programs::Programs;
use bigsister::report::{self, Meta, Opts};
use bigsister::scan::{Agent, scan};

thread_local! {
    static PROGRAMS: Rc<Programs> = Rc::new(Programs::from_env());
    /// A folder of its own for this process's transcripts.
    static DIR: PathBuf = {
        let dir = std::env::temp_dir().join(format!("bigsister-fuzz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fuzz folder");
        dir
    };
}

fuzz_target!(|data: &[u8]| {
    let dir = DIR.with(Clone::clone);
    for name in ["s.jsonl", "s.jsonl.zst"] {
        std::fs::write(dir.join(name), data).expect("transcript");
    }
    let prices = Rc::new(Prices::bundled());
    let mut dets = PROGRAMS.with(|p| build(Command::All, p, &prices));
    let roots = [
        (Agent::Claude, dir.clone()),
        (Agent::Codex, dir.clone()),
        (Agent::Pi, dir),
    ];
    let since = Timestamp::UNIX_EPOCH;
    let stats = scan(&roots, Some(since), &prices, &mut dets);
    // Each root lists both files, the compressed one counted as read or skipped.
    for st in &stats {
        assert_eq!(st.files + st.compressed, 2, "{st:?}");
    }
    let opts = Opts {
        limit: 20,
        paths: true,
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
});
