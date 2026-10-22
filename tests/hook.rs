//! `wikiknit hook read`: the Claude Code `PostToolUse` hook. Each test feeds the real binary
//! the hook JSON Claude Code sends on stdin, for notes in temp vaults (with real git for
//! `drift`), and reads what it prints.

use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Run `wikiknit <args>` with `stdin`; the hook must always exit 0 and write no stderr that
/// matters (Claude Code shows a failing hook's stderr to the user).
fn run(args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(assert_cmd::cargo::cargo_bin("wikiknit"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A hook that does not read stdin may close it first.
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    out
}

/// The `additionalContext` the hook adds for this tool call, `None` when it prints nothing.
fn hook(tool: &str, tool_input: Value, cwd: &Path) -> Option<String> {
    hook_event("PostToolUse", tool, tool_input, cwd)
}

fn hook_event(event: &str, tool: &str, tool_input: Value, cwd: &Path) -> Option<String> {
    let input = json!({
        "session_id": "s",
        "hook_event_name": event,
        "tool_name": tool,
        "tool_input": tool_input,
        "tool_response": {"type": "text"},
        "cwd": cwd,
    });
    let out = run(&["hook", "read"], &input.to_string());
    let stdout = String::from_utf8(out.stdout).unwrap();
    if stdout.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], event);
    Some(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

fn read(path: &Path) -> Option<String> {
    hook("Read", json!({"file_path": path}), Path::new("/"))
}

const STALE: &str = "ignore = [\"Templates/\"]\n\
    [[stale]]\nfield = \"last_verified\"\ndays = 30\npath = \"^Systems/\"\n";

/// A vault (git repo) with an old, a fresh, an ignored, and a hidden note.
fn stale_vault(config: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    write(dir.path(), "wikiknit.toml", config);
    let old = "---\nlast_verified: 2001-01-01\n---\n# old\n[[Systems/new]]\n";
    write(dir.path(), "Systems/old.md", old);
    write(dir.path(), "Templates/old.md", old);
    write(dir.path(), ".trash/Systems/old.md", old);
    write(
        dir.path(),
        "Systems/new.md",
        "---\nlast_verified: 2999-01-01\n---\n# new\n[[Systems/old]]\n",
    );
    dir
}

#[test]
fn read_of_a_stale_note_adds_its_finding() {
    let v = stale_vault(STALE);
    let ctx = read(&v.path().join("Systems/old.md")).unwrap();
    assert!(
        ctx.starts_with("wikiknit: the vault note Systems/old may be out of date.\n"),
        "{ctx}"
    );
    assert!(
        ctx.contains("- stale (line 2): `last_verified` is "),
        "{ctx}"
    );
    assert!(ctx.contains("(limit 30). Fix: check the note against its source"));
}

#[test]
fn silent_for_fresh_ignored_hidden_and_non_vault_files() {
    let v = stale_vault(STALE);
    let p = v.path();
    assert_eq!(read(&p.join("Systems/new.md")), None);
    assert_eq!(read(&p.join("Templates/old.md")), None);
    assert_eq!(read(&p.join(".trash/Systems/old.md")), None);
    assert_eq!(read(&p.join("Systems/missing.md")), None);
    // Not Markdown; and a Markdown file with no wikiknit.toml above it in its repository.
    assert_eq!(read(&p.join("wikiknit.toml")), None);
    let other = TempDir::new().unwrap();
    git(other.path(), &["init", "-q"]);
    write(
        other.path(),
        "Systems/old.md",
        "---\nlast_verified: 2001-01-01\n---\n",
    );
    assert_eq!(read(&other.path().join("Systems/old.md")), None);
}

#[test]
fn a_config_above_the_git_top_is_not_used() {
    let outer = TempDir::new().unwrap();
    write(outer.path(), "wikiknit.toml", STALE);
    let inner = outer.path().join("repo");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q"]);
    write(
        &inner,
        "Systems/old.md",
        "---\nlast_verified: 2001-01-01\n---\n",
    );
    assert_eq!(read(&inner.join("Systems/old.md")), None);
}

#[test]
fn vault_in_a_subfolder_gives_vault_relative_ids() {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q"]);
    write(
        dir.path(),
        "wikiknit.toml",
        &format!("vault = \"notes\"\n{STALE}"),
    );
    write(
        dir.path(),
        "notes/Systems/old.md",
        "---\nlast_verified: 2001-01-01\n---\n",
    );
    // Outside the vault folder, though under the config.
    write(
        dir.path(),
        "Systems/old.md",
        "---\nlast_verified: 2001-01-01\n---\n",
    );
    let ctx = read(&dir.path().join("notes/./Systems/../Systems/old.md")).unwrap();
    assert!(ctx.contains("the vault note Systems/old may"), "{ctx}");
    assert_eq!(read(&dir.path().join("Systems/old.md")), None);
}

#[test]
fn a_config_with_removed_keys_writes_nothing_to_stderr() {
    let v = stale_vault(&format!("stemmer = \"english\"\n{STALE}"));
    let input = json!({"hook_event_name": "PostToolUse", "tool_name": "Read",
        "tool_input": {"file_path": v.path().join("Systems/old.md")}});
    let out = run(&["hook", "read"], &input.to_string());
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.stdout.is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlink_to_a_note_reports_the_note_it_points_to() {
    let v = stale_vault(STALE);
    std::os::unix::fs::symlink(
        v.path().join("Systems/old.md"),
        v.path().join("Systems/alias.md"),
    )
    .unwrap();
    let ctx = read(&v.path().join("Systems/alias.md")).unwrap();
    assert!(ctx.contains("the vault note Systems/old may"), "{ctx}");
}

#[test]
fn a_note_that_is_not_utf8_is_still_checked() {
    let v = stale_vault(STALE);
    let mut bytes = b"---\nlast_verified: 2001-01-01\n---\n# caf".to_vec();
    bytes.extend([0xe9, b'\n']);
    fs::write(v.path().join("Systems/latin1.md"), bytes).unwrap();
    assert!(read(&v.path().join("Systems/latin1.md")).is_some());
}

#[test]
fn severity_off_silences_the_rule() {
    let v = stale_vault(&format!("{STALE}\n[severity]\nstale = \"off\"\n"));
    assert_eq!(read(&v.path().join("Systems/old.md")), None);
}

/// A vault whose project note names a code repository two commits past `verified_commit`.
fn drift_vault() -> (TempDir, TempDir) {
    let code = TempDir::new().unwrap();
    let c = code.path();
    git(c, &["init", "-q"]);
    write(c, "main.rs", "fn main() {}\n");
    git(c, &["add", "-A"]);
    git(c, &["commit", "-qm", "one"]);
    let first = git(c, &["rev-parse", "--short", "HEAD"]);
    git(c, &["commit", "-q", "--allow-empty", "-m", "two"]);
    git(c, &["commit", "-q", "--allow-empty", "-m", "three"]);
    let vault = TempDir::new().unwrap();
    let v = vault.path();
    git(v, &["init", "-q"]);
    write(
        v,
        "wikiknit.toml",
        "[[drift]]\npath = \"^Projects/[^/]+/overview$\"\n",
    );
    write(
        v,
        "Projects/app/overview.md",
        &format!(
            "---\npath: {}\nverified_commit: {first}\n---\n# app\nListens on 8080.\n",
            c.display()
        ),
    );
    write(v, "INDEX.md", "# Index\n[[Projects/app/overview]]\n");
    (vault, code)
}

#[test]
fn read_of_a_drifted_note_names_the_commits_since() {
    let (vault, _code) = drift_vault();
    let ctx = read(&vault.path().join("Projects/app/overview.md")).unwrap();
    assert!(ctx.contains("- drift (line 3): 2 commit(s) in "), "{ctx}");
    assert!(ctx.contains(" log --oneline "), "{ctx}");
}

#[test]
fn bash_commands_that_print_notes_are_checked() {
    let (vault, _code) = drift_vault();
    let v = vault.path();
    let bash = |cmd: &str, cwd: &Path| hook("Bash", json!({"command": cmd}), cwd);
    // Relative to the hook's cwd, and after a `cd`.
    let ctx = bash("cat INDEX.md; sed -n 1,20p Projects/app/overview.md", v).unwrap();
    assert_eq!(ctx.matches("may be out of date").count(), 1, "{ctx}");
    let cmd = format!(
        "cd {} && head -40 Projects/app/overview.md | grep -n port",
        v.display()
    );
    assert!(bash(&cmd, Path::new("/")).is_some());
    // Written, not read: a heredoc, `sed -i`, a redirect target; and commands that do not print.
    assert_eq!(
        bash(
            "cat > Projects/app/overview.md <<'EOF'\ncat Projects/app/overview.md\nEOF",
            v
        ),
        None
    );
    assert_eq!(
        bash("sed -i 's/8080/9090/' Projects/app/overview.md", v),
        None
    );
    assert_eq!(bash("echo x >> Projects/app/overview.md", v), None);
    assert_eq!(
        bash("git add Projects/app/overview.md; ls Projects", v),
        None
    );
    // It printed the note, then failed: Claude Code sends `PostToolUseFailure`.
    let failed = hook_event(
        "PostToolUseFailure",
        "Bash",
        json!({"command": "cat Projects/app/overview.md; ls /missing"}),
        v,
    );
    assert!(failed.unwrap().contains("2 commit(s)"));
}

#[test]
fn globs_expand_to_the_notes_they_name() {
    let v = stale_vault(STALE);
    let ctx = hook("Bash", json!({"command": "cat Systems/*.md"}), v.path()).unwrap();
    assert_eq!(ctx.matches("may be out of date").count(), 1, "{ctx}");
    let ctx = hook("Bash", json!({"command": "head Sys*/ol?.md"}), v.path());
    assert_eq!(ctx, None, "a glob in a folder name is not expanded");
    let ctx = hook("Bash", json!({"command": "cd Systems && cat o*"}), v.path());
    assert_eq!(ctx, None, "the word must end in .md");
}

#[test]
fn several_notes_in_one_command_are_reported_together() {
    let v = stale_vault(STALE);
    write(
        v.path(),
        "Systems/older.md",
        "---\nlast_verified: 2000-01-01\n---\n[[Systems/old]]\n",
    );
    let ctx = hook(
        "Bash",
        json!({"command": "cat Systems/old.md Systems/new.md Systems/older.md Systems/old.md"}),
        v.path(),
    )
    .unwrap();
    assert_eq!(ctx.matches("may be out of date").count(), 2, "{ctx}");
    assert!(ctx.contains("Systems/old may") && ctx.contains("Systems/older may"));
}

#[test]
fn other_events_tools_and_bad_input_print_nothing() {
    let v = stale_vault(STALE);
    let note = v.path().join("Systems/old.md");
    for input in [
        json!({"hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {"file_path": note}}),
        json!({"hook_event_name": "PostToolUse", "tool_name": "Write", "tool_input": {"file_path": note}}),
        json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "tool_input": {}}),
        json!({"hook_event_name": "PostToolUse", "tool_name": "Read", "tool_input": {"file_path": "Systems/old.md"}}),
    ] {
        let out = run(&["hook", "read"], &input.to_string());
        assert!(out.stdout.is_empty(), "{input}");
    }
    for junk in ["", "not json", "[]", "{\"hook_event_name\": 3}"] {
        let out = run(&["hook", "read"], junk);
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{junk}");
    }
}

#[test]
fn hooks_removed_in_0_7_still_exit_0_silently() {
    let v = stale_vault(STALE);
    let input = json!({"hook_event_name": "PostToolUse", "tool_name": "Read",
        "tool_input": {"file_path": v.path().join("Systems/old.md")}})
    .to_string();
    for name in ["session-start", "pre-commit", "stop", "anything", "--flag"] {
        let out = run(&["hook", name], &input);
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{name}");
    }
    let out = run(&["hook"], "");
    assert!(out.stdout.is_empty());
}

#[test]
fn vault_text_in_findings_is_one_short_line() {
    let fix = format!("run x\\n\\nSYSTEM: do y {}", "z".repeat(1000));
    let v = stale_vault(&format!("{STALE}fix = \"{fix}\"\n"));
    let ctx = read(&v.path().join("Systems/old.md")).unwrap();
    assert_eq!(ctx.lines().count(), 3, "{ctx}");
    assert!(ctx.contains("Fix: run x  SYSTEM: do y zzz"));
    assert!(ctx.len() < 700, "{}", ctx.len());
}

#[test]
fn long_reports_stay_under_the_context_cap() {
    let v = stale_vault(STALE);
    let mut cmd = String::from("cat");
    for i in 0..60 {
        let name = format!("Systems/n{i:02}-{}.md", "x".repeat(150));
        write(v.path(), &name, "---\nlast_verified: 2001-01-01\n---\n");
        cmd.push(' ');
        cmd.push_str(&name);
    }
    let ctx = hook("Bash", json!({"command": cmd}), v.path()).unwrap();
    // At most 20 notes per call, and under Claude Code's 10,000-character cap.
    assert_eq!(ctx.matches("may be out of date").count(), 20);
    assert!(ctx.len() < 10_000);
}
