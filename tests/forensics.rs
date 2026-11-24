//! Integration tests for sub-TTL cache busts, compaction health, truncation re-runs and diff.

mod forensics {
    mod cache_bust;
    mod compaction;
    mod diff;
    mod truncation;

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde_json::Value;

    pub(super) fn temp_dir(prefix: &str, name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bigsister-{prefix}-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    pub(super) fn write_jsonl(dir: &Path, rel: &str, lines: &[String]) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, format!("{}\n", lines.join("\n"))).expect("write fixture");
    }

    pub(super) fn run(args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_bigsister"))
            .args(args)
            .output()
            .expect("run bigsister");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub(super) fn run_json(cmd: &str, extra: &[&str]) -> Value {
        let mut args = vec![cmd, "--since", "2026-11-01", "--json"];
        args.extend(extra);
        let (code, stdout, stderr) = run(&args);
        assert_eq!(code, 0, "{stderr}");
        serde_json::from_str(&stdout).expect("valid JSON")
    }

    pub(super) fn table_rows<'a>(doc: &'a Value, section: &str, title: &str) -> &'a [Value] {
        doc["detectors"][section]["tables"]
            .as_array()
            .expect("tables")
            .iter()
            .find(|t| t["title"] == title)
            .and_then(|t| t["rows"].as_array())
            .map(Vec::as_slice)
            .expect("table by title")
    }

    pub(super) fn find_row<'a>(rows: &'a [Value], pairs: &[(&str, Value)]) -> &'a Value {
        rows.iter()
            .find(|r| pairs.iter().all(|(k, v)| r.get(*k) == Some(v)))
            .unwrap_or_else(|| panic!("no row matching {pairs:?} in {rows:?}"))
    }

    pub(super) fn reply(
        ts: &str,
        id: &str,
        model: &str,
        input: u64,
        write: u64,
        read: u64,
        content: &Value,
    ) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{id}","timestamp":"{ts}","message":{{"model":"{model}","id":"{id}","role":"assistant","content":{content},"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{write},"cache_read_input_tokens":{read},"cache_creation":{{"ephemeral_5m_input_tokens":{write},"ephemeral_1h_input_tokens":0}}}}}}}}"#
        )
    }
}
