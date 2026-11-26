//! Codex `exec` cells: the commands, reads and edits their JavaScript runs.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::Regex;

use super::is_literal_path;

// shortcut: Codex cells are read with regexes, not a JavaScript parser. A `cmd:` key
// whose value is one string literal counts as a command (`whole_literal`); a `cmd`
// built from variables or other values is missed, and calls inside strings and
// comments count. Strings must count: Codex passes code that runs to REPL tools as a
// string. Patch bodies are skipped (`PATCH_BODY`).
static CMD_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)\bcmd["']?\s*:\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)"#)
        .expect("valid regex")
});
static READ_CALL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)\breadFile(?:Sync)?\(\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`)"#,
    )
    .expect("valid regex")
});
/// The text an `apply_patch` call writes, from the string literal that opens it. A
/// `readFile(` line it adds is not a read.
static PATCH_BODY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)(["'`])\*\*\* Begin Patch.*?\*\*\* End Patch"#).expect("valid regex")
});

/// One step of a Codex tool call, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Command(String),
    Edit,
}

const EDIT_CALLS: [&str; 3] = ["apply_patch(", "writeFile(", "writeFileSync("];

/// `input` with each patch body after its opening quote blanked to spaces of the
/// same length, so offsets still give source order. A quote after `//` on its line
/// is in a comment and opens no body.
///
/// shortcut: a `//` straight after `:` is taken for a URL, and `/* */` comments are
/// not recognised; a JavaScript lexer would settle both.
fn without_patch_bodies(input: &str) -> Cow<'_, str> {
    PATCH_BODY.replace_all(input, |c: &regex::Captures<'_>| {
        let whole = c.get(0).expect("the whole match");
        let line_start = input[..whole.start()].rfind('\n').map_or(0, |i| i + 1);
        let line = &input[line_start..whole.start()];
        if line
            .match_indices("//")
            .any(|(i, _)| !line[..i].ends_with(':'))
        {
            whole.as_str().to_owned()
        } else {
            format!("{}{}", &c[1], " ".repeat(whole.len() - c[1].len()))
        }
    })
}

/// The shell commands (each `cmd` string literal, unescaped) and file edits of a
/// Codex `exec` cell, in source order.
#[must_use]
pub fn codex_steps(input: &str) -> Vec<Step> {
    let input = without_patch_bodies(input);
    let input = input.as_ref();
    let mut steps: Vec<(usize, Step)> = CMD_LITERAL
        .captures_iter(input)
        .filter_map(|c| c.get(1))
        .filter(|m| whole_literal(input, m))
        .map(|m| (m.start(), Step::Command(js_unescape(m.as_str()))))
        .collect();
    for call in EDIT_CALLS {
        steps.extend(input.match_indices(call).map(|(i, _)| (i, Step::Edit)));
    }
    steps.sort_by_key(|(i, _)| *i);
    steps.into_iter().map(|(_, s)| s).collect()
}

fn js_call_literals<'a>(input: &'a str, re: &'a Regex) -> impl Iterator<Item = String> + 'a {
    re.captures_iter(input)
        .filter_map(|c| c.get(1))
        .filter(|m| whole_literal(input, m))
        .map(|m| js_unescape(m.as_str()))
}

/// Paths a Codex `exec` cell reads with `readFile` or `readFileSync`.
#[must_use]
pub fn codex_read_calls(input: &str) -> Vec<String> {
    let input = without_patch_bodies(input);
    js_call_literals(&input, &READ_CALL)
        .filter(|p| is_literal_path(p))
        .collect()
}

/// Returns the index straight after the closing quote of a JS string literal starting at
/// `start`, or `None` when the literal runs to the end of `bytes` without closing.
fn scan_js_string(bytes: &[u8], start: usize) -> Option<usize> {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == quote {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Scans template-literal text from `start` (after `` ` `` or a closing `}` of `${...}`),
/// stopping after an unescaped `${` (pushing brace depth `0`), after the closing `` ` ``,
/// or at end of input.
fn advance_template(bytes: &[u8], mut i: usize, template_braces: &mut Vec<usize>) -> usize {
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'`' {
            return i + 1;
        }
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            template_braces.push(0);
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

/// If `input[after_ident..]` opens a call whose first argument is a JS string literal,
/// returns `(next_cursor, Some(unescaped))` when the literal is a whole value, or
/// `(next_cursor, None)` when it is composite or unterminated (returning `None` for
/// interpolated template literals so the outer scanner still visits `${...}`).
fn call_first_string_arg(input: &str, after_ident: usize) -> Option<(usize, Option<String>)> {
    let bytes = input.as_bytes();
    let j = after_ident
        + input[after_ident..]
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .count();
    if bytes.get(j) != Some(&b'(') {
        return None;
    }
    let k = j
        + 1
        + input[j + 1..]
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .count();
    if !matches!(bytes.get(k), Some(b'"' | b'\'' | b'`')) {
        return None;
    }
    let end = scan_js_string(bytes, k);
    if bytes[k] == b'`' && substitutes(&input[k..end.unwrap_or(bytes.len())]) {
        return None;
    }
    let Some(end) = end else {
        return Some((bytes.len(), None));
    };
    let val = is_whole_lit(input, k, end).then(|| js_unescape(&input[k..end]));
    Some((end, val))
}

fn scan_edit_ident(
    input: &str,
    start: usize,
    patches: &mut Vec<String>,
    writes: &mut Vec<String>,
) -> usize {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len()
        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
    {
        i += 1;
    }
    let name = &input[start..i];
    if matches!(name, "apply_patch" | "writeFile" | "writeFileSync")
        && let Some((next, val)) = call_first_string_arg(input, i)
    {
        if let Some(val) = val {
            if name == "apply_patch" {
                patches.push(val);
            } else if is_literal_path(&val) {
                writes.push(val);
            }
        }
        return next;
    }
    i
}

/// Patch texts from `apply_patch(...)` and literal paths from `writeFile(...)` /
/// `writeFileSync(...)` in a Codex `exec` cell, skipping JS comments and non-interpolated
/// string/template text whilst scanning executable `${...}` template expressions.
///
/// shortcut: JavaScript regex literals (such as `/}/` inside `${...}`) are not lexed,
/// matching the rest of this non-AST Codex cell scanner.
#[must_use]
pub fn codex_edit_targets(input: &str) -> (Vec<String>, Vec<String>) {
    let bytes = input.as_bytes();
    let (mut patches, mut writes) = (Vec::new(), Vec::new());
    let mut template_braces = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i = bytes[i + 2..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |p| i + 2 + p + 1);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = input[i + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |p| i + 2 + p + 2);
            }
            b'"' | b'\'' => i = scan_js_string(bytes, i).unwrap_or(bytes.len()),
            b'`' => i = advance_template(bytes, i + 1, &mut template_braces),
            b'{' => {
                if let Some(depth) = template_braces.last_mut() {
                    *depth += 1;
                }
                i += 1;
            }
            b'}' => match template_braces.last_mut() {
                Some(depth) if *depth > 0 => {
                    *depth -= 1;
                    i += 1;
                }
                Some(_) => {
                    template_braces.pop();
                    i = advance_template(bytes, i + 1, &mut template_braces);
                }
                None => i += 1,
            },
            b if b.is_ascii_alphabetic() || b == b'_' || b == b'$' => {
                i = scan_edit_ident(input, i, &mut patches, &mut writes);
            }
            _ => i += 1,
        }
    }
    (patches, writes)
}

/// Whether the string literal `lit` in `input` is a whole value rather than part of
/// one built from other values: no `${}` substitution in a template literal, and
/// nothing after it but a comma, a closing bracket, a semicolon, a comment or the end.
/// A literal joined to a value with `+` or a method would otherwise stand for every
/// command built from it.
fn whole_literal(input: &str, lit: &regex::Match<'_>) -> bool {
    is_whole_lit(input, lit.start(), lit.end())
}

fn is_whole_lit(input: &str, start: usize, end: usize) -> bool {
    let text = &input[start..end];
    let rest = input[end..].trim_start();
    !(text.starts_with('`') && substitutes(text))
        && (rest.is_empty()
            || rest.starts_with([',', ')', ']', '}', ';'])
            || rest.starts_with("//")
            || rest.starts_with("/*"))
}

/// Whether template literal text holds a `${` that is not escaped.
fn substitutes(template: &str) -> bool {
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '$' if chars.as_str().starts_with('{') => return true,
            _ => {}
        }
    }
    false
}

/// The value of a JavaScript string literal, quotes included in `lit`.
#[must_use]
pub fn js_unescape(lit: &str) -> String {
    let inner = lit.get(1..lit.len().saturating_sub(1)).unwrap_or_default();
    let mut out = String::with_capacity(inner.len());
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('x') => out.extend(hex_char(&mut it, 2)),
            Some('u') if it.peek() == Some(&'{') => {
                it.next();
                let hex: String = it.by_ref().take_while(|c| *c != '}').collect();
                out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
            }
            Some('u') => out.extend(hex_char(&mut it, 4)),
            Some('\n') => {}
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn hex_char(it: &mut impl Iterator<Item = char>, n: usize) -> Option<char> {
    let hex: String = it.take(n).collect();
    u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
}
