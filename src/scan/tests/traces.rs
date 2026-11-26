use super::parallel::test_dir;
use super::*;

struct DynamicBlob {
    enabled: bool,
    seen: Vec<bool>,
}
impl Detector for DynamicBlob {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }
    fn scans_whole_file(&self) -> bool {
        true
    }
    fn prefilter(&self) -> Option<Prefilter> {
        Some(Prefilter::Superset(|_, line| line.contains("\"keep\"")))
    }
    fn reads_request_blobs(&self) -> bool {
        self.enabled
    }
    fn begin(&mut self, _s: &Session) {
        self.enabled = true;
    }
    fn wants(&self, _s: &Session, line: &str) -> bool {
        line.contains("\"keep\"")
    }
    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        self.seen.push(r.in_window);
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: vec![
                ("seen".to_owned(), (self.seen.len() as u64).into()),
                (
                    "in_window".to_owned(),
                    (self.seen.iter().filter(|&&w| w).count() as u64).into(),
                ),
            ],
            ..Section::default()
        }
    }
}

fn assert_dynamic_blob(dets: &[Box<dyn Detector>]) {
    let opts = Opts {
        limit: 0,
        paths: false,
        by_branch: false,
        by_day: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    assert_eq!(
        dets[0].section(&opts).summary,
        vec![
            ("seen".to_owned(), 1u64.into()),
            ("in_window".to_owned(), 0u64.into()),
        ]
    );
}

#[test]
fn dynamic_blob_detector_receives_wanted_blobs_and_ignores_rejected_ones() {
    let blob_text = concat!(
        r#"{"type":"api-request-blob","timestamp":"2026-11-10T00:00:00Z","drop":true}"#,
        "\n",
        r#"{"type":"api-request-blob","broken":true"#,
        "\n",
        r#"{"type":"api-request-blob","keep":true}"#,
        "\n",
    );
    let prices = Prices::bundled();
    let cutoff = parse_ts("2026-11-09T00:00:00Z");
    let mut mem_dets: Vec<Box<dyn Detector>> = vec![Box::new(DynamicBlob {
        enabled: false,
        seen: Vec::new(),
    })];
    let mem = [Memory {
        agent: Agent::Claude,
        path: "b.jsonl",
        text: blob_text,
    }];
    assert_eq!(
        scan_memory(&mem, "mem", cutoff, &prices, &mut mem_dets)[0].bad_lines,
        0
    );
    assert_dynamic_blob(&mem_dets);

    let dir = test_dir("blob");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    fs::write(dir.join("b.jsonl"), blob_text).expect("blob file");
    let mut disk_dets: Vec<Box<dyn Detector>> = vec![Box::new(DynamicBlob {
        enabled: false,
        seen: Vec::new(),
    })];
    let st = scan(
        &[(Agent::Claude, dir.clone())],
        cutoff,
        &prices,
        &mut disk_dets,
    );
    assert_eq!(st[0].bad_lines, 0);
    assert_dynamic_blob(&disk_dets);
    fs::remove_dir_all(&dir).expect("clean up");
}

#[test]
fn pi_branch_events_count_toward_prep_budget() {
    let dir = test_dir("pi-budget");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    let path = dir.join("p.jsonl");
    let pi_text = concat!(
        r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-11-08T01:00:00Z"}"#,
        "\n",
        r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-08T01:01:00Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
        "\n",
        r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-11-08T01:02:00Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"content":[{"type":"text","text":"one"}]}}"#,
        "\n",
        r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-11-08T01:03:00Z","message":{"role":"user","content":[{"type":"text","text":"next"}]}}"#,
        "\n",
        r#"{"type":"message","id":"d","parentId":"b","timestamp":"2026-11-08T01:04:00Z","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0},"content":[{"type":"text","text":"two"}]}}"#,
        "\n",
    );
    fs::write(&path, pi_text).expect("pi file");
    let prices = Prices::bundled();
    let opts = PrepareOpts {
        agent: Agent::Pi,
        prefilters: None,
        prices: &prices,
        max_events: 4,
        max_raw_bytes: MAX_PREP_RAW_BYTES,
    };
    assert!(matches!(prepare_file(&path, &opts), PreparedFile::TooLarge));
    let mut det = crate::detectors::models::Models::new(std::rc::Rc::new(Prices::bundled()));
    let mut st = RootStats::new(Agent::Pi, dir.clone());
    let mut earlier = Earlier::default();
    let window = Window {
        until: None,
        since: Some(Timestamp::MIN),
        prices: &prices,
    };
    scan_files_with_budget(
        vec![path],
        &window,
        &mut [&mut det],
        &[0],
        &mut st,
        &mut earlier,
        4,
        MAX_PREP_RAW_BYTES,
    );
    assert_eq!(st.files, 1);
    assert_eq!(st.bad_lines, 0);
    fs::remove_dir_all(&dir).expect("clean up");
}

struct Trace(Vec<(String, bool)>);
impl Detector for Trace {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude]
    }
    fn scans_whole_file(&self) -> bool {
        true
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, _s: &Session, r: &Record<'_>) {
        let id = r
            .value
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        self.0.push((id, r.in_window));
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: self
                .0
                .iter()
                .map(|(id, w)| (id.clone(), (*w).into()))
                .collect(),
            ..Section::default()
        }
    }
}

#[test]
fn parallel_scan_preserves_cross_file_order_and_deduplication_trace() {
    let dir = test_dir("trace");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp folder");
    fs::write(
            dir.join("a-later.jsonl"),
            concat!(
                r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T02:00:00Z","message":{"id":"m1"}}"#,
                "\n",
                r#"{"type":"assistant","uuid":"u3","timestamp":"2026-11-08T02:01:00Z","message":{"id":"m1"}}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","timestamp":"2026-11-08T02:02:00Z","message":{}}"#,
                "\n",
            ),
        )
        .expect("later");
    fs::write(
            dir.join("z-earlier.jsonl"),
            r#"{"type":"assistant","uuid":"u1","timestamp":"2026-11-08T01:00:00Z","message":{"id":"m1"}}"#,
        )
        .expect("earlier");
    let prices = Prices::bundled();
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(Trace(Vec::new()))];
    let _ = scan(
        &[(Agent::Claude, dir.clone())],
        Some(Timestamp::MIN),
        &prices,
        &mut dets,
    );
    let opts = Opts {
        limit: 0,
        paths: false,
        by_branch: false,
        by_day: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    assert_eq!(
        dets[0].section(&opts).summary,
        vec![
            ("u1".to_owned(), true.into()),
            ("u1".to_owned(), false.into()),
            ("u3".to_owned(), false.into()),
            ("u2".to_owned(), true.into()),
        ]
    );
    fs::remove_dir_all(&dir).expect("clean up");
}

struct InWindowCount(BTreeMap<Agent, u64>);
impl Detector for InWindowCount {
    fn agents(&self) -> &'static [Agent] {
        &[Agent::Claude, Agent::Gemini, Agent::Qwen]
    }
    fn scans_whole_file(&self) -> bool {
        true
    }
    fn wants(&self, _s: &Session, _line: &str) -> bool {
        true
    }
    fn record(&mut self, s: &Session, r: &Record<'_>) {
        if r.in_window && r.kind() == "assistant" {
            *self.0.entry(s.agent).or_default() += 1;
        }
    }
    fn section(&self, _opts: &Opts) -> Section {
        Section {
            summary: self
                .0
                .iter()
                .map(|(a, n)| (a.name().to_owned(), (*n).into()))
                .collect(),
            ..Section::default()
        }
    }
}

#[test]
fn non_claude_agents_do_not_collide_on_short_per_session_ids() {
    let dir = test_dir("short-ids");
    let _ = fs::remove_dir_all(&dir);
    for sub in ["claude", "gemini", "qwen"] {
        fs::create_dir_all(dir.join(sub)).expect("mkdir");
    }
    let claude_rec = r#"{"type":"assistant","uuid":"g-0","timestamp":"2026-11-08T01:00:00Z","message":{"id":"m1","model":"claude-opus-5-5"}}"#;
    fs::write(dir.join("claude/c1.jsonl"), claude_rec).expect("c1");
    fs::write(dir.join("claude/c2.jsonl"), claude_rec).expect("c2");
    let gemini_rec = r#"{"id":"g-0","type":"gemini","timestamp":"2026-11-08T02:00:00Z","model":"gemini-2.5-pro","toolCalls":[]}"#;
    fs::write(dir.join("gemini/s1.jsonl"), gemini_rec).expect("g1");
    fs::write(dir.join("gemini/s2.jsonl"), gemini_rec).expect("g2");
    let qwen_rec = r#"{"uuid":"g-0","type":"assistant","timestamp":"2026-11-08T03:00:00Z","model":"qwen3-coder-plus","message":{"parts":[]}}"#;
    fs::write(dir.join("qwen/s1.jsonl"), qwen_rec).expect("q1");
    fs::write(dir.join("qwen/s2.jsonl"), qwen_rec).expect("q2");

    let prices = Prices::bundled();
    let mut dets: Vec<Box<dyn Detector>> = vec![Box::new(InWindowCount(BTreeMap::new()))];
    let _ = scan(
        &[
            (Agent::Claude, dir.join("claude")),
            (Agent::Gemini, dir.join("gemini")),
            (Agent::Qwen, dir.join("qwen")),
        ],
        Some(Timestamp::MIN),
        &prices,
        &mut dets,
    );
    let opts = Opts {
        limit: 0,
        paths: false,
        by_branch: false,
        by_day: false,
        home: None,
        usd_per_mtok: None,
        prices_file: None,
    };
    assert_eq!(
        dets[0].section(&opts).summary,
        vec![
            ("claude".to_owned(), 1u64.into()),
            ("gemini".to_owned(), 2u64.into()),
            ("qwen".to_owned(), 2u64.into()),
        ]
    );
    let _ = fs::remove_dir_all(&dir);
}
