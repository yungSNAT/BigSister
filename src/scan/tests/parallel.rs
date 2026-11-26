use super::*;

pub(super) fn test_dir(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(format!("bigsister-{name}-{}", std::process::id()))
}

#[test]
fn parallel_scan_preserves_counts_errors_and_order_across_files() {
    let dir = test_dir("par");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    fs::write(
            dir.join("a.jsonl"),
            concat!(
                r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T01:00:00Z","message":{"model":"claude-sonnet-4-6"}}"#,
                "\n\nnot-json\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2026-11-08T01:01:00Z","message":{}}"#,
                "\n",
            ),
        )
        .expect("a.jsonl");
    fs::write(dir.join("b.jsonl"), "").expect("empty file");
    fs::write(dir.join("c.jsonl.zst"), b"not-zstd").expect("non-zstd");
    fs::write(
        dir.join("d.jsonl.zst"),
        [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x01],
    )
    .expect("corrupt zstd");

    let prices = Prices::bundled();
    let since = Some(Timestamp::MIN);
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(WholeCount(0))];
    let stats = scan(&[(Agent::Claude, dir.clone())], since, &prices, &mut dets);
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].files, 3);
    assert_eq!(stats[0].compressed, 1);
    assert_eq!(stats[0].unreadable, 1);
    assert_eq!(stats[0].bad_lines, 1);
    assert_eq!(counted(&dets), 2);
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn early_finish_and_pi_parallel_paths_record_errors_and_bad_lines() {
    let dir = test_dir("par-err");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("early")).expect("temp folder");
    fs::create_dir_all(dir.join("pi")).expect("temp folder");
    fs::write(dir.join("early").join("c.jsonl.zst"), b"not-zstd").expect("non-zstd");
    fs::write(
        dir.join("early").join("d.jsonl.zst"),
        [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x01],
    )
    .expect("corrupt zstd");
    fs::write(
            dir.join("pi").join("p.jsonl"),
            concat!(
                r#"{"type":"session","id":"s","timestamp":"2026-11-09T06:00:00.000Z"}"#,
                "\n\n{bad\n",
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-09T06:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .expect("p.jsonl");

    let prices = Prices::bundled();
    let since = Some(Timestamp::MIN);
    let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
    let rc_prices = std::rc::Rc::new(Prices::bundled());
    let mut early = crate::detectors::build(crate::cli::Command::Startup, &programs, &rc_prices);
    let early_stats = scan(
        &[(Agent::Claude, dir.join("early"))],
        since,
        &prices,
        &mut early,
    );
    assert_eq!(early_stats[0].files, 1);
    assert_eq!(early_stats[0].compressed, 1);
    assert_eq!(early_stats[0].unreadable, 1);

    let mut pi_dets = crate::detectors::build(crate::cli::Command::Tools, &programs, &rc_prices);
    let pi_stats = scan(&[(Agent::Pi, dir.join("pi"))], since, &prices, &mut pi_dets);
    assert_eq!(pi_stats[0].files, 1);
    assert_eq!(pi_stats[0].bad_lines, 1);
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn streaming_scan_surfaces_mid_file_read_error() {
    struct FailAfterFirst(bool);
    impl Read for FailAfterFirst {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.0 {
                self.0 = true;
                let line = b"{\"type\":\"summary\"}\n";
                buf[..line.len()].copy_from_slice(line);
                Ok(line.len())
            } else {
                Err(io::Error::other("broken stream"))
            }
        }
    }
    let mut st = RootStats::new(Agent::Claude, PathBuf::new());
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Count::default())];
    let mut refs: Vec<&mut (dyn Detector + 'static)> = dets.iter_mut().map(Box::as_mut).collect();
    let err = scan_file(
        Box::new(BufReader::new(FailAfterFirst(false))),
        None,
        Path::new("s.jsonl"),
        &Window {
            until: None,
            since: Some(Timestamp::MIN),
            prices: &Prices::bundled(),
        },
        &mut refs,
        &[0],
        &mut st,
        &mut Earlier::default(),
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Other);
}

#[test]
fn exact_prefilters_match_detector_wants() {
    let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
    let prices = std::rc::Rc::new(Prices::bundled());
    let dets = crate::detectors::build(crate::cli::Command::All, &programs, &prices);
    let session = Session {
        agent: Agent::Claude,
        subagent: false,
    };
    let samples = [
        r#"{"type":"assistant","message":{}}"#,
        r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
        r#"{"type":"api-request-shape"}"#,
        r#"{"type":"summary"}"#,
    ];
    for d in &dets {
        if let Some(Prefilter::Exact(f)) = d.prefilter() {
            for line in samples {
                assert_eq!(f(&session, line), d.wants(&session, line));
            }
        }
    }
}

struct WholeCount(u64);
impl Detector for WholeCount {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Codex]
    }
    fn scans_whole_file(&self) -> bool {
        true
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, _s: &Session, _r: &Record<'_>) {
        self.0 += 1;
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: vec![("records".to_owned(), self.0.into())],
            ..Section::default()
        }
    }
}

#[test]
fn parallel_prep_coalesces_timestamps_and_streams_when_over_budget() {
    let dir = test_dir("budget");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    let body = [
        r#"{"type":"progress","timestamp":"2026-11-08T01:00:00Z"}"#,
        r#"{"type":"progress","timestamp":"2026-11-08T02:00:00Z"}"#,
        r#"{"type":"assistant","uuid":"u1","message":{"model":"claude-sonnet-4-6"}}"#,
        r#"{"type":"assistant","uuid":"u2","message":{"model":"claude-sonnet-4-6"}}"#,
    ]
    .join("\n");
    let file = dir.join("a.jsonl");
    fs::write(&file, format!("{body}\n")).expect("write");

    let prices = Prices::bundled();
    let window = Window {
        until: None,
        since: parse_ts("2026-11-08T01:30:00Z"),
        prices: &prices,
    };
    let programs = std::rc::Rc::new(crate::programs::Programs::from_env());
    let rc_prices = std::rc::Rc::new(Prices::bundled());
    let mut dets = crate::detectors::build(crate::cli::Command::Models, &programs, &rc_prices);
    let mut st = RootStats::new(Agent::Claude, dir.clone());
    let mut earlier = Earlier::default();
    let mut refs: Vec<&mut (dyn Detector + 'static)> = dets.iter_mut().map(Box::as_mut).collect();
    scan_files_with_budget(
        vec![file],
        &window,
        &mut refs,
        &[0],
        &mut st,
        &mut earlier,
        1,
        MAX_PREP_RAW_BYTES,
    );
    assert_eq!(st.files, 1);
    assert_eq!(st.unreadable, 0);
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn more_than_64_active_detectors_do_not_overflow_bitmasks() {
    let prices = Prices::bundled();
    let mut dets: Vec<Box<dyn Detector>> = (0..65)
        .map(|_| Box::new(WholeCount(0)) as Box<dyn Detector>)
        .collect();
    let files = [Memory {
        agent: Agent::Claude,
        path: "s.jsonl",
        text: r#"{"type":"summary"}"#,
    }];
    let stats = scan_memory(&files, "mem", Some(Timestamp::MIN), &prices, &mut dets);
    assert_eq!(stats[0].files, 1);
    assert_eq!(counted(&dets[64..]), 1);
}

struct StopAfterOne(u64);
impl Detector for StopAfterOne {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, _s: &Session, _r: &Record<'_>) {
        self.0 += 1;
    }
    fn done(&self, _s: &Session) -> bool {
        self.0 >= 1
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: vec![("seen".to_owned(), self.0.into())],
            ..Section::default()
        }
    }
}

struct PriceLog(Vec<Option<f64>>);
impl Detector for PriceLog {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Codex]
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, _s: &Session, _r: &Record<'_>) {}
    fn read_by(&mut self, _s: &Session, p: Option<f64>, _in_window: bool) {
        self.0.push(p);
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: vec![("calls".to_owned(), (self.0.len() as u64).into())],
            ..Section::default()
        }
    }
}

#[test]
fn legacy_done_detector_and_spaced_codex_rules_match_sequential() {
    let dir = test_dir("s3");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    fs::write(dir.join("s.jsonl"), "{\"type\":\"summary\"}\nnot-json\n").expect("write");
    let prices = Prices::bundled();
    let mut stop: Vec<Box<dyn Detector>> = vec![Box::new(StopAfterOne(0))];
    let st = scan(
        &[(Agent::Claude, dir.clone())],
        Some(Timestamp::MIN),
        &prices,
        &mut stop,
    );
    assert_eq!(st[0].bad_lines, 0);
    assert_eq!(counted(&stop), 1);

    let mut pl: Vec<Box<dyn Detector>> = vec![Box::new(PriceLog(Vec::new()))];
    let cm = [Memory {
        agent: Agent::Codex,
        path: "c.jsonl",
        text: "{\"type\": \"turn_context\", \"payload\": {\"model\": \"gpt-5.4\"}}\n",
    }];
    let _ = scan_memory(&cm, "mem", Some(Timestamp::MIN), &prices, &mut pl);
    assert_eq!(counted(&pl), 1);
    fs::remove_dir_all(&dir).expect("clean up");
}
