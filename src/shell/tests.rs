
use super::*;

/// Parsing alone: every word counts as a known program.
fn vc(cmd: &str) -> String {
    verb_class(cmd, &mut |_| true)
}

fn hl(cmd: &str) -> String {
    hook_label(cmd, &mut |_| true)
}

#[test]
fn segments_split_on_operators_but_not_in_quotes_or_redirects() {
    let s = segments("cd /x && cargo test 2>&1 | tail -5; echo 'a | b'");
    assert_eq!(
        s,
        vec![
            vec!["cd", "/x"],
            vec!["cargo", "test", "2>&1"],
            vec!["tail", "-5"],
            vec!["echo", "a | b"]
        ]
    );
}

#[test]
fn split_marks_only_single_pipes() {
    let piped: Vec<bool> = split("a | b || c |& d && e")
        .iter()
        .map(|s| s.piped)
        .collect();
    assert_eq!(piped, vec![true, false, true, false, false]);
}

#[test]
fn heredoc_bodies_are_not_commands() {
    let cmd = "git commit -F - <<'EOF'\nMore notes here\ncargo test | tail\nEOF\ncat a.md";
    assert_eq!(
        segments(cmd),
        vec![
            vec!["git", "commit", "-F", "-", "<<EOF"],
            vec!["cat", "a.md"]
        ]
    );
    assert_eq!(read_targets(cmd), vec!["a.md"]);
    assert!(!pipe_check(cmd).runner);
    assert_eq!(
        segments("cat << -X\nbody\n-X\nls"),
        vec![vec!["cat", "<<", "-X"], vec!["ls"]]
    );
}

#[test]
fn verb_classes() {
    assert_eq!(
        vc("cd /c/x && RUST_LOG=1 rtk bypass cargo test --locked"),
        "cargo"
    );
    assert_eq!(
        vc("ssh -o BatchMode=yes l@ll 'agent-heavy status'"),
        "ssh agent-heavy"
    );
    assert_eq!(vc("ssh ll"), "ssh");
    // A wrapper with no command after it runs nothing.
    assert_eq!(vc("sudo -u me"), "(empty)");
    assert_eq!(vc("rtk bypass"), "(empty)");
    assert_eq!(vc(r#""C:\Program Files\Git\bin\git.exe" status"#), "git");
    assert_eq!(vc("Get-Content -Raw x.md"), "get-content");
    assert_eq!(vc("sudo -u root agent-heavy status"), "agent-heavy");
    assert_eq!(vc("sudo --user root -- agent-heavy"), "agent-heavy");
    assert_eq!(vc("time --output t.txt cargo build"), "cargo");
    assert_eq!(vc("time -p cargo build"), "cargo");
    assert_eq!(vc("OUT=/tmp/zz; sleep 5"), "sleep");
    assert_eq!(vc("$x=1; & git status"), "git");
    assert_eq!(vc("$files.Count"), "(other)");
    assert_eq!(vc("rtk test cargo test"), "cargo");
    assert_eq!(vc(""), "(empty)");
}

#[test]
fn assignment_values_never_become_classes() {
    for cmd in [
        "$x = 'sentinel'",
        "$stamp = Get-Date",
        "$env:NAME=sentinel",
        "$n += 1",
        "$i = 0",
        "X=1",
    ] {
        assert_eq!(vc(cmd), "(assignment)", "{cmd}");
    }
    assert_eq!(vc("$env:K = 'v'; cargo test"), "cargo");
}

#[test]
fn odd_first_words_and_hosts_are_other() {
    assert_eq!(vc("https://example.test/sentinel"), "(other)");
    assert_eq!(vc("ssh 'me@sentinel host' ls"), "ssh ls");
    assert_eq!(vc("ssh $HOST ls"), "ssh ls");
    assert_eq!(vc("ssh ll -- cargo test"), "ssh cargo");
    // A flag that takes no value is passed over alone, one that takes one with it.
    assert_eq!(vc("ssh -t -p 22 ll git status"), "ssh git");
    assert_eq!(vc(r#"RUST_LOG="a b" cargo test"#), "cargo");
}

#[test]
fn comments_and_here_strings_are_skipped() {
    assert_eq!(
        segments("cargo test # sentinel note\nls"),
        vec![vec!["cargo", "test"], vec!["ls"]]
    );
    for q in ['\'', '"'] {
        let cmd = format!("$body = @{q}\nsentinel | text\n{q}@\ngit status");
        assert_eq!(
            segments(&cmd),
            vec![vec!["$body", "="], vec!["git", "status"]],
            "{cmd}"
        );
    }
    assert_eq!(
        segments("cat<<EOF\nsentinel\nEOF\nls"),
        vec![vec!["cat<<EOF"], vec!["ls"]]
    );
    assert_eq!(
        segments("echo $((1<<2))\nls"),
        vec![vec!["echo", "$((1<<2))"], vec!["ls"]]
    );
}

#[test]
fn read_targets_cover_common_readers() {
    assert_eq!(
        read_targets("cat a.md b.md | head -20"),
        vec!["a.md", "b.md"]
    );
    assert_eq!(
        read_targets("sed -n '1,40p' src/lib.rs"),
        vec!["src/lib.rs"]
    );
    assert_eq!(
        read_targets("head -n 30 notes.txt && tail -5 log.txt"),
        vec!["notes.txt", "log.txt"]
    );
    assert_eq!(
        read_targets(r"Get-Content -Raw -LiteralPath 'C:\hub\x.md' -TotalCount 50"),
        vec![r"C:\hub\x.md"]
    );
    assert_eq!(read_targets("gc -Encoding utf8 y.md"), vec!["y.md"]);
    assert_eq!(
        read_targets("Get-Content z.md -ErrorAction SilentlyContinue"),
        vec!["z.md"]
    );
}

#[test]
fn read_targets_skip_writes_and_searches() {
    assert!(read_targets("sed -i 's/a/b/' x.rs").is_empty());
    assert!(read_targets("cat <<'EOF' > out.txt").is_empty());
    assert!(read_targets("cat > out.txt").is_empty());
    assert!(read_targets("rg -n foo src").is_empty());
    assert!(read_targets("type cargo").is_empty());
    assert!(read_targets("cat $f ${dir}/x.md").is_empty());
    assert!(read_targets("Get-Content -LiteralPath $p").is_empty());
    assert!(read_targets("cat << EOF\nbody\nEOF").is_empty());
    assert!(read_targets("cat << -X\nbody\n-X").is_empty());
    assert!(read_targets("sed --in-place=.bak 's/a/b/' x.rs").is_empty());
}

#[test]
fn read_targets_skip_option_values() {
    assert_eq!(read_targets("bat --theme zqx a.md"), vec!["a.md"]);
    assert_eq!(read_targets("less +F b.md"), vec!["b.md"]);
    assert_eq!(read_targets("tail -s 5 c.md"), vec!["c.md"]);
    assert!(read_targets("cat <<'END-X'\ncat body.md\nEND-X").is_empty());
    assert_eq!(read_targets("sed --expression=1p a.md"), vec!["a.md"]);
    assert_eq!(read_targets("bat -l rust x.rs"), vec!["x.rs"]);
    assert_eq!(read_targets("$text = Get-Content x.md"), vec!["x.md"]);
}

#[test]
fn pipe_checks() {
    let c = pipe_check("cargo test 2>&1 | tail -5");
    assert_eq!(
        c,
        PipeCheck {
            runner: true,
            piped: true,
            remote: false,
            guarded: false
        }
    );
    assert!(pipe_check("set -o pipefail; pytest -q | tail -3").guarded);
    assert!(pipe_check("cd x && npx vitest run | head -20").piped);
    assert!(!pipe_check("git log | head").runner);
    assert!(!pipe_check("cargo test --locked").piped);
    assert!(!pipe_check("cargo test > out.txt; tail out.txt").piped);
    assert!(!pipe_check("cargo test || tail log").piped);
    for wrapped in [
        "agent-heavy --wait 900 -- cargo test --locked 2>&1 | tail -3",
        "nice -n 19 pytest -q | tail",
        "timeout -k 5 10m cargo nextest run | head",
        "env RUST_LOG=1 rtk test cargo test | tail -5",
        "rtk test cargo test | tail",
        "for c in a b; do cargo test -p $c | tail -2; done",
    ] {
        assert!(pipe_check(wrapped).piped, "{wrapped}");
    }
    assert!(!pipe_check("echo cargo test | tail").runner);
    assert!(!pipe_check("agent-heavy status | tail").runner);
}

#[test]
fn pipe_checks_ignore_quoted_text() {
    assert!(!pipe_check(r#"git commit -m "fix cargo test | tail output""#).runner);
    let local = pipe_check("cargo test -p ssh-config 2>&1 | tail -3");
    assert!(local.piped && !local.remote);
    assert!(!pipe_check("cat ~/.ssh/config | head").runner);
}

#[test]
fn pipe_checks_over_ssh() {
    let remote = pipe_check("ssh ll 'cargo test | tail'");
    assert!(remote.runner && remote.piped && remote.remote);
    let local = pipe_check("ssh ll 'cargo test' | tail -3");
    assert!(local.runner && local.piped && !local.remote);
    let unpiped = pipe_check("ssh -p 22 ll cargo test");
    assert!(unpiped.runner && !unpiped.piped);
}

#[test]
fn failure_and_pass_markers() {
    assert!(shows_failure("test result: FAILED. 1 passed; 2 failed"));
    assert!(shows_failure("thread 'x' panicked at src/lib.rs:3"));
    assert!(!shows_failure("0 failed"));
    assert!(!shows_failure("test result: ok. 4 passed; 0 failed"));
    assert!(shows_pass("test result: ok. 4 passed"));
}

#[test]
fn hook_labels() {
    assert_eq!(
        hl(r#""C:\Users\x\.claude\hooks\guard.exe" --ccd-pr"#),
        "guard.exe"
    );
    assert_eq!(
        hl("node C:/hub/.scripts/hub-digest.js --codex"),
        "hub-digest.js"
    );
    assert_eq!(hl("rtk hook claude"), "rtk");
    assert_eq!(hl("node x.js --config=C:/secret/dir"), "x.js");
    assert_eq!(hl(""), "(empty)");
    // An interpreter with no inline-code option, such as `uv` or `cmd`, runs its
    // script argument.
    assert_eq!(hl("uv run -q hooks/check.py --fast"), "check.py");
    assert_eq!(hl("cmd /c guard.cmd"), "guard.cmd");
}

#[test]
fn hook_labels_hide_assignments_and_arguments() {
    assert_eq!(hl("API_KEY=sk-SECRET rtk hook claude"), "rtk");
    assert_eq!(hl("cd /x && node h.js"), "h.js");
    assert_eq!(hl("curl https://example.test/SECRET"), "curl");
    assert_eq!(hl("TOKEN=abc"), "(empty)");
    assert_eq!(hl("command hook 12"), "command hook 12");
    assert_eq!(hl("Loading puritanical mode..."), "Loading");
    assert_eq!(hl("echo /abs/private.js"), "echo");
    assert_eq!(hl("cat notes.py"), "cat");
    assert_eq!(hl("node a.js b.py"), "a.js");
    assert_eq!(hl("node --eval 'console.log(1)' private.py"), "node");
    assert_eq!(hl("python -m pytest t.py"), "python");
    assert_eq!(hl("pwsh -NoProfile -Command x.ps1"), "pwsh");
    assert_eq!(hl("node -r dotenv/config x.js"), "x.js");
    assert_eq!(hl("node -e 'x' a.js"), "node");
    assert_eq!(hl("bash -e hooks/guard.sh"), "guard.sh");
    assert_eq!(hl("bash -lc 'x' run.sh"), "bash");
    assert_eq!(hl("python -E guard.py"), "guard.py");
    assert_eq!(hl("pwsh -File x.ps1"), "x.ps1");
}

#[test]
fn hook_labels_keep_only_plain_names() {
    assert_eq!(hl(r#"bash -c "TOKEN=sentinel node x.js""#), "bash");
    assert_eq!(hl("$env:K='sentinel'; node h.js"), "h.js");
    assert_eq!(hl(r#""echo sentinel""#), "(other)");
    assert_eq!(hl("node 'a sentinel/x.js'"), "x.js");
    assert_eq!(hl("node x.js --a$b"), "x.js");
}

#[test]
fn codex_cells() {
    let input = r#"const r = await tools.exec_command({cmd: "Get-Content -Raw 'C:\\hub\\a.md'"});
const s = readFile('C:\\hub\\b.md'); await tools.apply_patch(p);"#;
    assert_eq!(
        codex_steps(input),
        vec![
            Step::Command(r"Get-Content -Raw 'C:\hub\a.md'".to_owned()),
            Step::Edit
        ]
    );
    assert_eq!(codex_read_calls(input), vec![r"C:\hub\b.md"]);
    assert!(codex_read_calls("readFile(`${dir}/x.md`)").is_empty());
    assert_eq!(
        codex_steps(r#"tools.exec_command({"cmd": `git status`})"#),
        vec![Step::Command("git status".to_owned())]
    );
}

#[test]
fn values_built_from_other_values_are_not_commands_or_reads() {
    for cell in [
        "tools.exec_command({cmd: 'cargo test ' + name});",
        "tools.exec_command({cmd: `cargo test ${name}`});",
        "tools.exec_command({cmd: 'git log -'.concat(n)});",
        "tools.exec_command({cmd:\n  \"rg \"\n  + pattern, yield_time_ms: 1000});",
        "readFile('docs/' + name); readFileSync(`${dir}`, 'utf8');",
    ] {
        assert!(codex_steps(cell).is_empty(), "{cell}");
        assert!(codex_read_calls(cell).is_empty(), "{cell}");
    }
    // A literal that is the whole value still counts, before a comma, a closing
    // bracket, a comment or the end of the cell; an escaped `${` is text.
    let whole = "x({cmd: `echo \\${HOME}`, yield_time_ms: 5}); y({cmd: 'ls' /* c */});\n\
            z({cmd: 'pwd' // c\n}); readFile('a.md', 'utf8'); w({cmd: 'git status'";
    assert_eq!(
        codex_steps(whole),
        ["echo ${HOME}", "ls", "pwd", "git status"]
            .map(|c| Step::Command(c.to_owned()))
            .to_vec()
    );
    assert_eq!(codex_read_calls(whole), vec!["a.md"]);
}

#[test]
fn codex_steps_keep_source_order() {
    let input = r#"await tools.apply_patch(p); await tools.exec_command({cmd: "git status"}); writeFile('x', y);"#;
    assert_eq!(
        codex_steps(input),
        vec![
            Step::Edit,
            Step::Command("git status".to_owned()),
            Step::Edit
        ]
    );
}

#[test]
fn patch_bodies_hold_no_calls() {
    let input = "await tools.apply_patch(`*** Begin Patch\n*** Add File: a.js\n\
            +const c = readFileSync('cfg.json'); writeFile('b', c);\n\
            +await tools.exec_command({cmd: 'git status'});\n*** End Patch`);\n\
            await tools.exec_command({cmd: 'cargo test'}); readFile('real.md');\n\
            writeFile('real_w.rs', 'x'); writeFileSync('real_ws.rs', 'y');\n\
            writeFile('dyn/' + name, 'z'); writeFileSync(`${dir}/dyn.rs`, 'z');";
    assert_eq!(
        codex_steps(input),
        vec![
            Step::Edit,
            Step::Command("cargo test".to_owned()),
            Step::Edit,
            Step::Edit,
            Step::Edit,
            Step::Edit,
        ]
    );
    assert_eq!(codex_read_calls(input), vec!["real.md".to_owned()]);
    let (patches, writes) = codex_edit_targets(input);
    assert_eq!(patches.len(), 1);
    assert!(patches[0].contains("*** Add File: a.js"));
    assert_eq!(writes, vec!["real_w.rs", "real_ws.rs"]);
    // Markers outside a string open no patch body.
    let commented =
        "// *** Begin Patch\nawait tools.exec_command({cmd: 'git status'});\n// *** End Patch";
    assert_eq!(
        codex_steps(commented),
        vec![Step::Command("git status".to_owned())]
    );
    let quoted =
        "// \"*** Begin Patch\nawait tools.exec_command({cmd: 'git status'});\n// *** End Patch";
    assert_eq!(
        codex_steps(quoted),
        vec![Step::Command("git status".to_owned())]
    );
    let after_url = "await fetch('https://x.test'); await tools.apply_patch(`*** Begin Patch\n\
            +readFileSync('cfg.json');\n*** End Patch`);";
    assert_eq!(codex_read_calls(after_url), Vec::<String>::new());
}

#[test]
fn codex_edit_targets_skips_comments_strings_and_nested_patches() {
    let input = concat!(
        "// writeFile('ghost_line.rs', 'x');\n",
        "// apply_patch('*** Begin Patch\\n*** Add File: ghost_line_p.rs\\n*** End Patch');\n",
        "/* writeFileSync('ghost_block.rs', 'x'); apply_patch(`*** Begin Patch\n*** Add File: ghost_block_p.rs\n*** End Patch`); */\n",
        "const note = \"writeFile('ghost_str.rs', 'x')\";\n",
        "const tpl = `apply_patch('*** Begin Patch\\n*** Add File: ghost_tpl.rs\\n*** End Patch')`;\n",
        "writeFile('real_outer.rs', `*** Begin Patch\n+apply_patch('*** Begin Patch\\n*** Add File: ghost_nested.rs\\n*** End Patch')\n*** End Patch`);\n",
        "await tools.apply_patch(\"*** Begin Patch\\n*** Update File: outer.rs\\n+writeFile('ghost_in_patch.rs', 'x')\\n+apply_patch('*** Begin Patch\\\\n*** Add File: inner.rs\\\\n*** End Patch')\\n*** End Patch\");\n",
        "fs.writeFileSync('escaped\\'quote.rs', 'ok');\n",
        "my_writeFile('ignored_ident.rs', 'x');\n",
        "const f = writeFile; writeFile(dynamicPath, 'x');\n",
        "writeFile('dyn/' + part, 'x'); writeFileSync(`${dir}/v.rs`, 'x');\n",
        "writeFile('unterminated",
    );
    let (patches, writes) = codex_edit_targets(input);
    assert_eq!(patches.len(), 1);
    assert!(patches[0].contains("*** Update File: outer.rs"));
    assert_eq!(writes, vec!["real_outer.rs", "escaped'quote.rs"]);
    assert_eq!(
        codex_edit_targets("const broken = 'eof"),
        (Vec::new(), Vec::new())
    );
    assert_eq!(
        codex_edit_targets("/* unterminated comment"),
        (Vec::new(), Vec::new())
    );
    assert_eq!(
        codex_edit_targets("// trailing line comment without newline"),
        (Vec::new(), Vec::new())
    );
    // Executable edits inside `${...}` template interpolations (including nested braces
    // and nested template literals) count, whilst literal template text and `\${...}` do not.
    let tpl_exec = concat!(
        "const a = `${writeFile('interp.rs', 'ok')} writeFile('tpl_lit.rs', 'no') \\${writeFile('esc.rs', 'no')}`;\n",
        "const b = `${(() => { /* writeFile('c.rs', 'no') */ return `${writeFileSync('nested_interp.rs', 'ok')}`; })()} writeFile('after.rs', 'no')`;\n",
        "const c = { k: 1 };\n",
        "const d = `unterminated template",
    );
    let (tpl_patches, tpl_writes) = codex_edit_targets(tpl_exec);
    assert!(tpl_patches.is_empty());
    assert_eq!(tpl_writes, vec!["interp.rs", "nested_interp.rs"]);
    assert_eq!(
        codex_edit_targets("writeFile(`${writeFile('unterm_inner.rs', 'x')}"),
        (Vec::new(), vec!["unterm_inner.rs".to_owned()])
    );
}

#[test]
fn js_unescape_handles_escapes() {
    assert_eq!(
        js_unescape(r#""a\"b\\c\n\u0041\u{42}\x43""#),
        "a\"b\\c\nABC"
    );
    assert_eq!(js_unescape("''"), "");
    // Tab, carriage return and NUL escapes, a line continuation, an unknown escape
    // read as its character, and a lone backslash at the end kept as written.
    assert_eq!(js_unescape("'a\\tb\\rc\\0d\\\ne\\qf\\'"), "a\tb\rc\0deqf\\");
}

#[test]
fn labels_name_only_known_programs() {
    let mut known = |w: &str| ["node", "git", "gh", "ssh", "whoami"].contains(&program(w).as_str());
    for cmd in [
        "ARR=(\n  sentinel\n)\nprintf x",
        "$params = @{\n  sentinel = 'v'\n}\nInvoke-RestMethod @params",
        r"'C:\clients\sentinel.pdf' | Remove-Item",
        "\"owner/sentinel\" | Set-Clipboard",
        r#"TOKEN="ab\" ; sentinel rest" cmd"#,
        "'sentinel' | gh auth login --with-token",
    ] {
        assert_eq!(verb_class(cmd, &mut known), "(other)", "{cmd}");
    }
    assert_eq!(
        verb_class("sudo --user sentinel whoami", &mut known),
        "whoami"
    );
    assert_eq!(
        verb_class("ssh ll 'ARR=(\n sentinel\n)'", &mut known),
        "ssh (other)"
    );
    assert_eq!(verb_class("git status", &mut known), "git");
    assert_eq!(hook_label("node 'C:/a b/sentinel.js'", &mut known), "node");
    assert_eq!(
        hook_label("'sentinel' | gh auth login --with-token", &mut known),
        "(other)"
    );
    assert_eq!(
        hook_label("curl -s https://x.test/a/sentinel.sh", &mut known),
        "(other)"
    );
    assert_eq!(
        hook_label("Loading sentinel mode...", &mut known),
        "(other)"
    );
}

#[test]
fn normalise_path_folds_case_and_slashes() {
    assert_eq!(normalise_path(r"'C:\Hub\A.md'"), "c:/hub/a.md");
}
