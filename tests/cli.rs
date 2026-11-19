//! Runs the `bigsister` binary over the synthetic fixtures in `tests/fixtures`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

const CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude");
const CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex");
const SINCE: &str = "2026-11-01T00:00:00Z";
/// A folder of empty stub programs for the fixtures' commands, put first on `PATH`
/// so their labels do not depend on the machine.
fn stubs() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stub-programs");
        std::fs::create_dir_all(&dir).expect("stub folder");
        for name in ["agent-heavy", "rtk"] {
            let file = dir.join(if cfg!(windows) {
                format!("{name}.exe")
            } else {
                name.to_owned()
            });
            std::fs::write(&file, "").expect("stub program");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                    .expect("execute bit");
            }
        }
        dir
    })
}

/// Runs `bigsister` from the crate folder, where fixture hook commands name scripts in
/// `tests/fixtures/hooks`, with the stub programs first on `PATH`.
fn run(args: &[&str]) -> (i32, String, String) {
    run_env(args, &[])
}

/// As `run`, with extra environment variables.
fn run_env(args: &[&str], env: &[(&str, &Path)]) -> (i32, String, String) {
    let system = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(stubs().to_path_buf()).chain(std::env::split_paths(&system)),
    )
    .expect("PATH");
    // An empty home folder, so that no default folder of the machine running the tests
    // is read; a test can give its own.
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-empty");
    std::fs::create_dir_all(&home).expect("home folder");
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args(args)
        .env("PATH", path)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .envs(env.iter().copied())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("binary runs");
    let text = |b: Vec<u8>| String::from_utf8(b).expect("UTF-8 output");
    (
        out.status.code().unwrap_or(-1),
        text(out.stdout),
        text(out.stderr),
    )
}

fn fixture_args<'a>(cmd: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        cmd,
        "--claude-root",
        CLAUDE,
        "--codex-root",
        CODEX,
        "--since",
        SINCE,
    ];
    args.extend_from_slice(extra);
    args
}

fn report(cmd: &str, extra: &[&str]) -> Value {
    let mut args = fixture_args(cmd, extra);
    args.push("--json");
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn rows<'a>(doc: &'a Value, detector: &str, table: usize) -> &'a Vec<Value> {
    doc["detectors"][detector]["tables"][table]["rows"]
        .as_array()
        .expect("rows")
}

fn find<'a>(rows: &'a [Value], pairs: &[(&str, Value)]) -> &'a Value {
    rows.iter()
        .find(|r| pairs.iter().all(|(k, v)| &r[*k] == v))
        .unwrap_or_else(|| panic!("no row matching {pairs:?} in {rows:?}"))
}

#[test]
fn startup_measures_main_sessions_started_in_the_window() {
    let doc = report("startup", &[]);
    let s = &doc["detectors"]["startup"]["summary"];
    assert_eq!(s["sessions"], 1);
    assert_eq!(s["entry_points"], "claude-desktop 1");
    assert_eq!(s["median_total_chars"], 230);
    assert_eq!(s["sessions_with_request_tokens"], 1);
    assert_eq!(s["median_first_request_tokens"], 4312);
    let r = rows(&doc, "startup", 0);
    for (piece, chars) in [
        ("tool schemas (loaded)", 121),
        ("skill listing", 30),
        ("file: C:/Users/tester/.claude/CLAUDE.md", 25),
        ("hook: hub-digest.js", 19),
        ("deferred tool names", 19),
        ("mcp instructions: firecrawl", 16),
    ] {
        assert_eq!(
            find(r, &[("piece", json!(piece))])["median_chars"],
            chars,
            "{piece}"
        );
    }
    assert_eq!(
        r.len(),
        6,
        "nothing after the first reply and nothing from the old session"
    );
}

#[test]
fn startup_tokens_skip_synthetic_replies_and_unusable_usage() {
    // 5 sessions: first replies recording 100, 300 and (after a synthetic reply and an
    // instruction file) 200 input tokens, one recording none and one whose sum
    // overflows.
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/startup-tokens");
    let (code, stdout, stderr) = run(&[
        "startup",
        "--agent",
        "claude",
        "--claude-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let s = &doc["detectors"]["startup"]["summary"];
    assert_eq!(s["sessions"], 5);
    assert_eq!(s["sessions_with_request_tokens"], 3);
    assert_eq!(s["median_first_request_tokens"], 200);
    let file = find(
        rows(&doc, "startup", 0),
        &[("piece", json!("file: /t/CLAUDE.md"))],
    );
    assert_eq!(file["median_chars"], 8);
}

#[test]
fn startup_does_not_measure_a_session_whose_replies_are_all_synthetic() {
    let root = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/startup-synthetic"
    );
    let (code, stdout, stderr) = run(&[
        "startup",
        "--agent",
        "claude",
        "--claude-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(root_counts(&doc)[0], &json!(1), "the session was read");
    let s = &doc["detectors"]["startup"]["summary"];
    assert_eq!(s["sessions"], 0);
    assert_eq!(s["sessions_with_request_tokens"], 0);
    assert_eq!(s["median_first_request_tokens"], Value::Null);
    assert!(rows(&doc, "startup", 0).is_empty());
}

#[test]
fn startup_alone_stops_reading_at_the_first_reply() {
    // The main session ends in a cut line long after its first reply.
    assert_eq!(report("startup", &[])["roots"][0]["lines_skipped"], 0);
    assert_eq!(report("all", &[])["roots"][0]["lines_skipped"], 1);
}

const CACHE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cache");

/// The cache section over the cache fixture, with `extra` options.
fn cache_report(extra: &[&str]) -> Value {
    let mut args = vec![
        "cache",
        "--claude-root",
        CACHE,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn cache_splits_writes_into_new_context_and_rebuilds_by_cause() {
    let doc = cache_report(&[]);
    let r = rows(&doc, "cache", 0);
    let row = |scope: &str, cause: &str| {
        let row = find(r, &[("scope", json!(scope)), ("cause", json!(cause))]);
        (
            row["replies"].as_u64().unwrap(),
            row["write_tokens"].as_u64().unwrap(),
            row["write_usd_est"].as_f64().unwrap(),
            row["unpriced_tokens"].as_u64().unwrap(),
        )
    };
    // A second record of the first reply adds nothing; 5-minute writes cost 1.25 times
    // the input price (Opus 5.5, US$4), 1-hour writes 2 times (Sonnet 5.5, US$2).
    assert_eq!(row("main", "first reply"), (1, 1_000_000, 5.0, 0));
    // Warm replies, the new part of each rebuild, after a compaction, and the new part
    // of the second reply of a session whose first is before the window.
    assert_eq!(row("main", "new context"), (7, 1_110_000, 4.94, 0));
    // 9 minutes past a 5-minute write, 3 hours past a 1-hour write, and 2 days after a
    // reply before the window.
    assert_eq!(
        row("main", "rebuilt after idle gap"),
        (3, 2_500_000, 11.7, 0)
    );
    assert_eq!(
        row("main", "rebuilt after model switch"),
        (1, 1_600_000, 4.0, 0)
    );
    // 28 minutes after a 1-hour write is inside the lifetime.
    assert_eq!(row("main", "rebuilt otherwise"), (1, 1_750_000, 7.0, 0));
    assert_eq!(row("subagent", "first reply"), (1, 400_000, 2.0, 0));
    // A model without a price leaves its rebuild unpriced.
    assert_eq!(
        row("subagent", "rebuilt after idle gap"),
        (1, 400_000, 0.0, 400_000)
    );
    assert_eq!(r.len(), 7);
    let s = &doc["detectors"]["cache"]["summary"];
    assert_eq!(s["sessions"], 3);
    assert_eq!(s["write_tokens"], 8_760_000);
    assert_eq!(s["idle_rebuilds"], 4);
    assert_eq!(s["sessions_with_idle_rebuilds"], 3);
    assert_eq!(s["idle_rebuild_share"], 0.33);
    assert_eq!(s["replies_skipped"], 1);
    assert_eq!(s["replies_without_split"], 2);
}

#[test]
fn cache_dollars_take_the_given_rate_times_the_write_multiplier() {
    let doc = cache_report(&["--usd-per-mtok", "10"]);
    let idle = find(
        rows(&doc, "cache", 0),
        &[
            ("scope", json!("main")),
            ("cause", json!("rebuilt after idle gap")),
        ],
    );
    // 1,100,000 and 600,000 at 1.25, 800,000 at 2: 3,725,000 at US$10 a million.
    assert_eq!(idle["write_usd_est"], 37.25);
    let sub = find(
        rows(&doc, "cache", 0),
        &[
            ("scope", json!("subagent")),
            ("cause", json!("rebuilt after idle gap")),
        ],
    );
    assert_eq!(
        (sub["write_usd_est"].clone(), sub["unpriced_tokens"].clone()),
        (json!(5.0), json!(0))
    );
    let notes = doc["detectors"]["cache"]["notes"].to_string();
    assert!(notes.contains("--usd-per-mtok"), "{notes}");
}

const MODELS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/models");

fn models_report(extra: &[&str]) -> Value {
    let mut args = vec![
        "models",
        "--claude-root",
        MODELS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

/// The values of `keys` in a row or summary, as a JSON array.
fn cells(row: &Value, keys: &[&str]) -> Value {
    keys.iter().map(|k| row[*k].clone()).collect()
}

#[test]
fn models_price_input_by_model_effort_and_agent_type() {
    let doc = models_report(&[]);
    let r = rows(&doc, "models", 0);
    let keys = [
        "scope",
        "model",
        "effort",
        "agent_type",
        "input_usd_est",
        "input_usd_share",
    ];
    // Most input dollars first. Uncached input at the input price, writes at 1.25 or 2
    // times, reads at 0.025 (Fable 5.2), 0.05 (Opus and Sonnet 5.5) or 0.1 times it.
    let got: Value = r.iter().map(|row| cells(row, &keys)).collect();
    let want = json!([
        ["main", "claude-opus-5-5", "(none)", "(none)", 1.6, 0.42],
        ["main", "claude-fable-5-2", "xhigh", "(none)", 1.21, 0.32],
        ["subagent", "claude-fable-5-2", "high", "worker", 0.48, 0.13],
        ["main", "claude-opus-5-5", "high", "(none)", 0.4, 0.11],
        [
            "subagent",
            "claude-sonnet-5-5",
            "low",
            "mechanical",
            0.1,
            0.03
        ],
        ["subagent", "claude-opus-5-5", "high", "worker", 0.02, 0.01],
        [
            "subagent",
            "claude-unpriced-x",
            "(none)",
            "(other)",
            0.0,
            0.0
        ]
    ]);
    assert_eq!(got, want);
    // A second block of a reply adds nothing; output is counted, not priced.
    let tokens = [
        "replies",
        "input_tokens",
        "cache_write_tokens",
        "cache_read_tokens",
        "output_tokens",
    ];
    assert_eq!(
        cells(&r[1], &tokens),
        json!([1, 1000, 100_000, 1_000_000, 5000])
    );
    assert_eq!(r[6]["unpriced_replies"], 1);
}

#[test]
fn models_rank_subagents_against_the_parent_s_top_priced_model() {
    let keys = [
        "replies",
        "replies_skipped",
        "subagent_sessions",
        "subagent_sessions_on_top_model",
        "subagent_sessions_below_top_model",
        "subagent_sessions_indeterminate",
        "on_top_input_usd_est",
    ];
    // A Fable 5.2 subagent is on top of its parent's Fable 5.2 (a dearer model before the
    // window does not count); a Sonnet one is below; an unpriced one and one whose
    // parent was not read are indeterminate.
    let doc = models_report(&[]);
    let s = &doc["detectors"]["models"]["summary"];
    assert_eq!(cells(s, &keys), json!([7, 1, 4, 1, 1, 2, 0.48]));
    // Only the 1-hour reply records a split.
    assert_eq!(s["replies_without_split"], 6);
    // A rate prices every reply's weighted tokens, unpriced ones too, and ranks the same.
    let doc = models_report(&["--usd-per-mtok", "10"]);
    let s = &doc["detectors"]["models"]["summary"];
    assert_eq!(cells(s, &keys), json!([7, 1, 4, 1, 1, 2, 0.6]));
    let unpriced = find(
        rows(&doc, "models", 0),
        &[("model", json!("claude-unpriced-x"))],
    );
    assert_eq!(
        cells(unpriced, &["input_usd_est", "unpriced_replies"]),
        json!([0.13, 0])
    );
}

#[test]
fn models_rank_subagents_at_a_price_file_s_prices() {
    // Fable 5.2 at US$2 under Opus 5.5 at US$4 puts the Fable subagent below its parent's
    // top, and Sonnet 5.5 at US$8 puts the Sonnet one on top: its 1,000,000 cache reads at
    // 0.05 times US$8 per million are US$0.40.
    let file = price_file(
        "prices-models.json",
        r#"{"claude-fable-5-2": {"input_cost_per_token": 2e-06},
            "claude-opus-5-5": {"input_cost_per_token": 4e-06},
            "claude-sonnet-5-5": {"input_cost_per_token": 8e-06}}"#,
    );
    let doc = models_report(&["--prices", file.to_str().expect("UTF-8 path")]);
    let s = &doc["detectors"]["models"]["summary"];
    let keys = [
        "subagent_sessions_on_top_model",
        "subagent_sessions_below_top_model",
        "subagent_sessions_indeterminate",
        "on_top_input_usd_est",
    ];
    assert_eq!(cells(s, &keys), json!([1, 1, 2, 0.4]));
}

const MODELS_TIERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/models-tiers");

/// The models section of the tiers fixture.
fn tier_report(extra: &[&str]) -> Value {
    let mut args = vec![
        "models",
        "--claude-root",
        MODELS_TIERS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    doc["detectors"]["models"].clone()
}

/// The subagent ranks of the tiers fixture: on top, below and the on-top dollars.
fn tier_ranks(extra: &[&str]) -> Value {
    let keys = [
        "subagent_sessions_on_top_model",
        "subagent_sessions_below_top_model",
        "on_top_input_usd_est",
    ];
    cells(&tier_report(extra)["summary"], &keys)
}

#[test]
fn models_rank_subagents_by_base_price_whatever_the_prompt_size() {
    // t1's Haiku 5.5 subagent is on its parent's top model, although one of t1's prompts
    // was over 100,000 tokens and so dearer; its 100,000 tokens at US$0.10 per million are
    // US$0.01. t2's Sonnet 5.5 subagent is below Opus 5.5.
    assert_eq!(tier_ranks(&[]), json!([1, 1, 0.01]));
    // Dollars keep each reply's tier: t1's 150,000 tokens at US$0.50 per million and its
    // 50,000 at US$0.10 are US$0.08.
    let section = tier_report(&[]);
    let main = find(
        section["tables"][0]["rows"].as_array().expect("rows"),
        &[
            ("scope", json!("main")),
            ("model", json!("claude-haiku-5-5")),
        ],
    );
    assert_eq!(main["input_usd_est"], 0.08);
    // A price file that tiers Sonnet 5.5 above Opus 5.5 for long prompts still ranks it
    // by its base price, below.
    let file = price_file(
        "prices-models-tiers.json",
        r#"{"claude-haiku-5-5": {"input_cost_per_token": 1e-07,
                                 "input_cost_per_token_above_100k_tokens": 5e-07},
            "claude-opus-5-5": {"input_cost_per_token": 4e-06},
            "claude-sonnet-5-5": {"input_cost_per_token": 2e-06,
                                  "input_cost_per_token_above_100k_tokens": 9e-06}}"#,
    );
    let file = file.to_str().expect("UTF-8 path");
    assert_eq!(tier_ranks(&["--prices", file]), json!([1, 1, 0.01]));
}

/// Claude Code 2.1.308 wrote a block of a subagent's reply again in sibling subagent files
/// with new uuids (seen on one machine, October 2026): the message id says it is one reply,
/// counted once.
#[test]
fn a_reply_another_file_holds_under_new_uuids_counts_once() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("reply-copies");
    let _ = std::fs::remove_dir_all(&dir);
    let subs = dir.join("proj/p/subagents");
    std::fs::create_dir_all(&subs).expect("folder");
    let block = |uuid: &str, time: &str| {
        json!({"type": "assistant", "uuid": uuid, "timestamp": time, "sessionId": "p",
               "message": {"model": "claude-opus-5-5", "id": "msg_x", "role": "assistant",
                           "content": [], "usage": {"input_tokens": 10,
                           "cache_creation_input_tokens": 40000,
                           "cache_read_input_tokens": 0, "output_tokens": 5}}})
        .to_string()
    };
    let start = json!({"type": "user", "uuid": "start", "timestamp": "2026-11-08T01:00:00Z",
                       "sessionId": "p", "message": {"role": "user", "content": "go"}})
    .to_string();
    let a = [
        start.clone(),
        block("a1", "2026-11-08T01:00:10Z"),
        block("a2", "2026-11-08T01:00:11Z"),
    ];
    let b = [
        start.replace("\"start\"", "\"start-b\""),
        block("b1", "2026-11-08T01:00:11Z"),
    ];
    std::fs::write(subs.join("agent-a.jsonl"), a.join("\n")).expect("file");
    std::fs::write(subs.join("agent-b.jsonl"), b.join("\n")).expect("file");
    let root = dir.to_str().expect("UTF-8 path");
    let report = |section: &str| {
        let args = [
            section,
            "--claude-root",
            root,
            "--agent",
            "claude",
            "--since",
            SINCE,
            "--json",
        ];
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "stderr: {stderr}");
        serde_json::from_str::<Value>(&stdout).expect("valid JSON")["detectors"][section]["summary"]
            .clone()
    };
    let models = report("models");
    assert_eq!(
        cells(&models, &["replies", "subagent_sessions"]),
        json!([1, 1])
    );
    assert_eq!(report("cache")["write_tokens"], json!(40000));
}

#[test]
fn models_count_unpriced_replies_whatever_the_limit() {
    // The unpriced row sorts last and --limit 1 cuts it; the summary still counts it, in
    // each format: 1 reply with 10,000 cache write tokens.
    let args = [
        "models",
        "--claude-root",
        MODELS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--limit",
        "1",
    ];
    let doc: Value = serde_json::from_str(&output(&args, "--json")).expect("valid JSON");
    let section = &doc["detectors"]["models"];
    let keys = ["replies_unpriced", "unpriced_input_tokens"];
    assert_eq!(cells(&section["summary"], &keys), json!([1, 10_000]));
    assert_eq!(section["tables"][0]["omitted"], 6);
    let csv = output(&args, "--csv");
    for record in [
        "models,summary,,,replies_unpriced,1\r\n",
        "models,summary,,,unpriced_input_tokens,10000\r\n",
        "models,omitted,0,,,6\r\n",
    ] {
        assert!(csv.contains(record), "{record} {csv}");
    }
    let (code, text, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    for line in [
        "replies unpriced: 1\n",
        "unpriced input tokens: 10000\n",
        "(6 more rows; raise --limit to see them)",
    ] {
        assert!(text.contains(line), "{line} {text}");
    }
    // A rate prices every reply.
    let mut rated = args.to_vec();
    rated.extend(["--usd-per-mtok", "10", "--json"]);
    let (_, stdout, _) = run(&rated);
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let s = &doc["detectors"]["models"]["summary"];
    assert_eq!(cells(s, &keys), json!([0, 0]));
}

#[test]
fn cache_dollars_take_a_price_file_and_name_it() {
    // Opus 5.5 at US$8 instead of US$4 doubles the first reply's 5-minute writes.
    let file = price_file(
        "prices-cache.json",
        r#"{"claude-opus-5-5": {"input_cost_per_token": 8e-06}}"#,
    );
    let doc = cache_report(&["--prices", file.to_str().expect("UTF-8 path")]);
    let first = find(
        rows(&doc, "cache", 0),
        &[("scope", json!("main")), ("cause", json!("first reply"))],
    );
    assert_eq!(first["write_usd_est"], 10.0);
    let notes = doc["detectors"]["cache"]["notes"].to_string();
    assert!(notes.contains("prices-cache.json"), "{notes}");
    assert!(!notes.contains("bundled"), "{notes}");
}

#[test]
fn schemas_report_idle_groups_first_and_skip_deferred_tools() {
    let doc = report("schemas", &[]);
    let s = &doc["detectors"]["schemas"]["summary"];
    assert_eq!(
        (s["main_sessions"].clone(), s["subagent_sessions"].clone()),
        (json!(1), json!(1))
    );
    let r = rows(&doc, "schemas", 0);
    assert_eq!(r[0]["tool"], "mcp__unused__*");
    assert_eq!(
        (
            r[0]["idle_chars"].clone(),
            r[0]["idle_tokens_est"].clone(),
            r[0]["sessions_called"].clone()
        ),
        (json!(28), json!(7), json!(0))
    );
    let browser = find(
        r,
        &[("scope", json!("main")), ("tool", json!("mcp__browser__*"))],
    );
    assert_eq!(browser["tools"], 2);
    assert_eq!(browser["calls"], 1);
    assert_eq!(browser["median_chars"], 63);
    assert_eq!(browser["idle_chars"], 0);
    assert!(
        r.iter().all(|row| row["tool"] != "WebFetch"),
        "deferred tools are not loaded"
    );
    assert_eq!(
        find(r, &[("scope", json!("subagent")), ("tool", json!("Bash"))])["calls"],
        2
    );
}

const SKILLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/skills");

#[test]
fn schemas_list_skills_against_skill_calls_and_slash_commands() {
    let (code, stdout, stderr) = run(&[
        "schemas",
        "--claude-root",
        SKILLS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let r = rows(&doc, "schemas", 1);
    let skill = |name: &str| find(r, &[("scope", json!("main")), ("skill", json!(name))]);
    // Called with Skill in one session, listed and idle in the other, where a second,
    // shorter listing of it leaves its largest entry.
    let used = skill("used-skill");
    assert_eq!(
        [
            &used["sessions_listed"],
            &used["sessions_used"],
            &used["uses"],
            &used["idle_chars"]
        ],
        [&json!(2), &json!(1), &json!(1), &json!(29)]
    );
    // A slash command uses a skill; its name quoted in a tool result does not.
    assert_eq!(skill("slash-skill")["sessions_used"], 1);
    let idle = skill("plug:idle-skill");
    assert_eq!(
        [
            &idle["sessions_used"],
            &idle["idle_chars"],
            &idle["idle_tokens_est"]
        ],
        [&json!(0), &json!(60), &json!(15)]
    );
    // A name listed alone, a name listed in a later listing, and a name that is not
    // printable.
    assert_eq!(skill("bare-skill")["idle_chars"], 13);
    assert_eq!(skill("late-skill")["idle_chars"], 28);
    assert_eq!(skill("(other)")["idle_chars"], 26);
    // A line goes to the longest name it starts: `- pre:fix:` is not `pre`'s.
    assert_eq!(skill("pre")["idle_chars"], 18);
    assert_eq!(skill("pre:fix")["idle_chars"], 22);
    assert_eq!(
        r[0]["skill"], "plug:idle-skill",
        "most idle characters first"
    );
    assert_eq!(r.len(), 8);
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
}

/// A skill listing without a `names` array and a `Skill` call without a string skill are
/// counted as skipped in every format, not dropped.
#[test]
fn schemas_count_skill_listings_and_calls_they_cannot_read() {
    let out = |format: &str| {
        let (code, stdout, stderr) = run(&[
            "schemas",
            "--claude-root",
            SKILLS,
            "--agent",
            "claude",
            "--since",
            SINCE,
            format,
        ]);
        assert_eq!(code, 0, "stderr: {stderr}");
        stdout
    };
    let doc: Value = serde_json::from_str(&out("--json")).expect("valid JSON");
    let s = &doc["detectors"]["schemas"]["summary"];
    assert_eq!(
        (&s["skill_listings_skipped"], &s["skill_calls_skipped"]),
        (&json!(1), &json!(2))
    );
    let text = out("--limit=20");
    assert!(text.contains("\nskill listings skipped: 1\n"), "{text}");
    assert!(text.contains("\nskill calls skipped: 2\n"), "{text}");
    let csv = out("--csv");
    assert!(
        csv.contains("schemas,summary,,,skill_listings_skipped,1"),
        "{csv}"
    );
    assert!(
        csv.contains("schemas,summary,,,skill_calls_skipped,2"),
        "{csv}"
    );
}

#[test]
fn pipes_count_unguarded_test_runs_and_masked_failures() {
    let doc = report("pipes", &[]);
    let s = &doc["detectors"]["pipes"]["summary"];
    let expected = json!({
        "test_runs": 5,
        "piped_into_tail_or_head": 5,
        "over_ssh_not_judged": 1,
        "guarded_with_pipefail_or_pipestatus": 1,
        "unguarded": 3,
        "unguarded_results_found": 2,
        "unguarded_reported_success": 2,
        "showed_a_failure_but_reported_success": 1,
        "showed_no_pass_or_fail_line": 0,
        "codex_outputs_not_paired": 0,
    });
    assert_eq!(s, &expected);
}

#[test]
fn polls_separate_repeats_from_reruns_after_an_edit() {
    let doc = report("polls", &[]);
    let totals = rows(&doc, "polls", 0);
    for (agent, scope, commands, polls, reruns) in [
        ("claude", "main", 14, 3, 1),
        ("claude", "subagent", 2, 1, 0),
        ("codex", "main", 5, 1, 1),
        ("codex", "subagent", 2, 1, 0),
    ] {
        let row = find(totals, &[("agent", json!(agent)), ("scope", json!(scope))]);
        assert_eq!(
            (
                row["commands"].clone(),
                row["polls"].clone(),
                row["reruns_after_edit"].clone()
            ),
            (json!(commands), json!(polls), json!(reruns)),
            "{agent} {scope}"
        );
    }
    let verbs = rows(&doc, "polls", 1);
    assert_eq!(verbs[0]["verb"], "agent-heavy");
    assert_eq!(verbs[0]["polls"], 2);
    assert_eq!(
        find(
            verbs,
            &[
                ("agent", json!("codex")),
                ("verb", json!("ssh agent-heavy"))
            ]
        )["polls"],
        1
    );
    assert_eq!(
        find(
            verbs,
            &[("agent", json!("claude")), ("verb", json!("cargo"))]
        )["reruns_after_edit"],
        1
    );
}

#[test]
fn polls_report_result_characters_and_repeats_of_a_failed_run() {
    let doc = report("polls", &[]);
    let totals = rows(&doc, "polls", 0);
    let cost = |row: &Value| {
        (
            row["polls_after_failure"].clone(),
            row["poll_chars"].clone(),
            row["poll_tokens_est"].clone(),
        )
    };
    // `agent-heavy status` fails, is polled (17 characters back) and polled again after
    // that success (15 and 5 characters in 2 text blocks). The old session's `git status`
    // failed before the window and is polled inside it, with no result in the transcript.
    let main = find(
        totals,
        &[("agent", json!("claude")), ("scope", json!("main"))],
    );
    assert_eq!(cost(main), (json!(2), json!(37), json!(10)));
    let sub = find(
        totals,
        &[("agent", json!("claude")), ("scope", json!("subagent"))],
    );
    assert_eq!(cost(sub), (json!(0), json!(0), json!(0)));
    let verbs = rows(&doc, "polls", 1);
    for (verb, expected) in [
        ("agent-heavy", (json!(1), json!(37), json!(10))),
        ("git", (json!(1), json!(0), json!(0))),
    ] {
        let row = find(verbs, &[("agent", json!("claude")), ("verb", json!(verb))]);
        assert_eq!(cost(row), expected, "{verb}");
    }
    // The Codex fixture's one output is not an `exec_command` result, so it adds none.
    let codex = find(
        totals,
        &[("agent", json!("codex")), ("scope", json!("main"))],
    );
    assert_eq!(cost(codex), (json!(0), json!(0), json!(0)));
}

const CODEX_RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex-results");

/// A detector's report over the Codex results fixture.
fn codex_results(cmd: &str) -> Value {
    let (code, stdout, stderr) = run(&[
        cmd,
        "--codex-root",
        CODEX_RESULTS,
        "--agent",
        "codex",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn polls_read_codex_command_results() {
    let doc = codex_results("polls");
    let row = find(rows(&doc, "polls", 0), &[("scope", json!("main"))]);
    // `git status` exits 128, is polled (5 characters back) and polled again in a cell
    // that runs 2 commands but prints 1 result, so that result pairs with neither. A
    // later cell runs 2 test commands and also prints 1 result.
    assert_eq!(
        (
            row["commands"].clone(),
            row["polls"].clone(),
            row["polls_after_failure"].clone(),
            row["poll_chars"].clone(),
            row["poll_tokens_est"].clone()
        ),
        (json!(7), json!(2), json!(1), json!(5), json!(2))
    );
    let summary = &doc["detectors"]["polls"]["summary"];
    assert_eq!(summary["codex_outputs_not_paired"], 2);
    assert_eq!(summary["codex_commands_not_paired"], 4);
    // Every command counts by verb class, polled or not; only paired results add
    // characters: `git status` printed 20 and then 5, and 1 of 3 `cargo` runs printed 52.
    let results = rows(&doc, "polls", 2);
    let verb = |v: &str| {
        let r = find(results, &[("agent", json!("codex")), ("verb", json!(v))]);
        (
            r["commands"].clone(),
            r["results"].clone(),
            r["result_chars"].clone(),
            r["result_tokens_est"].clone(),
        )
    };
    assert_eq!(verb("cargo"), (json!(3), json!(1), json!(52), json!(13)));
    assert_eq!(verb("git"), (json!(4), json!(2), json!(25), json!(7)));
    assert_eq!(results[0]["verb"], "cargo", "largest results first");
}

#[test]
fn pipes_read_codex_test_runs_and_their_exit_codes() {
    let doc = codex_results("pipes");
    let pipes = &doc["detectors"]["pipes"];
    // One test run's cell prints its result; a cell of 2 test runs prints 1 result,
    // which pairs with neither and is counted as not paired.
    let expected = [
        ("test_runs", 3),
        ("unguarded", 3),
        ("unguarded_results_found", 1),
        ("unguarded_reported_success", 1),
        ("showed_a_failure_but_reported_success", 1),
    ];
    let codex = find(rows(&doc, "pipes", 0), &[("agent", json!("codex"))]);
    for (key, n) in expected {
        assert_eq!(pipes["summary"][key], n, "{key}");
        assert_eq!(codex[key], n, "{key}");
    }
    assert_eq!(pipes["summary"]["codex_outputs_not_paired"], 1);
}

#[test]
fn rereads_count_codex_output_characters() {
    // The re-read's output is a plain string, not a list of text items.
    let doc = codex_results("rereads");
    let row = find(rows(&doc, "rereads", 0), &[("scope", json!("main"))]);
    assert_eq!(
        (
            row["reads"].clone(),
            row["rereads"].clone(),
            row["reread_chars"].clone(),
            row["reread_tokens_est"].clone()
        ),
        (json!(2), json!(1), json!(5), json!(2))
    );
}

const CODEX_WRAPPED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex-wrapped");

#[test]
fn codex_results_printed_inside_lists_and_objects_are_paired() {
    // Cells print allSettled entries, one list of them, results under 2 keys of an
    // object, a result still running and a result's output text alone.
    let args = |json: bool| {
        let mut a = vec![
            "all",
            "--codex-root",
            CODEX_WRAPPED,
            "--agent",
            "codex",
            "--since",
            SINCE,
        ];
        if json {
            a.push("--json");
        }
        a
    };
    let (code, stdout, stderr) = run(&args(true));
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let row = find(rows(&doc, "polls", 0), &[("scope", json!("main"))]);
    let keys = [
        "commands",
        "polls",
        "polls_after_failure",
        "poll_chars",
        "poll_tokens_est",
    ];
    let got: Vec<Value> = keys.iter().map(|k| row[*k].clone()).collect();
    assert_eq!(got, [json!(9), json!(4), json!(1), json!(22), json!(6)]);
    let polls = &doc["detectors"]["polls"]["summary"];
    assert_eq!(polls["codex_outputs_not_paired"], 3);
    assert_eq!(polls["codex_commands_not_paired"], 4);
    let pipes = &doc["detectors"]["pipes"]["summary"];
    for (key, n) in [
        ("unguarded", 1),
        ("unguarded_results_found", 1),
        ("showed_a_failure_but_reported_success", 1),
        ("codex_outputs_not_paired", 0),
    ] {
        assert_eq!(pipes[key], n, "{key}");
    }

    let (code, stdout, stderr) = run(&args(false));
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("codex commands not paired: 4"), "{stdout}");
}

#[test]
fn codex_record_shapes_are_counted_or_reported_as_not_paired() {
    // The key sets, item types and result shapes of real Codex rollouts, with
    // synthetic values: null outputs, string outputs, images, missing call ids, a
    // cell that prints only an exit code, and runs before and in the window.
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex-shapes");
    let (code, stdout, stderr) = run(&[
        "all",
        "--codex-root",
        root,
        "--agent",
        "codex",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let not_paired = |d: &str| doc["detectors"][d]["summary"]["codex_outputs_not_paired"].clone();

    let polls = find(rows(&doc, "polls", 0), &[("scope", json!("main"))]);
    let keys = ["commands", "polls", "polls_after_failure", "poll_chars"];
    let got: Vec<Value> = keys.iter().map(|k| polls[*k].clone()).collect();
    assert_eq!(got, [json!(9), json!(6), json!(1), json!(40)]);
    assert_eq!(not_paired("polls"), 3);

    let pipes = &doc["detectors"]["pipes"]["summary"];
    for (key, n) in [
        ("test_runs", 3),
        ("unguarded", 3),
        ("unguarded_results_found", 1),
        ("showed_a_failure_but_reported_success", 1),
    ] {
        assert_eq!(pipes[key], n, "{key}");
    }
    assert_eq!(not_paired("pipes"), 2);

    let rereads = find(rows(&doc, "rereads", 0), &[("scope", json!("main"))]);
    assert_eq!(
        (rereads["rereads"].clone(), rereads["reread_chars"].clone()),
        (json!(1), json!(55))
    );
    assert_eq!(not_paired("rereads"), 0);
}

const RESULTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/results/claude");

/// The main-scope row of a report over the result-pairing fixture.
fn results_row(cmd: &str) -> Value {
    let (code, stdout, stderr) = run(&[
        cmd,
        "--claude-root",
        RESULTS,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    find(rows(&doc, cmd, 0), &[("scope", json!("main"))]).clone()
}

#[test]
fn polls_pair_results_by_id_within_one_session_and_the_window() {
    let row = results_row("polls");
    // After a failure: the first run in the window, whose last run failed before it, and
    // its repeat. Not the rerun after an edit, the poll after the fixed run, 2 polls
    // issued before either result, or the next session's poll.
    assert_eq!(row["polls_after_failure"], 2);
    // 22 + 14 + 12 + 10 + 11 in one session and 11 in the next. Not the 13 of a poll
    // before the window, a first run's or a rerun's result, or a result in the second
    // session that carries an id from the first.
    assert_eq!(row["poll_chars"], 80);
    assert_eq!(
        (
            row["commands"].clone(),
            row["polls"].clone(),
            row["reruns_after_edit"].clone()
        ),
        (json!(11), json!(7), json!(1))
    );
}

#[test]
fn rereads_count_a_call_once_and_only_in_the_window() {
    let row = results_row("rereads");
    // One `cat` re-reads 2 files and returns 18 characters. A re-read before the window
    // adds nothing, though its result is inside it, and nor does a later read.
    assert_eq!(
        (
            row["reads"].clone(),
            row["rereads"].clone(),
            row["reread_chars"].clone()
        ),
        (json!(3), json!(2), json!(18))
    );
}

#[test]
fn rereads_count_files_read_again_after_compaction() {
    let doc = report("rereads", &["--paths"]);
    let r = rows(&doc, "rereads", 0);
    let claude = find(r, &[("agent", json!("claude")), ("scope", json!("main"))]);
    assert_eq!(claude["compactions"], 1);
    assert_eq!(claude["reads"], 6);
    assert_eq!(claude["rereads"], 2);
    assert_eq!(claude["reread_share"], 0.33);
    // The `Read` re-read returns 17 and 3 characters of text around an image, and the
    // `tail` re-read 13. The first reads and a second read since the compaction add none.
    assert_eq!(claude["reread_chars"], 33);
    assert_eq!(claude["reread_tokens_est"], 9);
    let codex = find(r, &[("agent", json!("codex")), ("scope", json!("main"))]);
    assert_eq!(
        (codex["reads"].clone(), codex["rereads"].clone()),
        (json!(3), json!(1))
    );
    // Its re-read's call id has no output after it.
    assert_eq!(codex["reread_chars"], 0);
    assert_eq!(codex["reread_tokens_est"], 0);
    let paths = rows(&doc, "rereads", 1);
    let names: Vec<&str> = paths
        .iter()
        .map(|p| p["path"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["c:/work/a.md", "c:/work/b.md", "c:/work/x.md"]);
}

#[test]
fn rereads_hide_paths_without_the_flag() {
    let doc = report("rereads", &[]);
    assert_eq!(
        doc["detectors"]["rereads"]["tables"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn hooks_rank_cancellations_first_and_skip_old_runs() {
    let doc = report("hooks", &[]);
    let r = rows(&doc, "hooks", 0);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0]["hook"], "rtk");
    assert_eq!(
        (r[0]["cancelled"].clone(), r[0]["timed_out"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(find(r, &[("hook", json!("guard.exe"))])["median_ms"], 40);
    let digest = find(r, &[("hook", json!("hub-digest.js"))]);
    assert_eq!(
        (
            digest["max_ms"].clone(),
            digest["median_output_chars"].clone()
        ),
        (json!(120), json!(19))
    );
}

#[test]
fn all_reports_each_root_and_counts_the_partial_last_line_once() {
    let doc = report("all", &[]);
    let roots = doc["roots"].as_array().expect("roots");
    assert_eq!(roots[0]["agent"], "claude");
    assert_eq!(roots[0]["files"], 3);
    assert_eq!(roots[0]["lines_skipped"], 1);
    assert_eq!(roots[1]["files"], 2);
    assert_eq!(doc["schema_version"], 2);
    assert_eq!(doc["tool"], "bigsister");
    assert_eq!(doc["detectors"].as_object().map(|o| o.len()), Some(10));
}

/// `bigsister all` over the Claude transcripts in a fixture folder, which must hold 2.
fn claude_all(fixture: &str) -> Value {
    let root = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let (code, stdout, stderr) = run(&[
        "all",
        "--agent",
        "claude",
        "--claude-root",
        &root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(root_counts(&doc)[0], &json!(2), "both files were read");
    doc
}

fn claude_polls(doc: &Value) -> (Value, Value) {
    let row = find(
        rows(doc, "polls", 0),
        &[("agent", json!("claude")), ("scope", json!("main"))],
    );
    (row["commands"].clone(), row["polls"].clone())
}

#[test]
fn records_a_resumed_session_copies_from_another_file_count_once() {
    // s-second resumes s-first: it starts with copies of s-first's 3 records, which keep
    // their uuids and times, then runs the same test again.
    let doc = claude_all("resumed");
    assert_eq!(doc["detectors"]["startup"]["summary"]["sessions"], 1);
    assert_eq!(doc["detectors"]["pipes"]["summary"]["test_runs"], 2);
    assert_eq!(rows(&doc, "hooks", 0)[0]["runs"], 1);
    // The copied run still counts as the run the new one repeats.
    assert_eq!(claude_polls(&doc), (json!(2), json!(1)));
}

#[test]
fn a_copy_of_part_of_a_session_counts_once_whichever_path_sorts_first() {
    // s-a copies the last 3 records of s-b, from its compaction on, then runs the test
    // again. s-a sorts first by path but s-b is read first: each later run polls the one
    // before, which the copy alone never saw.
    let doc = claude_all("resumed-slice");
    assert_eq!(claude_polls(&doc), (json!(3), json!(2)));
}

#[test]
fn a_copied_reply_becomes_cache_s_previous_reply_but_adds_no_writes_or_dollars() {
    // s-second resumes s-first: copies of s-first's prompt and reply (1,000,000 written),
    // then its own reply 2 minutes later, which reads nothing and writes 1,200,000.
    let doc = claude_all("resumed-cache");
    let r = rows(&doc, "cache", 0);
    let row = |cause: &str| {
        let row = find(r, &[("scope", json!("main")), ("cause", json!(cause))]);
        (
            row["replies"].clone(),
            row["write_tokens"].clone(),
            row["write_usd_est"].clone(),
        )
    };
    // Only s-first's reply is a first reply; the copy adds nothing. At 1.25 times
    // Opus 5.5's US$4 input price, a 5-minute write costs US$5 a million.
    assert_eq!(row("first reply"), (json!(1), json!(1_000_000), json!(5.0)));
    // s-second's reply rebuilds what the copied reply left cached, inside its lifetime.
    assert_eq!(
        row("rebuilt otherwise"),
        (json!(1), json!(1_000_000), json!(5.0))
    );
    assert_eq!(row("new context"), (json!(1), json!(200_000), json!(1.0)));
    assert_eq!(r.len(), 3, "{r:?}");
    let s = &doc["detectors"]["cache"]["summary"];
    assert_eq!(s["sessions"], 2);
    assert_eq!(s["write_tokens"], 2_200_000);
}

#[test]
fn startup_skips_copied_records_after_a_resumed_session_starts() {
    // s-b resumes s-a: a new hook runs, then come copies of s-a's hook and reply, then
    // s-b's own first reply, which records no tokens.
    let doc = claude_all("resumed-startup");
    let s = &doc["detectors"]["startup"]["summary"];
    assert_eq!(s["sessions"], 2);
    assert_eq!(s["sessions_with_request_tokens"], 1);
    let r = rows(&doc, "startup", 0);
    for piece in ["hook: rtk", "hook: agent-heavy"] {
        assert_eq!(
            find(r, &[("piece", json!(piece))])["sessions"],
            1,
            "{piece}"
        );
    }
}

#[test]
fn codex_default_reads_archived_sessions_and_counts_compressed_files() {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("home-archived");
    let _ = std::fs::remove_dir_all(&home);
    let rollout = std::fs::read(format!("{CODEX}/2026/11/08/rollout-main.jsonl")).expect("fixture");
    for (dir, name) in [
        ("sessions/2026/11/08", "rollout-live.jsonl"),
        ("archived_sessions", "rollout-archived.jsonl"),
        ("archived_sessions", "rollout-packed.jsonl.zst"),
    ] {
        let dir = home.join(".codex").join(dir);
        std::fs::create_dir_all(&dir).expect("folder");
        std::fs::write(dir.join(name), &rollout).expect("transcript");
    }
    let (code, stdout, stderr) = run_env(
        &["polls", "--agent", "codex", "--since", SINCE, "--json"],
        &[("HOME", &home), ("USERPROFILE", &home)],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let roots = doc["roots"].as_array().expect("roots");
    let counts: Vec<(&str, &Value, &Value)> = roots
        .iter()
        .map(|r| {
            let root = r["root"].as_str().expect("root");
            (
                root.rsplit('/').next().unwrap_or(root),
                &r["files"],
                &r["compressed_skipped"],
            )
        })
        .collect();
    assert_eq!(
        counts,
        [
            ("sessions", &json!(1), &json!(0)),
            ("archived_sessions", &json!(1), &json!(1)),
        ]
    );
}

/// `rollout-main.jsonl` compressed by the zstd 1.5.7 command line tool with `--check`.
const ZST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/zst/rollout-main.jsonl.zst"
);

/// Runs `polls` for Codex over one folder holding the given files.
fn codex_polls(name: &str, files: &[(&str, &[u8])]) -> Value {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    for (file, bytes) in files {
        std::fs::write(dir.join(file), bytes).expect("transcript");
    }
    let root = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--agent",
        "codex",
        "--codex-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn root_counts(doc: &Value) -> [&Value; 4] {
    let r = &doc["roots"][0];
    [
        &r["files"],
        &r["unreadable"],
        &r["lines_skipped"],
        &r["compressed_skipped"],
    ]
}

#[test]
fn codex_reads_compressed_rollouts_as_it_reads_plain_ones() {
    let plain = std::fs::read(format!("{CODEX}/2026/11/08/rollout-main.jsonl")).expect("fixture");
    let packed = std::fs::read(ZST).expect("fixture");
    let from_plain = codex_polls("zst-plain", &[("rollout.jsonl", &plain)]);
    let from_packed = codex_polls("zst-packed", &[("rollout.jsonl.zst", &packed)]);
    assert_eq!(
        root_counts(&from_packed),
        [&json!(1), &json!(0), &json!(0), &json!(0)]
    );
    assert_eq!(
        from_packed["detectors"]["polls"],
        from_plain["detectors"]["polls"]
    );
    assert!(
        rows(&from_packed, "polls", 0)
            .iter()
            .any(|r| r["polls"] != 0)
    );
}

#[test]
fn codex_counts_every_compressed_rollout_it_cannot_decode() {
    let packed = std::fs::read(ZST).expect("fixture");
    let mut bad_sum = packed.clone();
    *bad_sum.last_mut().expect("checksum") ^= 0xff;
    let two_frames = [packed.as_slice(), packed.as_slice()].concat();
    let trailing_byte = [packed.as_slice(), b"\0"].concat();
    // Frame headers asking for a 256 MiB window, and for one 1 GiB segment, above
    // the decoder's 128 MiB limit.
    let magic = [0x28, 0xb5, 0x2f, 0xfd];
    let big_window = [&magic[..], &[0x00, 0x90]].concat();
    let big_segment = [&magic[..], &[0xa0, 0, 0, 0, 0x40]].concat();
    let doc = codex_polls(
        "zst-bad",
        &[
            ("cut.jsonl.zst", &packed[..packed.len() / 2]),
            ("head.jsonl.zst", &packed[..5]),
            ("sum.jsonl.zst", &bad_sum),
            ("two.jsonl.zst", &two_frames),
            ("byte.jsonl.zst", &trailing_byte),
            ("window.jsonl.zst", &big_window),
            ("segment.jsonl.zst", &big_segment),
            ("text.jsonl.zst", b"{\"type\":\"session_meta\"}\n"),
            ("short.jsonl.zst", &magic[..2]),
        ],
    );
    assert_eq!(
        root_counts(&doc),
        [&json!(7), &json!(7), &json!(0), &json!(2)]
    );
    // A decoder's message can quote the data, so it is named only as such.
    assert_eq!(
        doc["roots"][0]["unreadable_reasons"],
        json!([
            {"reason": "its zstd data cannot be decoded (damaged, cut short or more than one frame)", "count": 4},
            {"reason": "its zstd frame header cannot be read", "count": 3},
        ])
    );
}

#[cfg(unix)]
#[test]
fn codex_counts_a_compressed_rollout_whose_read_fails_as_unreadable() {
    // A link to a folder opens, but reading the magic number from it fails.
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("zst-read-error");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    let link = dir.join("rollout.jsonl.zst");
    std::os::unix::fs::symlink(env!("CARGO_TARGET_TMPDIR"), link).expect("link");
    let root = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--agent",
        "codex",
        "--codex-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        root_counts(&doc),
        [&json!(1), &json!(1), &json!(0), &json!(0)]
    );
}

/// A zstd frame with a 1 MiB window and no checksum that decodes to a line of
/// `a` 128 KiB longer than the 64 MiB line cap, then `rest`, of at most 128 KiB.
fn frame_with_long_line(rest: &[u8]) -> Vec<u8> {
    let block = |size: usize, kind: u32, last: bool| {
        let header =
            (u32::try_from(size).expect("block size") << 3) | (kind << 1) | u32::from(last);
        header.to_le_bytes()[..3].to_vec()
    };
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x50];
    for _ in 0..513 {
        // An RLE block: 128 KiB of one byte.
        frame.extend(block(128 << 10, 1, false));
        frame.push(b'a');
    }
    frame.extend(block(rest.len(), 0, true));
    frame.extend_from_slice(rest);
    frame
}

#[test]
fn codex_skips_a_decoded_line_over_the_cap_and_reads_the_rest() {
    let plain = std::fs::read(format!("{CODEX}/2026/11/08/rollout-main.jsonl")).expect("fixture");
    let packed = frame_with_long_line(&[b"\n", plain.as_slice()].concat());
    let from_plain = codex_polls("zst-long-plain", &[("rollout.jsonl", &plain)]);
    let from_packed = codex_polls("zst-long", &[("rollout.jsonl.zst", &packed)]);
    assert_eq!(
        root_counts(&from_packed),
        [&json!(1), &json!(0), &json!(1), &json!(0)]
    );
    assert_eq!(
        from_packed["detectors"]["polls"],
        from_plain["detectors"]["polls"]
    );
}

/// Runs `all` over two roots with each output flag and checks that no sentinel
/// appears in any case: verb classes and paths are lower-cased.
fn assert_no_sentinel(claude: &str, codex: &str) {
    for extra in [
        &[][..],
        &["--json"],
        &["--paths"],
        &["--json", "--paths"],
        &["--csv", "--paths"],
    ] {
        let mut args = vec![
            "all",
            "--claude-root",
            claude,
            "--codex-root",
            codex,
            "--since",
            SINCE,
        ];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{stderr}");
        assert!(
            !stdout.to_lowercase().contains("sentinel"),
            "content leaked with {extra:?}:\n{stdout}"
        );
        assert!(!stderr.to_lowercase().contains("sentinel"), "{stderr}");
    }
}

#[test]
fn output_never_contains_transcript_content() {
    assert_no_sentinel(CLAUDE, CODEX);
    assert_no_sentinel(RESULTS, CODEX);
    assert_no_sentinel(RESULTS, CODEX_RESULTS);
    assert_no_sentinel(RESULTS, CODEX_WRAPPED);
    assert_no_sentinel(KINDS, CODEX);
    assert_no_sentinel(SKILLS, CODEX);
    assert_no_sentinel(CACHE, CODEX);
    assert_no_sentinel(MODELS, CODEX);
    assert_no_sentinel(MODELS_TIERS, CODEX);
    assert_no_sentinel(CONTEXT, CODEX);
}

/// One main session with the attachment kinds, hook errors and pipe result the main
/// fixtures lack.
const KINDS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/kinds/claude");

fn kinds(cmd: &str) -> Value {
    let (code, stdout, stderr) = run(&[
        cmd,
        "--agent",
        "claude",
        "--claude-root",
        KINDS,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn startup_sizes_the_other_attachment_kinds() {
    let doc = kinds("startup");
    let r = rows(&doc, "startup", 0);
    for (piece, chars) in [
        ("agent listing", 7),
        ("system prompt", 4),
        ("session context", 9),
        ("environment", 10),
        ("attachment: custom_thing", 29),
    ] {
        assert_eq!(
            find(r, &[("piece", json!(piece))])["median_chars"],
            chars,
            "{piece}"
        );
    }
    assert_eq!(
        r.len(),
        5,
        "hook errors after the first reply are not startup"
    );
}

#[test]
fn hooks_count_both_error_kinds() {
    let doc = kinds("hooks");
    let r = rows(&doc, "hooks", 0);
    assert_eq!(r.len(), 2);
    for (hook, ms) in [("guard.exe", 50), ("hub-digest.js", 30)] {
        let row = find(r, &[("hook", json!(hook))]);
        assert_eq!(
            (
                row["runs"].clone(),
                row["errors"].clone(),
                row["cancelled"].clone(),
                row["median_ms"].clone()
            ),
            (json!(1), json!(1), json!(0), json!(ms)),
            "{hook}"
        );
    }
}

#[test]
fn pipes_count_a_result_with_no_pass_or_fail_line() {
    let doc = kinds("pipes");
    let s = &doc["detectors"]["pipes"]["summary"];
    assert_eq!(
        (
            s["unguarded_results_found"].clone(),
            s["showed_a_failure_but_reported_success"].clone(),
            s["showed_no_pass_or_fail_line"].clone()
        ),
        (json!(1), json!(0), json!(1))
    );
}

#[cfg(unix)]
#[test]
fn a_transcript_that_cannot_be_opened_counts_as_unreadable() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("unreadable");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    std::os::unix::fs::symlink(dir.join("missing"), dir.join("gone.jsonl")).expect("symlink");
    let root = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&["hooks", "--claude-root", root, "--since", SINCE, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["unreadable"], 1);
}

/// Runs `hooks --claude-root dir` as JSON and as text.
#[cfg(unix)]
fn claude_root_report(dir: &Path) -> (Value, String) {
    let root = dir.to_str().expect("UTF-8 path");
    let args = ["hooks", "--claude-root", root, "--since", SINCE];
    let (code, text, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, stdout, stderr) = run(&[&args[..], &["--json"]].concat());
    assert_eq!(code, 0, "stderr: {stderr}");
    (serde_json::from_str(&stdout).expect("valid JSON"), text)
}

#[cfg(unix)]
#[test]
fn each_reason_a_transcript_is_unreadable_prints_with_its_count() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("unreadable-reasons");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("folder");
    for name in ["a.jsonl", "b.jsonl"] {
        std::os::unix::fs::symlink(dir.join("missing"), dir.join(name)).expect("symlink");
    }
    // A link to a folder opens, but reading from it fails.
    std::os::unix::fs::symlink(env!("CARGO_TARGET_TMPDIR"), dir.join("c.jsonl")).expect("link");
    let (doc, text) = claude_root_report(&dir);
    let opened = "a file cannot be opened: No such file or directory (os error 2)";
    let read = "a file cannot be read: Is a directory (os error 21)";
    assert_eq!(
        root_counts(&doc),
        [&json!(3), &json!(3), &json!(0), &json!(0)]
    );
    assert_eq!(
        doc["roots"][0]["unreadable_reasons"],
        json!([{"reason": opened, "count": 2}, {"reason": read, "count": 1}])
    );
    assert!(
        text.contains(&format!(
            "\n  unreadable (2): {opened}\n  unreadable: {read}\n"
        )),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn a_folder_that_cannot_be_listed_prints_its_reason() {
    use std::os::unix::fs::PermissionsExt;
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("unreadable-folder");
    let locked = dir.join("locked");
    let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&locked).expect("folder");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("mode");
    if std::fs::read_dir(&locked).is_ok() {
        // Run as root, which reads any folder: there is nothing to test.
        return;
    }
    let (doc, text) = claude_root_report(&dir);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let reason = "a folder cannot be listed: Permission denied (os error 13)";
    assert_eq!(doc["roots"][0]["unreadable"], 1);
    assert_eq!(
        doc["roots"][0]["unreadable_reasons"],
        json!([{"reason": reason, "count": 1}])
    );
    assert!(
        text.contains(&format!("\n  unreadable: {reason}\n")),
        "{text}"
    );
}

#[test]
fn probe_inputs_never_reach_the_output() {
    let probe = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/probe");
    let (claude, codex) = (format!("{probe}/claude"), format!("{probe}/codex"));
    assert_no_sentinel(&claude, &codex);
    // The probe must reach the detectors, or the check above proves nothing.
    let (code, stdout, stderr) = run(&[
        "all",
        "--claude-root",
        &claude,
        "--codex-root",
        &codex,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let verbs = rows(&doc, "polls", 1);
    for (agent, verb) in [
        ("claude", "(assignment)"),
        ("claude", "ssh ls"),
        ("claude", "(other)"),
        ("codex", "(assignment)"),
        ("codex", "bash"),
    ] {
        find(verbs, &[("agent", json!(agent)), ("verb", json!(verb))]);
    }
    let hooks = rows(&doc, "hooks", 0);
    find(
        hooks,
        &[
            ("hook_name", json!("(unknown)")),
            ("hook", json!("(other)")),
        ],
    );
    find(
        hooks,
        &[("hook_name", json!("(unknown)")), ("hook", json!("node"))],
    );
}

#[test]
fn text_output_has_a_section_per_detector() {
    let (code, stdout, _) = run(&fixture_args("all", &["--limit", "1"]));
    assert_eq!(code, 0);
    for name in [
        "startup", "schemas", "pipes", "polls", "rereads", "tools", "cache", "models", "hooks",
    ] {
        assert!(stdout.contains(&format!("== {name}: ")), "{name}");
    }
    assert!(stdout.contains("more rows; raise --limit"));
}

#[test]
fn a_detector_without_its_agent_says_so() {
    let doc = report("hooks", &["--agent", "codex"]);
    let notes = doc["detectors"]["hooks"]["notes"]
        .as_array()
        .expect("notes");
    assert_eq!(
        notes[0],
        "Reads claude transcripts only, and none were scanned."
    );
}

#[test]
fn help_and_version_exit_zero() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("Usage: bigsister"));
    let (code, stdout, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim(),
        format!("bigsister {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn release_files_match_the_crate() {
    // The release workflow refuses a tag whose VERSION differs from Cargo.toml. Like
    // release-policy, accept one line ending, LF or CRLF (a Windows checkout gives CRLF).
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("VERSION"))
        .expect("VERSION");
    let version = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    // It also refuses one whose RELEASE_NOTES.md does not start with the tag's heading.
    let notes =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("RELEASE_NOTES.md"))
            .expect("RELEASE_NOTES.md");
    assert_eq!(
        notes.lines().next(),
        Some(format!("# v{}", env!("CARGO_PKG_VERSION")).as_str())
    );
}

#[test]
fn help_says_pipes_reads_codex_commands_too() {
    let (code, stdout, _) = run(&["--help"]);
    assert_eq!(code, 0);
    let help = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        help.contains("a pipe into tail or head hides (Claude, Codex, pi, OpenCode, Gemini, Qwen)"),
        "{stdout}"
    );
}

#[test]
fn usage_errors_exit_two() {
    assert_eq!(run(&[]).0, 2);
    assert_eq!(run(&["polls", "--no-such-flag"]).0, 2);
    assert_eq!(run(&["polls", "--agent", "hermes"]).0, 2);
}

#[test]
fn domain_errors_exit_one_with_a_message() {
    let (code, _, stderr) = run(&["polls", "--claude-root", CLAUDE, "--since", "last week"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("--since"), "{stderr}");
    let (code, _, stderr) = run(&[
        "polls",
        "--claude-root",
        "no/such/folder",
        "--agent",
        "claude",
    ]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("claude transcript folder not found"),
        "{stderr}"
    );
}

#[test]
fn an_early_since_is_an_error_not_a_panic() {
    for since in ["1500-01-01", "200000d"] {
        let (code, _, stderr) = run(&["hooks", "--claude-root", CLAUDE, "--since", since]);
        assert_eq!(code, 1, "{since}: {stderr}");
        assert!(stderr.contains("--since"), "{stderr}");
    }
}

#[test]
fn a_far_future_since_finds_nothing() {
    let (code, stdout, stderr) = run(&[
        "polls",
        "--claude-root",
        CLAUDE,
        "--since",
        "9999-12-31",
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], 0);
}

#[test]
fn folders_for_agents_no_chosen_detector_reads_are_not_checked() {
    let (code, _, stderr) = run(&[
        "hooks",
        "--claude-root",
        CLAUDE,
        "--codex-root",
        "no/such/folder",
        "--since",
        SINCE,
    ]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn hooks_without_output_show_null() {
    let doc = report("hooks", &[]);
    let rtk = find(rows(&doc, "hooks", 0), &[("hook", json!("rtk"))]);
    assert_eq!(rtk["median_output_chars"], Value::Null);
}

#[test]
fn paths_under_home_start_with_a_tilde() {
    let home = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");
    let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
        .args([
            "hooks",
            "--agent",
            "claude",
            "--claude-root",
            CLAUDE,
            "--since",
            SINCE,
            "--json",
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .expect("binary runs");
    assert!(out.status.success());
    let doc: Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["root"], "~/fixtures/claude");
}

#[test]
fn files_modified_before_the_window_are_not_read() {
    let dir = std::env::temp_dir().join(format!("bigsister-mtime-{}", std::process::id()));
    let file = dir.join("proj").join("s.jsonl");
    std::fs::create_dir_all(file.parent().expect("parent")).expect("temp folder");
    std::fs::copy(format!("{CLAUDE}/proj-a/s-main.jsonl"), &file).expect("copy fixture");
    let october_2025 = UNIX_EPOCH + Duration::from_secs(1_760_000_000);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .and_then(|f| f.set_modified(october_2025))
        .expect("set the modification time");
    let root = dir.to_str().expect("UTF-8 path").to_owned();
    let (code, stdout, stderr) = run(&[
        "hooks",
        "--agent",
        "claude",
        "--claude-root",
        &root,
        "--since",
        SINCE,
        "--json",
    ]);
    std::fs::remove_dir_all(&dir).expect("remove the temporary folder");
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], 0);
}

#[test]
fn roots_no_detector_reads_are_left_out_and_cut_lines_still_count() {
    let doc = report("hooks", &[]);
    let roots = doc["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["agent"], "claude");
    assert_eq!(roots[0]["lines_skipped"], 1);
}

#[test]
fn a_rate_prices_each_token_estimate_and_nothing_else() {
    let plain = report("all", &[]);
    let priced = report("all", &["--usd-per-mtok", "1000"]);
    for (detector, table, tokens, usd) in [
        ("polls", 0, "poll_tokens_est", "poll_usd_est"),
        ("polls", 2, "result_tokens_est", "result_usd_est"),
        ("rereads", 0, "reread_tokens_est", "reread_usd_est"),
        ("schemas", 0, "idle_tokens_est", "idle_usd_est"),
    ] {
        let rows = rows(&priced, detector, table);
        assert!(!rows.is_empty(), "{detector}");
        for row in rows {
            let want = (row[tokens].as_f64().unwrap() * 1000.0 / 1e6 * 100.0).round() / 100.0;
            assert_eq!(row[usd].as_f64(), Some(want), "{detector} {row}");
        }
        // Without a rate, the fixtures name no model, so nothing is priced.
        let plain_rows: Vec<Value> = rows_of(&plain, detector)
            .into_iter()
            .filter(|r| r.get(usd).is_some())
            .collect();
        assert!(!plain_rows.is_empty(), "{detector} {usd}");
        assert!(
            plain_rows
                .iter()
                .all(|r| r[usd] == json!(0.0) && r["unpriced_tokens_est"] == r[tokens])
        );
        let notes = priced["detectors"][detector]["notes"].to_string();
        assert!(notes.contains("not a bill"), "{detector} {notes}");
    }
    // Startup's measured request tokens include cache reads, so they stay unpriced.
    assert_eq!(
        priced["detectors"]["startup"],
        plain["detectors"]["startup"]
    );

    let (code, text, stderr) = run(&fixture_args("polls", &["--usd-per-mtok", "3"]));
    assert_eq!(code, 0);
    assert!(text.contains("poll usd est"), "{text}");
    // The option is global, so it may also come before the subcommand.
    let mut first = vec!["--usd-per-mtok", "3"];
    first.extend(fixture_args("polls", &[]));
    assert_eq!(run(&first), (0, text, stderr));
    for bad in ["0", "-1", "NaN", "inf", "1001", "x"] {
        let (code, _, stderr) = run(&fixture_args("polls", &["--usd-per-mtok", bad]));
        assert_eq!(code, 2, "{bad}: {stderr}");
    }
}

/// Every row of every table of a detector.
fn rows_of(doc: &Value, detector: &str) -> Vec<Value> {
    doc["detectors"][detector]["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .flat_map(|t| t["rows"].as_array().expect("rows").clone())
        .collect()
}

const PRICED_CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/priced/claude");
const PRICED_CODEX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/priced/codex");
const PRICED_MIXED: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/priced-mixed/claude"
);

/// `bigsister all --json` over the priced Codex fixtures and `claude`, with `extra` options.
fn priced(claude: &str, extra: &[&str]) -> Value {
    let mut args = vec![
        "all",
        "--claude-root",
        claude,
        "--codex-root",
        PRICED_CODEX,
        "--since",
        SINCE,
        "--json",
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn each_estimate_is_priced_at_the_model_that_read_it() {
    let doc = priced(PRICED_CLAUDE, &[]);
    let polls = rows(&doc, "polls", 0);
    // Claude: 3 polls of 10,000 characters. The first is read by Claude Fable 5.2 at
    // US$8 per million; the second only by a synthetic reply and a model with no
    // bundled price; the third by no reply at all.
    let claude = find(polls, &[("agent", json!("claude"))]);
    assert_eq!(
        [&claude["poll_chars"], &claude["poll_tokens_est"]],
        [&json!(30000), &json!(7500)]
    );
    assert_eq!(claude["poll_usd_est"], json!(0.02));
    assert_eq!(claude["unpriced_tokens_est"], json!(5000));
    // Codex: 3 polls of 20,000 characters. Only the one in the GPT-6.1 Sol turn is
    // priced (US$2 per million); the others come before any turn and in a turn whose
    // model has no bundled price.
    let codex = find(polls, &[("agent", json!("codex"))]);
    assert_eq!(codex["poll_tokens_est"], json!(15000));
    assert_eq!(codex["poll_usd_est"], json!(0.01));
    assert_eq!(codex["unpriced_tokens_est"], json!(10000));
    // Schemas: the idle server is read last by Claude Opus 5.5 at US$4 per million.
    let idle = find(rows(&doc, "schemas", 0), &[("tool", json!("mcp__idle__*"))]);
    let chars = idle["idle_chars"].as_f64().expect("idle chars");
    assert!(chars > 19_000.0, "{idle}");
    assert_eq!(
        idle["idle_usd_est"].as_f64(),
        Some((chars / 4.0 * 4.0 / 1e6 * 100.0).round() / 100.0)
    );
    assert_eq!(idle["unpriced_tokens_est"], json!(0));
    // Rereads: a 40,000-character re-read read by Claude Sonnet 5.5 at US$2 per million.
    let reread = find(rows(&doc, "rereads", 0), &[("agent", json!("claude"))]);
    assert_eq!(reread["reread_chars"], json!(40000));
    assert_eq!(reread["reread_usd_est"], json!(0.02));
    assert_eq!(reread["unpriced_tokens_est"], json!(0));
    let notes = doc["detectors"]["polls"]["notes"].to_string();
    assert!(
        notes.contains("16 November 2026") && notes.contains("not a bill"),
        "{notes}"
    );
    assert_eq!(doc["schema_version"], 2);
}

/// Appends to a Codex transcript a call running `git status` that prints `printed`
/// characters and, when `read_in` gives a prompt size, the usage record of the model
/// call that read its output: Codex writes a call's usage as the call completes.
fn codex_git_status(lines: &mut Vec<Value>, call: &str, printed: usize, read_in: Option<u64>) {
    lines.push(
        json!({"timestamp": "2026-11-09T04:01:00.000Z", "type": "response_item",
        "payload": {"type": "custom_tool_call", "name": "exec", "call_id": call,
            "input": "text(await tools.exec_command({cmd: \"git status\"}));"}}),
    );
    let chunk = json!({"chunk_id": "k", "exit_code": 0, "original_token_count": 1,
        "output": "y".repeat(printed), "wall_time_seconds": 0.5});
    lines.push(
        json!({"timestamp": "2026-11-09T04:02:00.000Z", "type": "response_item",
        "payload": {"type": "custom_tool_call_output", "call_id": call, "output": [
            {"type": "input_text", "text": "Script completed\nWall time: 0.5 seconds\nOutput:\n"},
            {"type": "input_text", "text": chunk.to_string()}]}}),
    );
    if let Some(tokens) = read_in {
        lines.push(json!({"timestamp": "2026-11-09T04:03:00.000Z",
            "type": "token_usage_record", "payload": {"turn_id": "t", "response_id": call,
            "usage": {"input_tokens": tokens, "cached_input_tokens": 0, "output_tokens": 10}}}));
    }
}

/// A Codex `turn_context` naming `model`.
fn codex_turn(model: &str) -> Value {
    json!({"timestamp": "2026-11-09T04:00:30.000Z", "type": "turn_context",
           "payload": {"turn_id": "t", "cwd": "/work", "model": model}})
}

#[test]
fn codex_results_are_priced_by_the_prompt_of_the_call_that_read_them() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codex-prompt-tiers");
    let _ = std::fs::remove_dir_all(&dir);
    let day = dir.join("2026/11/09");
    std::fs::create_dir_all(&day).expect("folder");
    let mut lines = vec![
        json!({"timestamp": "2026-11-09T04:00:00.000Z", "type": "session_meta",
               "payload": {"id": "c-tiers", "cwd": "/work"}}),
        codex_turn("gpt-6.1-sol"),
    ];
    // `git status` once, then 4 polls of it. GPT-6.1 Sol costs US$2 per million up to
    // 272,000 prompt tokens and US$4 above.
    codex_git_status(&mut lines, "c0", 4, Some(20_000));
    codex_git_status(&mut lines, "c1", 80_000, Some(272_000));
    codex_git_status(&mut lines, "c2", 80_000, Some(272_001));
    // No call reads this one before the turn ends, so its prompt size is not known.
    codex_git_status(&mut lines, "c3", 40_000, None);
    // GPT-5.3 Codex has one price, so a result no recorded call follows is still priced.
    lines.push(codex_turn("gpt-5.3-codex"));
    codex_git_status(&mut lines, "c4", 40_000, None);
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(day.join("rollout-tiers.jsonl"), text).expect("transcript");
    let codex = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--codex-root",
        codex,
        "--claude-root",
        CLAUDE,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let row = find(rows(&doc, "polls", 0), &[("agent", json!("codex"))]);
    assert_eq!(row["polls"], json!(4), "{row}");
    // 20,000 tokens est at US$2, 20,000 at US$4, 10,000 unpriced and 10,000 at US$1.75:
    // US$0.04 + 0.08 + 0.0175. At one price per model, US$0.1175 and none unpriced.
    assert_eq!(row["poll_usd_est"], json!(0.14), "{row}");
    assert_eq!(row["unpriced_tokens_est"], json!(10000), "{row}");
}

#[test]
fn a_codex_result_is_priced_once_and_one_left_at_the_end_stays_unpriced() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("codex-prompt-once");
    let _ = std::fs::remove_dir_all(&dir);
    let day = dir.join("2026/11/09");
    std::fs::create_dir_all(&day).expect("folder");
    let usage = |tokens: u64| {
        json!({"timestamp": "2026-11-09T04:04:00.000Z", "type": "token_usage_record",
            "payload": {"turn_id": "t", "response_id": "later",
            "usage": {"input_tokens": tokens, "cached_input_tokens": 0, "output_tokens": 10}}})
    };
    let mut lines = vec![
        json!({"timestamp": "2026-11-09T04:00:00.000Z", "type": "session_meta",
               "payload": {"id": "c-once", "cwd": "/work"}}),
        codex_turn("gpt-6.1-sol"),
    ];
    codex_git_status(&mut lines, "c0", 4, Some(20_000));
    // Both read at one 20,000-token prompt; the later, larger calls and the next turn's
    // call read them again but price them no more.
    codex_git_status(&mut lines, "c1", 80_000, None);
    codex_git_status(&mut lines, "c1b", 80_000, Some(20_000));
    lines.push(usage(272_001));
    lines.push(usage(272_001));
    lines.push(codex_turn("gpt-6.1-sol"));
    lines.push(usage(272_001));
    // No call reads this one before the file ends, so its prompt size is not known.
    codex_git_status(&mut lines, "c2", 80_000, None);
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(day.join("rollout-once.jsonl"), text).expect("transcript");
    let codex = dir.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--codex-root",
        codex,
        "--claude-root",
        CLAUDE,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let row = find(rows(&doc, "polls", 0), &[("agent", json!("codex"))]);
    assert_eq!(row["polls"], json!(3), "{row}");
    // 40,000 tokens est at US$2, once, and 20,000 unpriced.
    assert_eq!(row["poll_usd_est"], json!(0.08), "{row}");
    assert_eq!(row["unpriced_tokens_est"], json!(20000), "{row}");
}

/// A Claude transcript of `calls`, each a tool name, its input, its result's content and
/// whether the result is an error; each call is made by a reply of Claude Opus 5.5, which
/// reads the result before it.
fn tool_calls(name: &str, calls: &[(&str, Value, Value, bool)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).expect("folder");
    let text: String = tool_call_records("", 0, calls).concat();
    std::fs::write(dir.join("proj").join("s.jsonl"), text).expect("transcript");
    dir
}

/// The records of `calls`, as `tool_calls` writes them, 2 lines for each call: ids start
/// with `tag` and times `minute` minutes after 05:00 on 9 November 2026.
fn tool_call_records(
    tag: &str,
    minute: usize,
    calls: &[(&str, Value, Value, bool)],
) -> Vec<String> {
    let mut out = Vec::new();
    for (i, (tool, input, content, error)) in calls.iter().enumerate() {
        let time = format!(
            "2026-11-09T05:{:02}:{:02}.000Z",
            minute + i / 30,
            i % 30 * 2
        );
        // Written as text: Claude Code puts `model` first in a reply's message, and
        // bigsister finds a reply by that.
        let use_block = json!({"type": "tool_use", "id": format!("{tag}t{i}"), "name": tool,
                               "input": input});
        let result = json!({"type": "user", "uuid": format!("{tag}u{i}"), "timestamp": time,
            "message": {"role": "user", "content": [{"type": "tool_result",
            "tool_use_id": format!("{tag}t{i}"), "content": content, "is_error": error}]}});
        out.push(format!(
            "{{\"type\":\"assistant\",\"uuid\":\"{tag}a{i}\",\"timestamp\":\"{time}\",\
             \"message\":{{\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\
             \"content\":[{use_block}]}}}}\n{result}\n"
        ));
    }
    out
}

#[test]
fn a_resumed_sessions_copied_errors_extend_no_cascade_or_churn() {
    let fail = || ("Bash", json!({"command": "false"}), json!("boom"), true);
    let first = tool_call_records(
        "",
        0,
        &[
            ("Bash", json!({"command": "true"}), json!(""), false),
            fail(),
            fail(),
            fail(),
        ],
    );
    let dir = tool_calls("tool-calls-resumed", &[]);
    std::fs::write(dir.join("proj").join("s.jsonl"), first.concat()).expect("first");
    // The resumed session starts with copies of the 3 errors, then errs once more: counted,
    // the copies would make a second cascade of 4 and 2 more churn calls.
    let resumed = first[1..].concat() + &tool_call_records("r", 5, &[fail()]).concat();
    std::fs::write(dir.join("proj").join("r.jsonl"), resumed).expect("resumed");
    let (code, stdout, stderr) = run(&[
        "tools",
        "--claude-root",
        dir.to_str().expect("UTF-8 path"),
        "--codex-root",
        CODEX,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let summary = &doc["detectors"]["tools"]["summary"];
    assert_eq!(
        [
            &summary["cascades"],
            &summary["calls_in_cascades"],
            &summary["retry_churn_calls"]
        ],
        [&json!(1), &json!(3), &json!(1)],
        "{summary}"
    );
    let bash = find(rows(&doc, "tools", 0), &[("tool", json!("Bash"))]);
    assert_eq!([&bash["calls"], &bash["errors"]], [&json!(5), &json!(4)]);
}

/// The tools section's JSON for a transcript of results of several tools: one large, one
/// with an image, and runs of errors.
fn mixed_tool_calls() -> Value {
    let fail = || json!({"command": "false"});
    let boom = || json!("boom");
    let dir = tool_calls(
        "tool-calls",
        &[
            (
                "WebSearch",
                json!({"query": "q"}),
                json!("w".repeat(40_000)),
                false,
            ),
            (
                "Read",
                json!({"file_path": "a.png"}),
                json!([{"type": "text", "text": "img"},
                       {"type": "image", "source": {"type": "base64", "data": "AAAA"}}]),
                false,
            ),
            // 3 erroring runs of one call, then another tool's error: a cascade of 4,
            // and retry churn from the third erroring run.
            ("Bash", fail(), boom(), true),
            ("Bash", fail(), boom(), true),
            ("Bash", fail(), boom(), true),
            ("Edit", json!({"file_path": "x"}), boom(), true),
            ("Bash", json!({"command": "true"}), json!(""), false),
            // The fourth erroring run of the call, with no success of it between: churn.
            ("Bash", fail(), boom(), true),
            // Once the call succeeds, its count starts again: 2 errors are no churn and,
            // with no third in a row, no cascade.
            ("Bash", fail(), json!("ok"), false),
            ("Bash", fail(), boom(), true),
            ("Grep", json!({"pattern": "p"}), boom(), true),
        ],
    );
    let args = [
        "tools",
        "--claude-root",
        dir.to_str().expect("UTF-8 path"),
        "--codex-root",
        CODEX,
        "--since",
        SINCE,
    ];
    let doc: Value = serde_json::from_str(&output(&args, "--json")).expect("valid JSON");
    assert_csv_holds(&output(&args, "--csv"), &doc["detectors"]);
    doc
}

#[test]
fn tools_report_result_sizes_errors_cascades_and_retry_churn() {
    let doc = mixed_tool_calls();
    let summary = &doc["detectors"]["tools"]["summary"];
    assert_eq!(summary["cascades"], json!(1), "{summary}");
    assert_eq!(summary["calls_in_cascades"], json!(4), "{summary}");
    assert_eq!(summary["retry_churn_calls"], json!(2), "{summary}");
    let table = rows(&doc, "tools", 0);
    assert_eq!(
        table[0]["tool"],
        json!("WebSearch"),
        "largest results first"
    );
    // 10,000 tokens est read by Claude Opus 5.5 at US$4 per million.
    let web = find(table, &[("tool", json!("WebSearch"))]);
    assert_eq!(
        [
            &web["calls"],
            &web["result_chars"],
            &web["result_tokens_est"]
        ],
        [&json!(1), &json!(40000), &json!(10000)]
    );
    assert_eq!(web["result_usd_est"], json!(0.04));
    assert_eq!(web["errors"], json!(0));
    let read = find(table, &[("tool", json!("Read"))]);
    assert_eq!(
        [&read["images"], &read["result_chars"]],
        [&json!(1), &json!(3)]
    );
    let bash = find(table, &[("tool", json!("Bash"))]);
    assert_eq!(
        [&bash["calls"], &bash["errors"], &bash["error_chars"]],
        [&json!(7), &json!(5), &json!(20)]
    );
    // The last result is read by no reply, so it is unpriced.
    let grep = find(table, &[("tool", json!("Grep"))]);
    assert_eq!(
        [&grep["errors"], &grep["unpriced_tokens_est"]],
        [&json!(1), &json!(1)]
    );
}

/// A Claude transcript of `git status` run once, then by each of `readers` (a model id
/// and its reply's `usage`, if any); each reader but the first reads a poll of 80,000
/// characters.
fn polls_read_by(name: &str, readers: &[(&str, Option<Value>)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("proj")).expect("folder");
    // Written as text: Claude Code puts `model` first in a reply's message, and bigsister
    // finds a reply by that.
    let reply = |minute: usize, model: &str, usage: Option<&Value>| {
        let usage = usage.map_or(String::new(), |u| format!(",\"usage\":{u}"));
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"2026-11-09T05:{minute:02}:00.000Z\",\
             \"message\":{{\"model\":\"{model}\",\"role\":\"assistant\",\"content\":[{{\
             \"type\":\"tool_use\",\"id\":\"p{minute}\",\"name\":\"Bash\",\
             \"input\":{{\"command\":\"git status\"}}}}]{usage}}}}}\n"
        )
    };
    let result = |minute: usize, size: usize| {
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"2026-11-09T05:{minute:02}:30.000Z\",\
             \"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\
             \"tool_use_id\":\"p{minute}\",\"content\":\"{}\"}}]}}}}\n",
            "x".repeat(size)
        )
    };
    let mut text = reply(0, "claude-opus-5-5", None) + &result(0, 4);
    for (at, (model, usage)) in readers.iter().enumerate() {
        text += &reply(at + 1, model, usage.as_ref());
        // The poll the next reader reads.
        if at + 1 < readers.len() {
            text += &result(at + 1, 80_000);
        }
    }
    std::fs::write(dir.join("proj").join("s.jsonl"), text).expect("transcript");
    dir
}

#[test]
fn haiku_5_5_is_priced_by_the_size_of_the_prompt_that_read_it() {
    let usage = |fresh: u64, written: u64, read: u64| {
        Some(
            json!({"input_tokens": fresh, "cache_creation_input_tokens": written,
            "cache_read_input_tokens": read, "output_tokens": 10}),
        )
    };
    // The first reply reads the first run, not a poll. Then 4 polls of 20,000 tokens est,
    // read by Haiku 5.5 with a 50,000-token prompt (US$0.10 per million), Haiku 5.5 with
    // a 150,000-token prompt (US$0.50), Haiku 5.5 with no usage recorded (unpriced) and
    // Claude Opus 4.5 by its dated id (US$5): US$0.002 + 0.01 + 0.1, rounded to cents.
    let dir = polls_read_by(
        "priced-by-prompt",
        &[
            ("claude-opus-4-8", None),
            ("claude-haiku-5-5", usage(2, 10_000, 39_998)),
            ("claude-haiku-5-5", usage(150_000, 0, 0)),
            ("claude-haiku-5-5", None),
            ("claude-opus-4-5-20251101", None),
        ],
    );
    let doc = priced(dir.to_str().expect("UTF-8 path"), &[]);
    let claude = find(rows(&doc, "polls", 0), &[("agent", json!("claude"))]);
    assert_eq!(claude["poll_tokens_est"], json!(80000));
    assert_eq!(claude["poll_usd_est"], json!(0.11));
    assert_eq!(claude["unpriced_tokens_est"], json!(20000));
}

#[test]
fn a_haiku_5_5_prompt_of_unknown_size_leaves_what_it_read_unpriced() {
    // 4 polls of 20,000 tokens est, read by Haiku 5.5 with a prompt of exactly 100,000
    // tokens and no cache fields (US$0.10 per million: US$0.002), Haiku 5.5 whose prompt
    // overflows a u64, Haiku 5.5 with a negative cache read (both unpriced) and Claude
    // Opus 4.5 (US$5: US$0.10). At US$0.50 the first would add a cent.
    let dir = polls_read_by(
        "prompt-of-unknown-size",
        &[
            ("claude-opus-4-8", None),
            ("claude-haiku-5-5", Some(json!({"input_tokens": 100_000}))),
            (
                "claude-haiku-5-5",
                Some(json!({"input_tokens": u64::MAX, "cache_creation_input_tokens": 1})),
            ),
            (
                "claude-haiku-5-5",
                Some(json!({"input_tokens": 100_000, "cache_read_input_tokens": -1})),
            ),
            ("claude-opus-4-5-20251101", None),
        ],
    );
    let doc = priced(dir.to_str().expect("UTF-8 path"), &[]);
    let claude = find(rows(&doc, "polls", 0), &[("agent", json!("claude"))]);
    assert_eq!(claude["poll_tokens_est"], json!(80000));
    assert_eq!(claude["poll_usd_est"], json!(0.1));
    assert_eq!(claude["unpriced_tokens_est"], json!(40000));
}

#[test]
fn a_given_rate_prices_every_estimate_and_leaves_none_unpriced() {
    let doc = priced(PRICED_CLAUDE, &["--usd-per-mtok", "1000"]);
    let claude = find(rows(&doc, "polls", 0), &[("agent", json!("claude"))]);
    assert_eq!(claude["poll_usd_est"], json!(7.5));
    assert_eq!(claude["unpriced_tokens_est"], json!(0));
    let tools = rows(&doc, "tools", 0);
    assert!(!tools.is_empty());
    for row in tools {
        assert_eq!(row["unpriced_tokens_est"], json!(0), "{row}");
    }
    let notes = doc["detectors"]["polls"]["notes"].to_string();
    assert!(notes.contains("--usd-per-mtok"), "{notes}");
    assert!(!notes.contains("16 November 2026"), "{notes}");
}

#[test]
fn a_copy_of_a_reply_written_after_a_result_does_not_read_it() {
    let doc = priced(PRICED_MIXED, &[]);
    // The poll's 10,000 characters are read by the Fable 5.2 reply (US$8 per million),
    // not by the copy of the Opus 5.5 reply (US$4) written before it.
    let claude = find(rows(&doc, "polls", 0), &[("agent", json!("claude"))]);
    assert_eq!(claude["poll_chars"], json!(10000));
    assert_eq!(claude["poll_usd_est"], json!(0.02));
    assert_eq!(claude["unpriced_tokens_est"], json!(0));
}

#[test]
fn idle_schemas_of_a_session_with_models_of_different_prices_stay_unpriced() {
    let doc = priced(PRICED_MIXED, &[]);
    let idle = find(
        rows(&doc, "schemas", 0),
        &[("tool", json!("mcp__mixed__*"))],
    );
    assert_eq!(idle["idle_usd_est"], json!(0.0));
    assert_eq!(idle["unpriced_tokens_est"], idle["idle_tokens_est"]);
    assert!(idle["idle_tokens_est"].as_u64() > Some(4000), "{idle}");
}

#[test]
fn model_ids_never_reach_the_output() {
    assert_no_sentinel(PRICED_CLAUDE, PRICED_CODEX);
}

const CSV_CLAUDE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/csv/claude");

/// The output of `args` with `flag` added, after a successful run.
fn output(args: &[&str], flag: &str) -> String {
    let mut args = args.to_vec();
    args.push(flag);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    stdout
}

/// The records of a `--csv` output, after checking its header.
fn csv_records(text: &str) -> Vec<csv::StringRecord> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let header = reader.headers().expect("a header").clone();
    assert_eq!(
        header.iter().collect::<Vec<_>>(),
        ["section", "kind", "table", "row", "column", "value"]
    );
    reader
        .records()
        .map(|r| r.expect("an RFC 4180 record"))
        .collect()
}

/// A `--csv` output rebuilt into the `detectors` object `--json` prints.
fn detectors_from_csv(text: &str) -> Value {
    let mut detectors = serde_json::Map::new();
    for r in csv_records(text) {
        let value: Value = serde_json::from_str(&r[5]).expect("a JSON value");
        let section = detectors
            .entry(r[0].to_owned())
            .or_insert_with(|| json!({"summary": {}, "tables": [], "notes": []}));
        let ordinal = |field: &str| field.parse::<usize>().expect("an ordinal");
        match &r[1] {
            "section_title" => section["title"] = value,
            "summary" => section["summary"][&r[4]] = value,
            "note" => section["notes"].as_array_mut().expect("notes").push(value),
            kind => {
                let tables = section["tables"].as_array_mut().expect("tables");
                let t = ordinal(&r[2]);
                if tables.len() == t {
                    tables.push(json!({"rows": []}));
                }
                match kind {
                    "table_title" => tables[t]["title"] = value,
                    "omitted" => tables[t]["omitted"] = value,
                    "cell" => {
                        let rows = tables[t]["rows"].as_array_mut().expect("rows");
                        let i = ordinal(&r[3]);
                        if rows.len() == i {
                            rows.push(json!({}));
                        }
                        rows[i][&r[4]] = value;
                    }
                    other => panic!("unknown kind {other}"),
                }
            }
        }
    }
    Value::Object(detectors)
}

/// Checks that a `--csv` output holds `detectors` from `--json`, each value once: a
/// repeated key would rebuild equal but print an extra record.
fn assert_csv_holds(csv: &str, detectors: &Value) {
    assert_eq!(&detectors_from_csv(csv), detectors);
    let count: usize = detectors
        .as_object()
        .expect("sections")
        .values()
        .map(|s| {
            let tables = s["tables"].as_array().expect("tables");
            let cells: usize = tables
                .iter()
                .flat_map(|t| t["rows"].as_array().expect("rows"))
                .map(|r| r.as_object().expect("a row").len())
                .sum();
            1 + s["summary"].as_object().expect("summary").len()
                + s["notes"].as_array().expect("notes").len()
                + 2 * tables.len()
                + cells
        })
        .sum();
    assert_eq!(csv_records(csv).len(), count);
}

#[test]
fn csv_holds_every_value_json_holds() {
    let args = fixture_args("all", &["--paths"]);
    let json: Value = serde_json::from_str(&output(&args, "--json")).expect("valid JSON");
    let csv = output(&args, "--csv");
    assert!(csv.lines().count() > 50, "{csv}");
    assert_csv_holds(&csv, &json["detectors"]);
}

#[test]
fn csv_quotes_every_field_and_starts_no_text_like_a_formula() {
    let args = [
        "schemas",
        "--agent",
        "claude",
        "--claude-root",
        CSV_CLAUDE,
        "--since",
        SINCE,
    ];
    let json: Value = serde_json::from_str(&output(&args, "--json")).expect("valid JSON");
    let csv = output(&args, "--csv");
    assert_csv_holds(&csv, &json["detectors"]);
    let tools: Vec<Value> = csv_records(&csv)
        .iter()
        .filter(|r| &r[1] == "cell" && &r[4] == "tool")
        .map(|r| serde_json::from_str(&r[5]).expect("a JSON value"))
        .collect();
    for name in [
        "=SUM(1,2)",
        "+1",
        "-2",
        "@x",
        "say \"hi\", there",
        "line\nbreak",
        "\tlead",
        "\rcr",
        "naïve 名前",
    ] {
        assert!(
            tools.contains(&json!(name)),
            "{name:?} missing from {tools:?}"
        );
    }
    // A spreadsheet reads a field starting with one of these as a formula; only a
    // number, such as -2.5, may start with '-'.
    for r in csv_records(&csv) {
        for field in r.iter() {
            let risky = field.starts_with(['=', '+', '@', '\t', '\r'])
                || (field.starts_with('-') && field.parse::<f64>().is_err());
            assert!(!risky, "field {field:?} in {r:?}");
        }
    }
}

#[test]
fn csv_and_json_cannot_be_combined() {
    let mut args = fixture_args("polls", &["--csv"]);
    args.push("--json");
    let (code, _, stderr) = run(&args);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

/// Writes `text` to a fresh price file named `name`.
fn price_file(name: &str, text: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, text).expect("price file");
    path
}

#[test]
fn a_price_file_replaces_the_bundled_prices() {
    // LiteLLM's names, with fields bigsister ignores. Claude Fable 5.2 repriced to US$20 per
    // million, a model the bundle lacks priced at US$40 and another at US$10, and GPT-6.1
    // Sol left out, so unpriced.
    let file = price_file(
        "prices-replace.json",
        r#"{
          "claude-fable-5-2": {"input_cost_per_token": 2e-05, "output_cost_per_token": 1e-04},
          "claude-SENTINEL-MODEL": {"input_cost_per_token": 4e-05},
          "gpt-SENTINEL-MODEL": {"input_cost_per_token": 1e-05, "mode": "chat"},
          "claude-opus-5-5": {"input_cost_per_token": 4e-06},
          "claude-sonnet-5-5": {"input_cost_per_token": 2e-06}
        }"#,
    );
    let doc = priced(
        PRICED_CLAUDE,
        &["--prices", file.to_str().expect("UTF-8 path")],
    );
    let polls = rows(&doc, "polls", 0);
    // Claude: 3 polls of 2,500 tokens est: US$0.05 at US$20, US$0.10 at US$40, and one
    // no reply read.
    let claude = find(polls, &[("agent", json!("claude"))]);
    assert_eq!(claude["poll_usd_est"], json!(0.15));
    assert_eq!(claude["unpriced_tokens_est"], json!(2500));
    // Codex: 3 polls of 5,000 tokens est: before any turn, in the GPT-6.1 Sol turn (no
    // longer priced) and in a turn at US$10.
    let codex = find(polls, &[("agent", json!("codex"))]);
    assert_eq!(codex["poll_usd_est"], json!(0.05));
    assert_eq!(codex["unpriced_tokens_est"], json!(10000));
    let notes = doc["detectors"]["polls"]["notes"].to_string();
    assert!(
        notes.contains("price file") && notes.contains("prices-replace.json"),
        "{notes}"
    );
    assert!(!notes.contains("bundled"), "{notes}");
}

#[test]
fn a_bad_price_file_fails_the_run() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-such-prices.json");
    let bad = price_file(
        "prices-bad.json",
        r#"{"claude-opus-5-5": {"input_cost_per_token": "4e-06"}}"#,
    );
    for (file, wanted) in [
        (&missing, "cannot read price file"),
        (
            &bad,
            "claude-opus-5-5: input_cost_per_token: not a number of at least 0",
        ),
    ] {
        let (code, stdout, stderr) = run(&[
            "polls",
            "--claude-root",
            CLAUDE,
            "--codex-root",
            CODEX,
            "--prices",
            file.to_str().expect("UTF-8 path"),
        ]);
        assert_eq!(code, 1, "{stderr}");
        assert!(stdout.is_empty(), "{stdout}");
        assert!(stderr.contains(wanted), "{stderr}");
    }
    let (code, _, stderr) = run(&[
        "polls",
        "--prices",
        bad.to_str().expect("UTF-8 path"),
        "--usd-per-mtok",
        "3",
    ]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

const PI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi");

/// `bigsister all --agent pi --json` over the pi fixtures: a session and a fork of it,
/// which pi writes as a copy of every entry.
fn pi_report(extra: &[&str]) -> Value {
    let mut args = vec![
        "all",
        "--agent",
        "pi",
        "--pi-root",
        PI,
        "--since",
        SINCE,
        "--json",
    ];
    args.extend_from_slice(extra);
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn pi_sessions_feed_pipes_polls_and_rereads_once_despite_a_fork() {
    let doc = pi_report(&[]);
    let pipes = find(rows(&doc, "pipes", 0), &[("agent", json!("pi"))]);
    assert_eq!(pipes["test_runs"], json!(1));
    assert_eq!(pipes["unguarded"], json!(1));
    assert_eq!(pipes["showed_a_failure_but_reported_success"], json!(1));
    let polls = find(rows(&doc, "polls", 0), &[("agent", json!("pi"))]);
    assert_eq!(polls["commands"], json!(4));
    assert_eq!(polls["polls"], json!(1));
    assert_eq!(polls["reruns_after_edit"], json!(1));
    // The poll's 4,000 characters, read by Claude Fable 5.1 at US$10 per million.
    assert_eq!(polls["poll_chars"], json!(4000));
    assert_eq!(polls["poll_usd_est"], json!(0.01));
    assert_eq!(polls["unpriced_tokens_est"], json!(0));
    let rereads = find(rows(&doc, "rereads", 0), &[("agent", json!("pi"))]);
    assert_eq!(rereads["compactions"], json!(1));
    assert_eq!(rereads["reads"], json!(2));
    assert_eq!(rereads["rereads"], json!(1));
    // The re-read's 4,000 characters, read by Claude Fable 5.1 at US$10 per million.
    assert_eq!(rereads["reread_chars"], json!(4000));
    assert_eq!(rereads["reread_usd_est"], json!(0.01));
    let roots = doc["roots"].as_array().expect("roots");
    assert_eq!(roots.len(), 1, "{roots:?}");
    assert_eq!(roots[0]["agent"], json!("pi"));
    assert_eq!(roots[0]["files"], json!(2));
    assert_eq!(roots[0]["lines_skipped"], json!(0));
}

/// pi's command results by verb class leave a blank command out, as the note says, whilst
/// the commands total counts it.
#[test]
fn pi_command_results_count_each_non_blank_command_by_verb_class() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi-blank");
    let (code, stdout, stderr) = run(&[
        "polls",
        "--agent",
        "pi",
        "--pi-root",
        root,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let totals = find(rows(&doc, "polls", 0), &[("agent", json!("pi"))]);
    assert_eq!(totals["commands"], json!(3));
    let results = rows(&doc, "polls", 2);
    assert_eq!(results.len(), 2, "{results:?}");
    for (verb, expected) in [("git", [1, 1, 10]), ("cargo", [1, 1, 7])] {
        let r = find(results, &[("agent", json!("pi")), ("verb", json!(verb))]);
        assert_eq!(
            [
                r["commands"].clone(),
                r["results"].clone(),
                r["result_chars"].clone()
            ],
            expected.map(|n| json!(n)),
            "{verb}"
        );
    }
    let notes = doc["detectors"]["polls"]["notes"].to_string();
    assert!(notes.contains("count every non-blank command"), "{notes}");
}

#[test]
fn pi_tool_calls_count_once_despite_a_fork() {
    let doc = pi_report(&[]);
    let table = rows(&doc, "tools", 0);
    assert_eq!(table.len(), 3, "{table:?}");
    for (tool, calls) in [("Bash", 4), ("Read", 2), ("Edit", 1)] {
        let row = find(table, &[("agent", json!("pi")), ("tool", json!(tool))]);
        assert_eq!(
            [&row["calls"], &row["errors"]],
            [&json!(calls), &json!(0)],
            "{tool}"
        );
    }
}

#[test]
fn pi_output_never_contains_transcript_content() {
    for extra in [&[][..], &["--json"], &["--paths"], &["--csv", "--paths"]] {
        let mut args = vec!["all", "--agent", "pi", "--pi-root", PI, "--since", SINCE];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{stderr}");
        assert!(
            !stdout.to_lowercase().contains("sentinel"),
            "content leaked with {extra:?}:\n{stdout}"
        );
        assert!(!stderr.to_lowercase().contains("sentinel"), "{stderr}");
    }
}

const GEMINI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gemini");
const QWEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/qwen");

/// Each of the 2 agents, its folder option and its fixture: one session whose 5 tool
/// calls are a test run piped into `tail`, `git status` twice, a read and a failed edit.
const GEMINI_AND_QWEN: [(&str, &str, &str); 2] = [
    ("gemini", "--gemini-root", GEMINI),
    ("qwen", "--qwen-root", QWEN),
];

/// `bigsister all --agent <agent> --json` over that agent's fixture.
fn agent_report(agent: &str, flag: &str, root: &str) -> Value {
    let args = [
        "all", "--agent", agent, flag, root, "--since", SINCE, "--json",
    ];
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

#[test]
fn gemini_and_qwen_sessions_feed_pipes_polls_and_tools() {
    for (agent, flag, root) in GEMINI_AND_QWEN {
        let doc = agent_report(agent, flag, root);
        let pipes = find(rows(&doc, "pipes", 0), &[("agent", json!(agent))]);
        for field in [
            "test_runs",
            "unguarded",
            "showed_a_failure_but_reported_success",
        ] {
            assert_eq!(pipes[field], json!(1), "{agent} {field}");
        }
        let polls = find(rows(&doc, "polls", 0), &[("agent", json!(agent))]);
        assert_eq!(
            [&polls["commands"], &polls["polls"]],
            [&json!(3), &json!(1)],
            "{agent}"
        );
        // No compaction record is read for either, so rereads does not read them.
        assert!(
            rows(&doc, "rereads", 0)
                .iter()
                .all(|r| r["agent"] != json!(agent)),
            "{agent}"
        );
        let roots = doc["roots"].as_array().expect("roots");
        assert_eq!(roots.len(), 1, "{agent}: {roots:?}");
        assert_eq!(roots[0]["agent"], json!(agent));
        assert_eq!(roots[0]["files"], json!(1), "{agent}");
        assert_eq!(roots[0]["lines_skipped"], json!(0), "{agent}");
    }
}

/// Gemini CLI writes a reply again under the same id as its tool calls progress: only the
/// last version counts, with each call's status.
#[test]
fn gemini_and_qwen_tool_calls_count_once_under_claude_names() {
    for (agent, flag, root) in GEMINI_AND_QWEN {
        let doc = agent_report(agent, flag, root);
        let table = rows(&doc, "tools", 0);
        assert_eq!(table.len(), 3, "{agent}: {table:?}");
        for (tool, calls, errors) in [("Bash", 3, 0), ("Read", 1, 0), ("Edit", 1, 1)] {
            let row = find(table, &[("agent", json!(agent)), ("tool", json!(tool))]);
            assert_eq!(
                [&row["calls"], &row["errors"]],
                [&json!(calls), &json!(errors)],
                "{agent} {tool}"
            );
        }
    }
}

#[test]
fn gemini_and_qwen_output_never_contains_transcript_content() {
    for (agent, flag, root) in GEMINI_AND_QWEN {
        for extra in [&[][..], &["--json"], &["--paths"], &["--csv", "--paths"]] {
            let mut args = vec!["all", "--agent", agent, flag, root, "--since", SINCE];
            args.extend_from_slice(extra);
            let (code, stdout, stderr) = run(&args);
            assert_eq!(code, 0, "{stderr}");
            let all = format!("{stdout}{stderr}").to_lowercase();
            assert!(!all.contains("sentinel"), "{agent} {extra:?}:\n{all}");
        }
    }
}

#[test]
fn a_missing_gemini_or_qwen_folder_is_an_error_when_asked_for() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-agent-here");
    let missing = missing.to_str().expect("UTF-8 path");
    for (agent, flag, _) in GEMINI_AND_QWEN {
        let (code, _, stderr) = run(&["polls", flag, missing, "--agent", agent]);
        assert_eq!(code, 1, "{stderr}");
        let wanted = format!("{agent} transcript folder not found");
        assert!(stderr.contains(&wanted), "{stderr}");
    }
}

#[test]
fn a_missing_pi_folder_is_an_error_when_asked_for_and_noted_otherwise() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-pi-here");
    let missing = missing.to_str().expect("UTF-8 path");
    let (code, _, stderr) = run(&["polls", "--pi-root", missing, "--agent", "pi"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("pi transcript folder not found"),
        "{stderr}"
    );
    // Under --agent all, a missing default pi or OpenCode folder is noted, as a missing default Claude
    // or Codex folder is, and the others are read.
    let (code, stdout, stderr) = run(&[
        "polls",
        "--claude-root",
        CLAUDE,
        "--codex-root",
        CODEX,
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(
        doc["skipped_roots"],
        json!([
            "pi transcript folder not found: ~/.pi/agent/sessions",
            "opencode transcript folder not found: ~/.local/share/opencode",
            "gemini transcript folder not found: ~/.gemini/tmp",
            "qwen transcript folder not found: ~/.qwen/projects"
        ])
    );
    assert_eq!(doc["roots"].as_array().expect("roots").len(), 2, "{doc}");
}

/// A pi entry: a message with `id`, `parent` and `message`, at minute `minute`.
fn pi_message(id: &str, parent: Option<&str>, minute: u32, message: &Value) -> String {
    json!({"type": "message", "id": id, "parentId": parent,
           "timestamp": format!("2026-11-09T07:{minute:02}:00.000Z"), "message": message})
    .to_string()
}

fn pi_call(name: &str, args: &Value) -> Value {
    json!({"role": "assistant", "model": "claude-opus-5-5",
           "content": [{"type": "toolCall", "id": format!("call-{name}-{args}"), "name": name,
                        "arguments": args}]})
}

fn pi_result(call: &Value) -> Value {
    json!({"role": "toolResult", "toolCallId": call["content"][0]["id"],
           "toolName": call["content"][0]["name"],
           "content": [{"type": "text", "text": "ok"}], "isError": false})
}

/// Writes pi sessions, each a header (with `parentSession` when given) and its entries,
/// under a fresh folder `name`.
fn pi_sessions(name: &str, sessions: &[(Option<&str>, Vec<String>)]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("--w--")).expect("folder");
    for (n, (parent, entries)) in sessions.iter().enumerate() {
        let mut header = json!({"type": "session", "version": 3, "id": format!("s{n}"),
                                "timestamp": format!("2026-11-09T0{}:00:00.000Z", 6 + n),
                                "cwd": "/w"});
        if let Some(p) = parent {
            header["parentSession"] = json!(p);
        }
        let text = std::iter::once(header.to_string())
            .chain(entries.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("--w--").join(format!("{n}.jsonl")), text + "\n").expect("session");
    }
    dir
}

/// `bigsister <command> --agent pi --json` over the pi sessions in `dir`.
fn pi_run(command: &str, dir: &Path) -> Value {
    let (code, stdout, stderr) = run(&[
        command,
        "--agent",
        "pi",
        "--pi-root",
        dir.to_str().expect("UTF-8 path"),
        "--since",
        SINCE,
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

fn pi_polls(dir: &Path) -> Value {
    let doc = pi_run("polls", dir);
    find(rows(&doc, "polls", 0), &[("agent", json!("pi"))]).clone()
}

#[test]
fn a_pi_branch_sees_only_its_own_path() {
    let status = pi_call("bash", &json!({"command": "git status"}));
    let again = pi_call("bash", &json!({"command": "git status", "timeout": 5}));
    let edit = pi_call("edit", &json!({"path": "/w/a.rs", "edits": []}));
    let entries = vec![
        pi_message("e1", None, 1, &status),
        pi_message("e2", Some("e1"), 2, &pi_result(&status)),
        pi_message("e3", Some("e2"), 3, &edit),
        pi_message("e4", Some("e3"), 4, &pi_result(&edit)),
        // /tree: back to before the edit, where git status runs again with no edit.
        pi_message("e5", Some("e2"), 5, &again),
        pi_message("e6", Some("e5"), 6, &pi_result(&again)),
    ];
    let row = pi_polls(&pi_sessions("pi-branch", &[(None, entries)]));
    assert_eq!(row["commands"], json!(2));
    assert_eq!(row["reruns_after_edit"], json!(0));
    assert_eq!(row["polls"], json!(1));
}

#[test]
fn only_a_forked_pi_session_holds_copies() {
    let status = pi_call("bash", &json!({"command": "git status"}));
    let entries = vec![
        pi_message("e1", None, 1, &status),
        pi_message("e2", Some("e1"), 2, &pi_result(&status)),
        pi_message("e3", Some("e2"), 3, &status),
        pi_message("e4", Some("e3"), 4, &pi_result(&status)),
    ];
    // Ids and times repeated by a session that names no parent are its own: 2 polls.
    let unrelated = pi_sessions(
        "pi-unrelated",
        &[(None, entries.clone()), (None, entries.clone())],
    );
    assert_eq!(pi_polls(&unrelated)["polls"], json!(2));
    // A fork's copies count once.
    let forked = pi_sessions(
        "pi-forked",
        &[(None, entries.clone()), (Some("/p/0.jsonl"), entries)],
    );
    assert_eq!(pi_polls(&forked)["polls"], json!(1));
}

#[test]
fn a_pi_fork_holds_copies_only_of_the_session_it_names() {
    let status = pi_call("bash", &json!({"command": "git status"}));
    let diff = pi_call("bash", &json!({"command": "git diff"}));
    let polled = |command: &Value, ids: [&str; 4], first: u32, parent: Option<&str>| {
        vec![
            pi_message(ids[0], parent, first, command),
            pi_message(ids[1], Some(ids[0]), first + 1, &pi_result(command)),
            pi_message(ids[2], Some(ids[1]), first + 2, command),
            pi_message(ids[3], Some(ids[2]), first + 3, &pi_result(command)),
        ]
    };
    let unrelated = polled(&status, ["e1", "e2", "e3", "e4"], 1, None);
    let parent = polled(&diff, ["f1", "f2", "f3", "f4"], 11, None);
    // A fork of the second session: its copies, then entries whose ids and times
    // happen to match the first session's.
    let mut fork = parent.clone();
    fork.extend(polled(&status, ["e1", "e2", "e3", "e4"], 1, Some("f4")));
    let dir = pi_sessions(
        "pi-fork-names-parent",
        &[
            (None, unrelated),
            (None, parent),
            (Some("/p/1.jsonl"), fork),
        ],
    );
    // 1 poll in each session, the fork's own one included; the fork's copies count once.
    assert_eq!(pi_polls(&dir)["polls"], json!(3));
}

#[test]
fn a_repeated_pi_id_is_skipped_with_what_descends_from_it() {
    let status = pi_call("bash", &json!({"command": "git status"}));
    let edit = pi_call("edit", &json!({"path": "/w/a.rs", "edits": []}));
    let entries = vec![
        pi_message("e1", None, 1, &status),
        pi_message("e2", Some("e1"), 2, &pi_result(&status)),
        pi_message("e3", Some("e2"), 3, &edit),
        pi_message("e4", Some("e3"), 4, &pi_result(&edit)),
        // e2 again, at another time: which e2 an entry continues from is unknown.
        pi_message("e2", Some("e1"), 5, &pi_result(&status)),
        pi_message("e5", Some("e2"), 6, &status),
        pi_message("e6", Some("e5"), 7, &pi_result(&status)),
    ];
    let dir = pi_sessions("pi-repeated-id", &[(None, entries)]);
    let doc = pi_run("polls", &dir);
    let row = find(rows(&doc, "polls", 0), &[("agent", json!("pi"))]);
    assert_eq!(row["commands"], json!(1), "{row}");
    assert_eq!(row["reruns_after_edit"], json!(0), "{row}");
    assert_eq!(doc["roots"][0]["lines_skipped"], json!(3), "{doc}");
}

#[test]
fn a_pi_branch_back_before_a_compaction_reads_without_rereading() {
    let read = pi_call("read", &json!({"path": "/w/a.rs"}));
    let compaction = |id: &str, parent: &str, minute: u32| {
        json!({"type": "compaction", "id": id, "parentId": parent,
               "timestamp": format!("2026-11-09T07:{minute:02}:00.000Z"),
               "summary": "s", "firstKeptEntryId": parent})
        .to_string()
    };
    let entries = vec![
        pi_message("e1", None, 1, &read),
        pi_message("e2", Some("e1"), 2, &pi_result(&read)),
        compaction("e3", "e2", 3),
        pi_message("e4", Some("e3"), 4, &read),
        pi_message("e5", Some("e4"), 5, &pi_result(&read)),
        // /tree: back to before the compaction, where the file is still read.
        pi_message("e6", Some("e2"), 6, &read),
        pi_message("e7", Some("e6"), 7, &pi_result(&read)),
        // /tree: back to after the compaction, where reading it again is a re-read.
        pi_message("e8", Some("e3"), 8, &read),
        pi_message("e9", Some("e8"), 9, &pi_result(&read)),
    ];
    let doc = pi_run(
        "rereads",
        &pi_sessions("pi-branch-compaction", &[(None, entries)]),
    );
    let row = find(rows(&doc, "rereads", 0), &[("agent", json!("pi"))]);
    assert_eq!(row["sessions_compacted"], json!(1), "{row}");
    assert_eq!(row["compactions"], json!(1), "{row}");
    assert_eq!(row["reads"], json!(4), "{row}");
    assert_eq!(row["rereads"], json!(2), "{row}");
}

#[test]
fn a_pi_line_in_another_agent_s_shape_counts_nowhere() {
    let status = pi_call("bash", &json!({"command": "git status"}));
    let claude_call = |id: &str, parent: &str, minute: u32| {
        json!({"type": "assistant", "id": id, "parentId": parent,
               "timestamp": format!("2026-11-09T07:{minute:02}:00.000Z"),
               "message": {"model": "claude-opus-5-5", "role": "assistant",
                           "content": [{"type": "tool_use", "id": format!("t{id}"),
                                        "name": "Bash", "input": {"command": "git status"}}]}})
        .to_string()
    };
    let entries = vec![
        pi_message("e1", None, 1, &status),
        pi_message("e2", Some("e1"), 2, &pi_result(&status)),
        claude_call("e3", "e2", 3),
        claude_call("e4", "e3", 4),
    ];
    let row = pi_polls(&pi_sessions("pi-foreign-shape", &[(None, entries)]));
    assert_eq!(row["commands"], json!(1), "{row}");
    assert_eq!(row["polls"], json!(0), "{row}");
}

/// `polls` over the fixtures from `since`, with extra environment variables.
fn since_run(since: &str, json: bool, env: &[(&str, &Path)]) -> (i32, String, String) {
    let mut args = vec![
        "polls",
        "--claude-root",
        CLAUDE,
        "--codex-root",
        CODEX,
        "--since",
        since,
    ];
    if json {
        args.push("--json");
    }
    run_env(&args, env)
}

/// The window's start as JSON prints it.
fn since_json(since: &str, env: &[(&str, &Path)]) -> Value {
    let (code, stdout, stderr) = since_run(since, true, env);
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    doc["since"].clone()
}

#[test]
fn json_prints_the_window_start_in_rfc_3339_with_an_offset() {
    for (since, printed) in [
        ("2026-11-01T00:00:00Z", "2026-11-01T00:00:00+00:00"),
        ("2026-11-01T11:00:00+11:00", "2026-11-01T00:00:00+00:00"),
        ("2026-11-01T00:00:00.5Z", "2026-11-01T00:00:00.500+00:00"),
        (
            "2026-11-01T00:00:00.123456Z",
            "2026-11-01T00:00:00.123456+00:00",
        ),
        (
            "2026-11-01T00:00:00.1234567Z",
            "2026-11-01T00:00:00.123456700+00:00",
        ),
    ] {
        assert_eq!(since_json(since, &[]), json!(printed), "{since}");
    }
}

/// A date is midnight where the machine is; text prints the start there, with its offset.
#[cfg(unix)]
#[test]
fn a_date_is_midnight_in_the_local_zone() {
    let sydney: &[(&str, &Path)] = &[("TZ", Path::new("Australia/Sydney"))];
    assert_eq!(
        since_json("2026-11-01", sydney),
        json!("2026-10-31T13:00:00+00:00")
    );
    let (code, stdout, stderr) = since_run("2026-11-01", false, sydney);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.starts_with(&format!(
            "bigsister {}: records since 2026-11-01 00:00 +11:00\n",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    let (_, stdout, _) = since_run("2026-07-01T00:00:00Z", false, sydney);
    assert!(
        stdout.contains(": records since 2026-07-01 10:00 +10:00\n"),
        "{stdout}"
    );
}

/// Havana's clocks went back from 01:00 to midnight on 4 November 2018, so that midnight
/// came twice: the first (04:00 UTC) is taken, so no record of that day is left out. On
/// 11 March 2018 they went forward from midnight to 01:00, so that midnight never came: the
/// date starts when the clocks resumed.
#[cfg(unix)]
#[test]
fn a_repeated_local_midnight_is_the_first_and_a_skipped_one_is_when_clocks_resume() {
    let havana: &[(&str, &Path)] = &[("TZ", Path::new("America/Havana"))];
    for (date, start) in [
        ("2018-11-03", "2018-11-03T04:00:00+00:00"),
        ("2018-11-04", "2018-11-04T04:00:00+00:00"),
        ("2018-03-11", "2018-03-11T05:00:00+00:00"),
        ("2018-03-12", "2018-03-12T04:00:00+00:00"),
    ] {
        assert_eq!(since_json(date, havana), json!(start), "{date}");
    }
}

/// The last date there is, at midnight in UTC, lies past the last instant the window can
/// start at: it still parses, and finds nothing.
#[cfg(unix)]
#[test]
fn the_last_date_at_utc_midnight_finds_nothing() {
    let utc: &[(&str, &Path)] = &[("TZ", Path::new("UTC"))];
    let (code, stdout, stderr) = since_run("9999-12-31", true, utc);
    assert_eq!(code, 0, "{stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["files"], 0);
    assert_eq!(doc["since"], json!("9999-12-30T22:00:00.999999999+00:00"));
}

/// The last date's midnight at +02:00 is the first instant past the last one a window can
/// start at; at +01:00 it lies past that instant, and the window holds nothing.
#[cfg(unix)]
#[test]
fn the_last_date_starts_a_window_only_from_plus_two_hours() {
    for (zone, start) in [
        ("Africa/Johannesburg", "9999-12-30T22:00:00+00:00"),
        ("Europe/Paris", "9999-12-30T22:00:00.999999999+00:00"),
    ] {
        let tz: &[(&str, &Path)] = &[("TZ", Path::new(zone))];
        assert_eq!(since_json("9999-12-31", tz), json!(start), "{zone}");
    }
}

const CONTEXT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/context/claude");

fn context_report() -> Value {
    let args = [
        "context",
        "--claude-root",
        CONTEXT,
        "--agent",
        "claude",
        "--since",
        SINCE,
        "--json",
    ];
    let (code, stdout, stderr) = run(&args);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(!stdout.to_lowercase().contains("sentinel"), "{stdout}");
    serde_json::from_str(&stdout).expect("valid JSON")
}

/// The cells `columns` of each row of the context section's table `table`.
fn context_cells(doc: &Value, table: usize, columns: &[&str]) -> Value {
    rows(doc, "context", table)
        .iter()
        .map(|r| cells(r, columns))
        .collect()
}

fn parsed(text: &str) -> Value {
    serde_json::from_str(text).expect("valid JSON")
}

/// Each request is rebuilt from Claude Code's request log: its shape's system prompt and
/// loaded tools, its base request's first `keep` messages and its tail, each found as a
/// request blob, a user record or the blocks of one reply, by the SHA-256 of the message.
#[test]
fn context_rebuilds_each_request_and_splits_its_input_by_part() {
    let doc = context_report();
    let s = &doc["detectors"]["context"]["summary"];
    assert_eq!(
        s["requests_rebuilt"], 3,
        "the request before the window is left out"
    );
    assert_eq!(
        s["requests_not_rebuilt"], 1,
        "its base request is not in the file"
    );
    assert_eq!(
        s["requests_partly_rebuilt"], 1,
        "it names a message not in the file, and counts in no table"
    );
    assert_eq!(s["requests_with_tokens"], 3);
    assert_eq!(s["messages_not_found"], 1);
    assert_eq!(s["median_input_tokens"], 1010);
    let columns = [
        "part",
        "requests",
        "median_chars",
        "max_chars",
        "share_of_chars",
        "est_input_tokens",
    ];
    assert_eq!(
        context_cells(&doc, 0, &columns),
        parsed(
            r#"[["system prompt",3,37,37,0.06,175],["built-in tools",3,79,79,0.16,473],
            ["MCP tools",2,263,263,0.36,902],["system messages",2,20,20,0.03,69],
            ["reminders",2,58,58,0.08,199],["tool results",1,0,78,0.05,117],
            ["tool calls",1,0,88,0.06,132],["thinking",1,0,32,0.02,48],
            ["text",3,66,66,0.1,275],["other blocks",1,0,90,0.06,135]]"#
        )
    );
}

/// MCP tools are split by server, over the requests that loaded each server's tools.
#[test]
fn context_splits_mcp_tools_by_server() {
    let doc = context_report();
    let columns = [
        "server",
        "requests",
        "median_chars",
        "max_chars",
        "est_input_tokens",
    ];
    assert_eq!(
        context_cells(&doc, 1, &columns),
        parsed(r#"[["browser",2,181,181,621],["notes",2,82,82,281]]"#)
    );
}

/// The largest requests come first, each with its scope, model and parts' tokens.
#[test]
fn context_lists_the_largest_requests_first() {
    let doc = context_report();
    let columns = [
        "time",
        "scope",
        "model",
        "input_tokens",
        "messages",
        "system_prompt",
        "built_in_tools",
        "mcp_tools",
        "system_messages",
        "reminders",
        "tool_results",
        "tool_calls",
        "thinking",
        "text",
        "other_blocks",
    ];
    assert_eq!(
        context_cells(&doc, 2, &columns),
        parsed(
            r#"[["2026-11-08T01:04:00+00:00","main","claude-opus-5-5",1215,5,55,118,394,30,87,117,132,48,99,135],
            ["2026-11-08T01:01:00+00:00","main","claude-opus-5-5",1010,3,71,153,508,39,112,0,0,0,127,0],
            ["2026-11-08T01:11:00+00:00","subagent","claude-haiku-5-5",300,1,49,202,0,0,0,0,0,0,49,0]]"#
        )
    );
}

/// With every section chosen, the request log's lines are read with none skipped, and the
/// context section still rebuilds each request.
#[test]
fn all_reads_the_request_log_with_every_section() {
    let args = |cmd| {
        vec![
            cmd,
            "--claude-root",
            CONTEXT,
            "--agent",
            "claude",
            "--since",
            SINCE,
            "--json",
        ]
    };
    let (code, stdout, stderr) = run(&args("all"));
    assert_eq!(code, 0, "stderr: {stderr}");
    let doc: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(doc["roots"][0]["lines_skipped"], 0);
    assert_eq!(
        doc["detectors"]["context"]["summary"]["requests_rebuilt"],
        3
    );
}

/// Copies every file under `from` into `to`, keeping its path below `from`.
fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("fixture folder") {
        let path = entry.expect("fixture entry").path();
        let dest = to.join(path.file_name().expect("file name"));
        if path.is_dir() {
            std::fs::create_dir_all(&dest).expect("folder");
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).expect("copy");
        }
    }
}

/// One folder holding the main Claude fixtures and the request-log fixture, as the sample
/// does.
fn sample_claude_root() -> PathBuf {
    let claude = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sample-claude");
    let _ = std::fs::remove_dir_all(&claude);
    std::fs::create_dir_all(&claude).expect("folder");
    copy_tree(Path::new(CLAUDE), &claude);
    copy_tree(Path::new(CONTEXT), &claude);
    claude
}

/// `--sample` reads the sample transcripts built into the binary, with no transcript
/// folder on the machine: the main Claude, Codex and pi fixtures and the request-log
/// fixture, from 2026-11-01. Each section reports what it reports on those fixtures read
/// from disk.
#[test]
fn sample_reports_the_bundled_fixtures_with_no_transcripts_on_the_machine() {
    let (code, stdout, stderr) = run(&["all", "--sample", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let sample: Value = serde_json::from_str(&stdout).expect("valid JSON");
    let claude = sample_claude_root();
    let claude = claude.to_str().expect("UTF-8 path");
    let (code, stdout, stderr) = run(&[
        "all",
        "--claude-root",
        claude,
        "--codex-root",
        CODEX,
        "--pi-root",
        PI,
        "--agent",
        "all",
        "--since",
        "2026-11-01",
        "--json",
    ]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let disk: Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(sample["detectors"], disk["detectors"]);
    assert_eq!(sample["since"], disk["since"]);
    let roots: Vec<Value> = sample["roots"]
        .as_array()
        .expect("roots")
        .iter()
        .map(|r| cells(r, &["agent", "root", "files"]))
        .collect();
    assert_eq!(
        roots,
        [
            json!(["claude", "(bundled sample)", 5]),
            json!(["codex", "(bundled sample)", 2]),
            json!(["pi", "(bundled sample)", 2])
        ]
    );
    assert_eq!(sample["skipped_roots"], json!([]));
    for section in sample["detectors"].as_object().expect("detectors").values() {
        let rows: usize = section["tables"]
            .as_array()
            .expect("tables")
            .iter()
            .map(|t| t["rows"].as_array().map_or(0, Vec::len))
            .sum();
        assert!(rows > 0, "a section with no rows: {section}");
    }
}

/// The sample's window and folders are its own, so `--since` and the folder options
/// are usage errors beside it.
#[test]
fn sample_takes_no_window_or_folder() {
    for extra in [
        ["--since", "7d"],
        ["--claude-root", CLAUDE],
        ["--codex-root", CODEX],
        ["--pi-root", PI],
        ["--opencode-root", PI],
    ] {
        let (code, _, stderr) = run(&["all", "--sample", extra[0], extra[1]]);
        assert_eq!(code, 2, "{extra:?}: {stderr}");
    }
}

/// With no transcript folder on the machine, the error names the sample run.
#[test]
fn no_transcript_folder_points_to_the_sample() {
    let (code, _, stderr) = run(&["all"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("transcript folder not found"), "{stderr}");
    assert!(stderr.contains("bigsister all --sample"), "{stderr}");
}
