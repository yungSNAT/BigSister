//! Listing transcript files and reading their lines, compressed or not.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use jiff::Timestamp;
use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
use ruzstd::decoding::{FrameDecoder, StreamingDecoder};
use serde_json::Value;

use super::RootStats;
use crate::time::parse_ts;

/// A `*.json` OTLP file, which holds one object, compact or not, as one compact line, so
/// that it reads as a `*.jsonl` file of one line does. A file that is not one JSON value,
/// or is longer than `MAX_LINE`, is read as it is, so its lines count as skipped. A byte
/// order mark before it is dropped.
pub(super) fn one_line(mut reader: Box<dyn BufRead>) -> Box<dyn BufRead> {
    let mut text = Vec::new();
    let read = reader.by_ref().take(MAX_LINE + 1).read_to_end(&mut text);
    if read.is_ok()
        && text.len() as u64 <= MAX_LINE
        && let Ok(v) =
            serde_json::from_slice::<Value>(text.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&text))
    {
        text = v.to_string().into_bytes();
    }
    Box::new(io::Cursor::new(text).chain(reader))
}

/// The transcripts under `root` modified at or after `cutoff`: `*.jsonl` and `*.jsonl.zst`
/// files, and `*.json` ones too when `json`.
pub(super) fn list_files(
    root: &Path,
    cutoff: SystemTime,
    json: bool,
    st: &mut RootStats,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Some(entries) = st.readable(fs::read_dir(&dir), "a folder cannot be listed") else {
            continue;
        };
        // Collected first, so the loop below can count into `st` too.
        let entries: Vec<_> = entries
            .filter_map(|entry| {
                let read = entry.and_then(|e| e.file_type().map(|ft| (e, ft)));
                st.readable(read, "a folder entry cannot be read")
            })
            .collect();
        for (entry, ft) in entries {
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else {
                let compressed = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".jsonl.zst"));
                let wanted = |x: &std::ffi::OsStr| x == "jsonl" || (json && x == "json");
                if !compressed && !path.extension().is_some_and(wanted) {
                    continue;
                }
                let modified = entry.metadata().and_then(|m| m.modified());
                keep_recent(path, modified, cutoff, &mut out, st);
            }
        }
    }
    out.sort();
    out
}

/// Adds `path` to `out` when it was `modified` at or after `cutoff`, or counts why its
/// time cannot be read.
pub(super) fn keep_recent(
    path: PathBuf,
    modified: io::Result<SystemTime>,
    cutoff: SystemTime,
    out: &mut Vec<PathBuf>,
    st: &mut RootStats,
) {
    match modified {
        Ok(m) if m >= cutoff => out.push(path),
        Ok(_) => {}
        Err(e) => st.count_unreadable(format!("a file's time cannot be read: {e}")),
    }
}

/// Antigravity's log of each conversation folder in `root` modified at or after
/// `cutoff`: `<id>/.system_generated/logs/transcript_full.jsonl`, or `transcript.jsonl`
/// beside it when there is no full log, whose long fields are cut short. Only those
/// paths are looked at, as a conversation folder can hold whole repositories.
pub(super) fn antigravity_logs(
    root: &Path,
    cutoff: SystemTime,
    st: &mut RootStats,
) -> Vec<PathBuf> {
    let Some(entries) = st.readable(fs::read_dir(root), "a folder cannot be listed") else {
        return Vec::new();
    };
    let entries: Vec<_> = entries
        .filter_map(|entry| st.readable(entry, "a folder entry cannot be read"))
        .collect();
    let mut out = Vec::new();
    for entry in entries {
        let logs = entry.path().join(".system_generated").join("logs");
        let full = logs.join("transcript_full.jsonl");
        let path = if full.is_file() {
            full
        } else {
            logs.join("transcript.jsonl")
        };
        if path.is_file() {
            let modified = fs::metadata(&path).and_then(|m| m.modified());
            keep_recent(path, modified, cutoff, &mut out, st);
        }
    }
    out.sort();
    out
}

/// The top-level `timestamp` of a transcript's first record that has one, or `None`
/// when no record has one or the file cannot be read.
pub(super) fn first_time(path: &Path) -> Option<Timestamp> {
    first_time_in(open(path).ok()?)
}

/// The top-level `timestamp` of the first record in `reader` that has one.
pub(super) fn first_time_in(reader: Box<dyn BufRead>) -> Option<Timestamp> {
    let mut lines = Lines::new(reader);
    while let Ok(Some(line)) = lines.next_line() {
        let value = serde_json::from_str::<Value>(line.trim_end()).ok();
        let t = value.as_ref().and_then(|v| v.get("timestamp"));
        if let Some(t) = t.and_then(Value::as_str).and_then(parse_ts) {
            return Some(t);
        }
    }
    None
}

/// The longest line read, in bytes. A decoded `*.jsonl.zst` line is not bounded by
/// the file's size: a small frame can decode to gigabytes with no newline.
pub(super) const MAX_LINE: u64 = 64 << 20;

/// shortcut: a line is held whole up to `MAX_LINE` bytes, far above the few MB of
/// the longest lines seen in real transcripts; a longer one is skipped unread and
/// counted as a skipped line. Parsing records as a stream would remove the cap.
pub(super) struct Lines {
    reader: Box<dyn BufRead>,
    buf: Vec<u8>,
    /// Lines skipped for being longer than `MAX_LINE`.
    pub(super) over_long: u64,
}

/// Why a transcript could not be opened.
pub(super) enum Open {
    /// A `*.jsonl.zst` file that does not start with the zstd magic number, or is
    /// shorter than it.
    NotZstd,
    /// A file that cannot be opened, or a zstd frame header that cannot be decoded,
    /// and why.
    Io(String),
}

/// Why a zstd frame header could not be read. The decoder's message is not given, as
/// it can quote the file's bytes.
pub(super) const ZSTD_HEADER: &str = "its zstd frame header cannot be read";

/// Why a transcript failed part way. An error the system gives names no file content;
/// any other comes from decoding a `*.jsonl.zst` one, whose message can quote the data,
/// so it is named only as such.
pub(super) fn read_error(e: &io::Error) -> String {
    if e.raw_os_error().is_some() {
        format!("a file cannot be read: {e}")
    } else {
        "its zstd data cannot be decoded (damaged, cut short or more than one frame)".to_owned()
    }
}

/// Opens a transcript, decoding a `*.jsonl.zst` one as it is read.
pub(super) fn open(path: &Path) -> Result<Box<dyn BufRead>, Open> {
    let file = BufReader::new(
        File::open(path).map_err(|e| Open::Io(format!("a file cannot be opened: {e}")))?,
    );
    if path.extension().is_none_or(|x| x != "zst") {
        return Ok(Box::new(file));
    }
    let decoder = StreamingDecoder::new(file).map_err(header_error)?;
    Ok(Box::new(BufReader::new(OneFrame(decoder))))
}

/// Why a zstd frame header could not be read: a file that ends before the magic
/// number is not zstd, but one whose read fails is unreadable.
pub(super) fn header_error(e: FrameDecoderError) -> Open {
    use ReadFrameHeaderError::{BadMagicNumber, MagicNumberReadError};
    let FrameDecoderError::ReadFrameHeaderError(e) = e else {
        return Open::Io(ZSTD_HEADER.to_owned());
    };
    match e {
        BadMagicNumber(_) => Open::NotZstd,
        MagicNumberReadError(e) if e.kind() == io::ErrorKind::UnexpectedEof => Open::NotZstd,
        _ => Open::Io(ZSTD_HEADER.to_owned()),
    }
}

/// Decodes a compressed transcript and fails, rather than stopping quietly, on a bad
/// checksum or anything after the frame.
///
/// shortcut: reads one zstd frame, as Codex writes one per rollout (all 203 on one
/// machine had one, 13 November 2026); a file with more frames counts as unreadable
/// after its first. Decoding each frame in turn would be the upgrade.
pub(super) struct OneFrame(StreamingDecoder<BufReader<File>, FrameDecoder>);

impl Read for OneFrame {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.0.read(buf)?;
        if n == 0 && !buf.is_empty() {
            let frame = &self.0.decoder;
            if let Some(sum) = frame.get_checksum_from_data()
                && frame.get_calculated_checksum() != Some(sum)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd checksum mismatch",
                ));
            }
            if !self.0.get_mut().fill_buf()?.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "data after the zstd frame",
                ));
            }
        }
        Ok(n)
    }
}

impl Lines {
    pub(super) fn new(reader: Box<dyn BufRead>) -> Self {
        Lines {
            reader,
            buf: Vec::new(),
            over_long: 0,
        }
    }

    /// The next line of at most `MAX_LINE` bytes before its newline, skipping and
    /// counting longer ones.
    pub(super) fn next_line(&mut self) -> io::Result<Option<Cow<'_, str>>> {
        loop {
            self.buf.clear();
            let n = (&mut self.reader)
                .take(MAX_LINE)
                .read_until(b'\n', &mut self.buf)?;
            if n == 0 {
                return Ok(None);
            }
            if n as u64 == MAX_LINE && self.buf.last() != Some(&b'\n') {
                // The cap was reached: the line is longer only if more of it follows.
                match self.reader.fill_buf()?.first() {
                    None => {}
                    Some(b'\n') => self.reader.consume(1),
                    Some(_) => {
                        self.reader.skip_until(b'\n')?;
                        self.over_long += 1;
                        self.buf = Vec::new();
                        continue;
                    }
                }
            }
            return Ok(Some(String::from_utf8_lossy(&self.buf)));
        }
    }
}
