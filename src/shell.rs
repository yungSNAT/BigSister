//! Reading commands without running them: words, verb classes, file reads,
//! test pipes and hook labels.

mod codex;
mod pipes;
#[cfg(test)]
mod tests;

use std::iter::Peekable;
use std::str::Chars;

pub use codex::{Step, codex_edit_targets, codex_read_calls, codex_steps, js_unescape};
pub use pipes::{PipeCheck, pipe_check, shows_failure, shows_pass};

/// One simple command, and whether a `|` sends its output to the next one.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<String>,
    pub piped: bool,
}

/// Splits a command at unquoted `|`, `|&`, `||`, `;`, `&`, `&&` and newlines, and
/// each part into words with quotes removed. `>&` and `<&` stay inside a word;
/// `#` comments, heredoc bodies and PowerShell here-string bodies are skipped.
///
/// shortcut: backslash escapes, `$(...)`, backticks and multi-line arrays are not
/// parsed, so a value can split into words or segments and land where a program
/// name is expected. Verb classes and hook labels print only names of programs
/// that exist (see `programs`), so such a value prints as `(other)`; counts can
/// still be off. A real shell parser would be the upgrade.
#[must_use]
pub fn split(cmd: &str) -> Vec<Segment> {
    let mut t = Splitter::default();
    let mut chars = cmd.chars().peekable();
    let mut quote: Option<char> = None;
    let mut prev = '\0';
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => t.word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, '#') if t.word.is_empty() => while chars.next_if(|c| *c != '\n').is_some() {},
            (None, '@') if t.word.is_empty() && opens_here_string(&chars) => {
                let quote = chars.next().unwrap_or('\'');
                skip_here_string(&mut chars, quote);
            }
            (None, '&') if prev == '>' || prev == '<' => t.word.push(c),
            (None, '|') => {
                let pipe = chars.next_if_eq(&'|').is_none();
                if pipe {
                    chars.next_if_eq(&'&');
                }
                t.end_segment(pipe);
            }
            (None, ';' | '&') => t.end_segment(false),
            (None, '\n') => {
                t.end_segment(false);
                for delimiter in std::mem::take(&mut t.heredocs) {
                    skip_heredoc(&mut chars, &delimiter);
                }
            }
            (None, c) if c.is_whitespace() => t.end_word(),
            (None, c) => t.word.push(c),
        }
        prev = c;
    }
    t.end_segment(false);
    t.segments
}

/// The words of each segment of a command.
#[must_use]
pub fn segments(cmd: &str) -> Vec<Vec<String>> {
    split(cmd).into_iter().map(|s| s.words).collect()
}

#[derive(Default)]
struct Splitter {
    segments: Vec<Segment>,
    words: Vec<String>,
    word: String,
    /// Delimiters of heredocs opened on the current line.
    heredocs: Vec<String>,
    delimiter_next: bool,
}

impl Splitter {
    fn end_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        let w = std::mem::take(&mut self.word);
        if std::mem::take(&mut self.delimiter_next) {
            self.heredocs.push(w.clone());
        } else if let Some(rest) = w
            .find("<<")
            .map(|i| &w[i + 2..])
            .filter(|r| !r.starts_with('<'))
        {
            let rest = rest.strip_prefix('-').unwrap_or(rest);
            if rest.is_empty() {
                self.delimiter_next = true;
            } else if rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-.".contains(c))
            {
                // Not `$((1<<2))`, whose "delimiter" would skip the rest of the command.
                self.heredocs.push(rest.to_owned());
            }
        }
        self.words.push(w);
    }

    fn end_segment(&mut self, piped: bool) {
        self.end_word();
        if !self.words.is_empty() {
            let words = std::mem::take(&mut self.words);
            self.segments.push(Segment { words, piped });
        }
    }
}

/// Skips a heredoc body up to and including its delimiter line.
fn skip_heredoc(chars: &mut Peekable<Chars<'_>>, delimiter: &str) {
    let mut line = String::new();
    for c in chars.by_ref() {
        if c != '\n' {
            line.push(c);
        } else if line.trim() == delimiter {
            return;
        } else {
            line.clear();
        }
    }
}

/// Whether `@` opens a PowerShell here-string: a quote, then the end of the line.
fn opens_here_string(chars: &Peekable<Chars<'_>>) -> bool {
    let mut ahead = chars.clone();
    matches!(ahead.next(), Some('\'' | '"')) && matches!(ahead.next(), Some('\n' | '\r'))
}

/// Skips a here-string body up to its closing quote and `@` at the start of a line.
fn skip_here_string(chars: &mut Peekable<Chars<'_>>, quote: char) {
    let mut line_start = false;
    while let Some(c) = chars.next() {
        if line_start && c == quote && chars.next_if_eq(&'@').is_some() {
            return;
        }
        line_start = c == '\n';
    }
}

/// A shell assignment such as `VAR=value`.
fn is_assignment(w: &str) -> bool {
    w.find('=').is_some_and(|i| {
        i > 0
            && w[..i]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The last path component, keeping its extension.
fn file_name(w: &str) -> &str {
    w.rsplit(['/', '\\']).next().unwrap_or(w)
}

/// A program name: last path component, lower-cased, without `.exe`.
pub(crate) fn program(w: &str) -> String {
    let b = file_name(w).to_lowercase();
    b.strip_suffix(".exe").map_or(b.clone(), str::to_owned)
}

/// Whether a segment is a PowerShell assignment such as `$x = 1` or
/// `$env:KEY='v'`. Its value is never read as a command, so it cannot reach the
/// output.
fn is_ps_assignment(words: &[String]) -> bool {
    words.first().is_some_and(|w| w.starts_with('$'))
        && (words[0].contains('=')
            || words
                .get(1)
                .is_some_and(|n| n.starts_with('=') || (n.len() <= 3 && n.ends_with('='))))
}

/// The words after leading `VAR=value` assignments; none for a PowerShell
/// assignment.
fn after_assignments(words: &[String]) -> Vec<&str> {
    if is_ps_assignment(words) {
        return Vec::new();
    }
    words
        .iter()
        .map(String::as_str)
        .skip_while(|w| is_assignment(w))
        .collect()
}

/// A segment's words after `VAR=value` assignments and the prefixes `rtk`,
/// `rtk bypass`, `rtk proxy`, `rtk test`, `rtk err`, `sudo` and `time` with
/// their options.
fn command_words(words: &[String]) -> Vec<&str> {
    let words = after_assignments(words);
    let mut i = 0;
    while let Some(w) = words.get(i) {
        let options_with_value: &[&str] = match program(w).as_str() {
            "rtk" => {
                i += 1;
                if words
                    .get(i)
                    .is_some_and(|w| matches!(*w, "bypass" | "proxy" | "test" | "err"))
                {
                    i += 1;
                }
                continue;
            }
            "sudo" => &[
                "-u",
                "-g",
                "-C",
                "-h",
                "-p",
                "-U",
                "-r",
                "-t",
                "-D",
                "-T",
                "--user",
                "--group",
                "--close-from",
                "--host",
                "--prompt",
                "--other-user",
                "--role",
                "--type",
                "--chdir",
                "--command-timeout",
            ],
            "time" => &["-f", "-o", "--format", "--output"],
            _ => break,
        };
        i += 1;
        while let Some(o) = words.get(i).filter(|o| o.starts_with('-')) {
            i += if options_with_value.contains(o) { 2 } else { 1 };
        }
    }
    words[i.min(words.len())..].to_vec()
}

/// The first segment that is not a leading `cd` or only assignments.
fn main_segment(segs: &[Segment]) -> Option<&Segment> {
    let mut i = 0;
    while i + 1 < segs.len()
        && after_assignments(&segs[i].words)
            .first()
            .is_none_or(|w| program(w) == "cd")
    {
        i += 1;
    }
    segs.get(i)
}

/// Whether a word, as written, names a program on this machine.
pub type Known<'a> = dyn FnMut(&str) -> bool + 'a;

/// A command's verb class, such as `cargo`, `get-content` or `ssh agent-heavy`.
/// The program word must be one that `known` accepts, otherwise the class is
/// `(other)`: parsing is heuristic, and a value can land in a program's place.
#[must_use]
pub fn verb_class(cmd: &str, known: &mut Known<'_>) -> String {
    let segs = split(cmd);
    match main_segment(&segs) {
        None => "(empty)".to_owned(),
        Some(seg) if after_assignments(&seg.words).is_empty() => "(assignment)".to_owned(),
        Some(seg) => class_of(&command_words(&seg.words), known),
    }
}

fn class_of(words: &[&str], known: &mut Known<'_>) -> String {
    let Some(first) = words.first() else {
        return "(empty)".to_owned();
    };
    let v = program(first);
    if first.contains("://") || !is_program_name(&v) || !known(first) {
        return "(other)".to_owned();
    }
    if v != "ssh" {
        return v;
    }
    // The host is not printed: a misparse can put a value in its place.
    match ssh_remote(&words[1..]) {
        Some(remote) if !remote.is_empty() => format!("ssh {}", verb_class(&remote, known)),
        _ => "ssh".to_owned(),
    }
}

/// The remote command in `ssh` arguments, after the options and the host.
fn ssh_remote(args: &[&str]) -> Option<String> {
    const WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut i = 0;
    while let Some(a) = args.get(i).filter(|a| a.starts_with('-')) {
        i += if a.len() == 2 && WITH_VALUE.contains(&a[1..]) {
            2
        } else {
            1
        };
    }
    args.get(i)?;
    let remote: Vec<&str> = args
        .get(i + 1..)
        .unwrap_or_default()
        .iter()
        .copied()
        .skip_while(|w| w.starts_with('-'))
        .collect();
    Some(remote.join(" "))
}

const READERS: [&str; 9] = [
    "cat",
    "head",
    "tail",
    "sed",
    "less",
    "more",
    "bat",
    "get-content",
    "gc",
];
const PATH_FLAGS: [&str; 3] = ["-path", "-literalpath", "-lp"];

fn value_flags(verb: &str) -> &'static [&'static str] {
    match verb {
        "head" | "tail" => &[
            "-n",
            "-c",
            "-s",
            "--lines",
            "--bytes",
            "--sleep-interval",
            "--pid",
        ],
        "sed" => &["-e", "-f", "--expression", "--file"],
        "bat" => &[
            "-l",
            "--language",
            "-r",
            "--line-range",
            "--highlight-line",
            "--theme",
            "--style",
            "-m",
            "--map-syntax",
        ],
        "get-content" | "gc" => &[
            "-totalcount",
            "-head",
            "-tail",
            "-first",
            "-last",
            "-encoding",
            "-readcount",
            "-delimiter",
            "-stream",
            "-erroraction",
            "-ea",
            "-warningaction",
            "-wa",
        ],
        _ => &[],
    }
}

/// Files a command reads with a reader such as `cat`, `sed -n` or `Get-Content`.
#[must_use]
pub fn read_targets(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    for seg in segments(cmd) {
        // `$text = Get-Content x.md` reads x.md, though the segment is an assignment.
        let words: Vec<&str> = if is_ps_assignment(&seg) && seg.get(1).is_some_and(|w| w == "=") {
            seg[2..].iter().map(String::as_str).collect()
        } else {
            after_assignments(&seg)
        };
        let Some(first) = words.first() else { continue };
        let verb = program(first);
        if !READERS.contains(&verb.as_str()) {
            continue;
        }
        if verb == "sed"
            && words
                .iter()
                .any(|w| w.starts_with("-i") || w.starts_with("--in-place"))
        {
            continue;
        }
        out.extend(reader_args(&verb, &words[1..]));
    }
    out
}

fn reader_args(verb: &str, args: &[&str]) -> Vec<String> {
    let flags = value_flags(verb);
    let script_given = args.iter().any(|w| {
        flags
            .iter()
            .any(|f| w == f || w.starts_with(&format!("{f}=")))
    });
    let mut script_pending = verb == "sed" && !script_given;
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(w) = args.get(i) {
        let lw = w.to_lowercase();
        i += 1;
        if PATH_FLAGS.contains(&lw.as_str()) {
            out.extend(
                args.get(i)
                    .filter(|p| is_literal_path(p))
                    .map(|p| (*p).to_owned()),
            );
            i += 1;
        } else if flags.contains(&lw.as_str()) || w.ends_with('>') || matches!(*w, "<<" | "<<-") {
            i += 1;
        } else if w.starts_with(['-', '+'])
            || w.contains(['>', '<'])
            || w.chars().all(|c| c.is_ascii_digit())
        {
        } else if script_pending {
            script_pending = false;
        } else if is_literal_path(w) {
            out.push((*w).to_owned());
        }
    }
    out
}

/// A program name such as `git`, `get-content` or `python3.12`, as opposed to a
/// variable, expression or quoted text in the first word.
fn is_program_name(v: &str) -> bool {
    v.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

/// A path written out, not a variable or expression whose value is unknown.
fn is_literal_path(w: &str) -> bool {
    !w.contains('$') && !w.starts_with(['(', '@', '{'])
}

/// A path as compared for re-reads: forward slashes, lower case.
#[must_use]
pub fn normalise_path(p: &str) -> String {
    p.trim_matches(['"', '\'', '`'])
        .replace('\\', "/")
        .to_lowercase()
}

const SCRIPT_EXTENSIONS: [&str; 9] = [
    ".js", ".mjs", ".cjs", ".py", ".exe", ".ps1", ".cmd", ".sh", ".bat",
];

/// Programs whose first script argument is the script they run.
const INTERPRETERS: [&str; 13] = [
    "bash",
    "bun",
    "cmd",
    "deno",
    "node",
    "powershell",
    "pwsh",
    "py",
    "python",
    "python3",
    "sh",
    "uv",
    "zsh",
];

/// Whether `arg` is an option with which `interpreter` runs code given on the
/// command line, so no later argument is the script being run. Short options may be
/// grouped, as in `bash -lc`; only PowerShell's options ignore case.
fn runs_inline_code(interpreter: &str, arg: &str) -> bool {
    let flag = arg.split('=').next().unwrap_or_default();
    let short_group_ending = |letters: &str| {
        flag.len() > 1
            && flag.starts_with('-')
            && !flag.starts_with("--")
            && flag.ends_with(|c| letters.contains(c))
    };
    match interpreter {
        "node" | "bun" | "deno" => ["-e", "-p", "--eval", "--print"].contains(&flag),
        "python" | "python3" | "py" => short_group_ending("cm"),
        "bash" | "sh" | "zsh" => short_group_ending("c"),
        "powershell" | "pwsh" => {
            ["-c", "-command", "-ec", "-encodedcommand"].contains(&flag.to_lowercase().as_str())
        }
        _ => false,
    }
}

/// A hook's label: the file name of its script, or its program when there is no
/// script. The script is the program itself or an interpreter's first script
/// argument before any option that runs inline code (`runs_inline_code`). Either
/// must be a word that `known` accepts, or the label is `(other)`.
/// Assignments and a leading `cd` are skipped, and arguments never print.
///
/// Recent Claude Code versions record `command hook N` (the hook's position under
/// its event and matcher) or its status message instead of the command; the first
/// is kept whole.
#[must_use]
pub fn hook_label(command: &str, known: &mut Known<'_>) -> String {
    if command
        .strip_prefix("command hook ")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    {
        return command.to_owned();
    }
    let segs = split(command);
    let words = main_segment(&segs)
        .map(|s| after_assignments(&s.words))
        .unwrap_or_default();
    // Only plain names reach the output; anything else could be part of a value.
    let plain = |w: &str| {
        !w.is_empty()
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
    };
    let is_script = |w: &str| {
        let n = file_name(w).to_lowercase();
        plain(file_name(w)) && SCRIPT_EXTENSIONS.iter().any(|e| n.ends_with(e))
    };
    let script = match words.first() {
        Some(w) if is_script(w) => Some(w),
        Some(w) if INTERPRETERS.contains(&program(w).as_str()) => {
            let interpreter = program(w);
            words[1..]
                .iter()
                .take_while(|a| !runs_inline_code(&interpreter, a))
                .find(|a| is_script(a))
        }
        _ => None,
    };
    if let Some(script) = script.filter(|s| known(s)) {
        return file_name(script).to_owned();
    }
    match words.first() {
        None => "(empty)".to_owned(),
        Some(w) if plain(file_name(w)) && known(w) => file_name(w).to_owned(),
        Some(_) => "(other)".to_owned(),
    }
}
