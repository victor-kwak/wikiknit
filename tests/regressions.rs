//! End-to-end regressions for bugs found in the pre-release review. Each test builds a small
//! vault in a temp dir and runs the real binary.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A temp vault with `wikiknit.toml` and the given files.
fn vault(config: &str, files: &[(&str, &[u8])]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("wikiknit.toml"), config).unwrap();
    for (path, body) in files {
        let p = dir.path().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    dir
}

fn wk(dir: &Path, state: &Path) -> Command {
    let mut cmd = Command::cargo_bin("wikiknit").unwrap();
    cmd.current_dir(dir)
        .env("XDG_STATE_HOME", state)
        .env_remove("WIKIKNIT_SESSION")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_ENV_FILE");
    cmd
}

fn json_of(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn rules(findings: &Value) -> Vec<(String, String)> {
    findings
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["note"].as_str().unwrap().to_string(),
                f["rule"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn mention_pairs(found: &Value) -> Vec<(String, String)> {
    found
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["term"].as_str().unwrap().to_string(),
                m["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn hook(dir: &Path, state: &Path, event: &str, input: &Value) -> std::process::Output {
    wk(dir, state)
        .args(["hook", event])
        .write_stdin(input.to_string())
        .output()
        .unwrap()
}

#[test]
fn non_utf8_note_is_a_finding_not_a_failure() {
    let v = vault(
        "",
        &[("a.md", b"# a\n[[b]]\n"), ("b.md", b"# caf\xe9\n[[a]]\n")],
    );
    let state = TempDir::new().unwrap();
    let out = wk(v.path(), state.path())
        .args(["--format", "json", "check"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let f: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rules(&f), [("b".to_string(), "file-not-utf8".to_string())]);
}

#[test]
fn links_into_ignored_folders_resolve() {
    let v = vault(
        "ignore = [\"Archive/\"]",
        &[
            ("a.md", b"# a\n[[old]] [[Archive/old]]\n"),
            ("Archive/old.md", b"# old\n"),
        ],
    );
    let state = TempDir::new().unwrap();
    let f = json_of(wk(v.path(), state.path()).args(["--format", "json", "check"]));
    // a links only to an ignored note: no unresolved links, but no note link either.
    assert_eq!(
        rules(&f),
        [("a".to_string(), "no-outbound-links".to_string())]
    );
    let msg = wk(v.path(), state.path())
        .args(["check", "Archive/old.md"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&msg.stderr).contains("ignores"));
}

#[cfg(unix)]
#[test]
fn symlinked_notes_are_notes() {
    let v = vault(
        "",
        &[
            ("a.md", b"# a\n[[linked]]\n"),
            ("real/r.md", b"# r\n[[a]]\n"),
        ],
    );
    std::os::unix::fs::symlink(v.path().join("real/r.md"), v.path().join("linked.md")).unwrap();
    let state = TempDir::new().unwrap();
    let f = json_of(wk(v.path(), state.path()).args(["--format", "json", "check"]));
    assert!(rules(&f).iter().all(|(_, r)| r != "unresolved-link"), "{f}");
}

#[test]
fn bom_html_and_schemes() {
    let v = vault(
        "",
        &[
            (
                "bom.md",
                "\u{feff}---\naliases: [Bommy]\n---\n# bom\n[[a]]\n".as_bytes(),
            ),
            (
                "a.md",
                b"# a\n[[bom]] Bommy here. [call](tel:123)\n\n<div>\n[[gone]] Bommy\n</div>\n",
            ),
        ],
    );
    let state = TempDir::new().unwrap();
    let f = json_of(wk(v.path(), state.path()).args(["--format", "json", "check"]));
    assert_eq!(rules(&f), [("a".to_string(), "link-in-html".to_string())]);
    let cat = json_of(wk(v.path(), state.path()).args(["--format", "json", "catalog"]));
    let bom = cat
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "bom")
        .unwrap();
    assert_eq!(bom["aliases"], json!(["Bommy"]));
}

#[test]
fn mention_false_positives_are_gone() {
    let v = vault(
        "",
        &[
            ("index.md", b"# Index\n[[python]]\n"),
            ("python.md", b"# python\n[[index]]\n"),
            (
                "a.md",
                b"# a\nThe search index is fast. [learn python](https://python.org) \
                  <span title=\"python\">x</span>\n",
            ),
        ],
    );
    let state = TempDir::new().unwrap();
    let found = json_of(wk(v.path(), state.path()).args(["--format", "json", "mentions", "a"]));
    assert_eq!(found, json!([]));
}

#[test]
fn unicode_mentions() {
    let v = vault(
        "",
        &[
            (
                "people/émile.md",
                "---\naliases: [\"Émile Zola\"]\n---\n# Émile\n[[a]]\n".as_bytes(),
            ),
            (
                "places/tower.md",
                "---\naliases: [\"東京タワー\"]\n---\n# tower\n[[a]]\n".as_bytes(),
            ),
            ("s1.md", b"---\naliases: [Shared]\n---\n# s1\n[[a]]\n"),
            ("s2.md", b"---\naliases: [Shared]\n---\n# s2\n[[a]]\n"),
            (
                "a.md",
                "# a\nI read émile zola. 私は東京タワーに行った。Shared thing.\n[[s1]]\n"
                    .as_bytes(),
            ),
        ],
    );
    let state = TempDir::new().unwrap();
    let pairs = mention_pairs(&json_of(
        wk(v.path(), state.path()).args(["--format", "json", "mentions", "a"]),
    ));
    assert!(
        pairs.contains(&("émile zola".into(), "people/émile".into())),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("東京タワー".into(), "places/tower".into())),
        "{pairs:?}"
    );
    // s1 is linked already; the other owner of the shared alias is still suggested.
    assert!(pairs.contains(&("Shared".into(), "s2".into())), "{pairs:?}");
    assert!(!pairs.iter().any(|(_, t)| t == "s1"), "{pairs:?}");
}

#[test]
fn vault_flag_uses_the_config_above() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("wikiknit.toml"),
        "vault = \"notes\"\nignore = [\"Templates/\"]\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("notes/Templates")).unwrap();
    fs::write(root.path().join("notes/a.md"), "# a\n[[b]]\n").unwrap();
    fs::write(root.path().join("notes/b.md"), "# b\n[[a]]\n").unwrap();
    fs::write(root.path().join("notes/Templates/t.md"), "[[broken]]\n").unwrap();
    let state = TempDir::new().unwrap();
    wk(root.path(), state.path())
        .args(["--vault", "notes", "check"])
        .assert()
        .success();
}

#[test]
fn file_argument_beats_a_root_note_of_the_same_name() {
    let v = vault(
        "",
        &[
            ("dup.md", b"# dup\n[[sub/dup]]\n"),
            ("sub/dup.md", b"# dup2\nno links\n"),
        ],
    );
    let state = TempDir::new().unwrap();
    wk(&v.path().join("sub"), state.path())
        .args(["--vault", "..", "check", "dup.md"])
        .assert()
        .code(1);
}

#[test]
fn vault_outside_the_config_folder_is_rejected() {
    let v = vault("vault = \"/\"", &[]);
    let state = TempDir::new().unwrap();
    let out = wk(v.path(), state.path()).arg("check").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside the config"));
}

/// A git-tracked vault with two notes, for hook tests.
fn git_vault() -> TempDir {
    let v = vault("", &[("a.md", b"# a\n[[b]]\n"), ("b.md", b"# b\n[[a]]\n")]);
    git(v.path(), &["init", "-q"]);
    v
}

fn commit_input(v: &Path, session: &str, command: &str) -> Value {
    json!({"session_id": session, "cwd": v, "tool_input": {"command": command}})
}

#[test]
fn commits_behind_keywords_wrappers_and_aliases_are_held() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "h", "cwd": p}),
    );
    fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
    git(p, &["config", "alias.ci", "commit -v"]);
    git(
        p,
        &["config", "alias.save", "!git add -A && git commit -m wip"],
    );
    git(p, &["config", "alias.wip", "!git ci -m wip"]);
    for command in [
        "if true; then git commit -m x; fi",
        "timeout 30 git commit -m x",
        "bash -c \"git commit -m x\"",
        "echo \"x << y\"\ngit commit -m z",
        "git ci -m x",
        "git save",
        "git wip",
        "bash -euo pipefail -c 'git commit -m x'",
        "out=\"$(git commit -m x)\"",
        "(cd /tmp && ls); git commit -m x",
    ] {
        let out = hook(
            p,
            state.path(),
            "pre-commit",
            &commit_input(p, "h", command),
        );
        assert_eq!(out.status.code(), Some(2), "not held: {command}");
    }
    let out = hook(
        p,
        state.path(),
        "pre-commit",
        &commit_input(p, "h", "git status"),
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_link_pass_counts_for_every_session() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    for s in ["one", "two"] {
        hook(
            p,
            state.path(),
            "session-start",
            &json!({"session_id": s, "cwd": p}),
        );
    }
    fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
    let commit = |s: &str| {
        hook(
            p,
            state.path(),
            "pre-commit",
            &commit_input(p, s, "git commit -m x"),
        )
    };
    assert_eq!(commit("one").status.code(), Some(2));
    assert_eq!(commit("two").status.code(), Some(2));
    wk(p, state.path())
        .args(["mark", "--session", "one"])
        .assert()
        .success();
    assert_eq!(commit("one").status.code(), Some(0));
    assert_eq!(
        commit("two").status.code(),
        Some(0),
        "session two still held after session one's pass"
    );
}

#[test]
fn pending_notes_survive_clear() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "before", "cwd": p}),
    );
    fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
    // /clear: a new session id, same terminal.
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "after", "cwd": p, "source": "clear"}),
    );
    let out = hook(
        p,
        state.path(),
        "pre-commit",
        &commit_input(p, "after", "git commit -m x"),
    );
    assert_eq!(out.status.code(), Some(2));
    // A plain new session (startup) does not inherit it.
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "fresh", "cwd": p, "source": "startup"}),
    );
    let out = hook(
        p,
        state.path(),
        "pre-commit",
        &commit_input(p, "fresh", "git commit -m x"),
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn corrupt_state_fails_closed() {
    for corrupt in ["linked.json", "sessions/c.json"] {
        let v = git_vault();
        let state = TempDir::new().unwrap();
        let p = v.path();
        hook(
            p,
            state.path(),
            "session-start",
            &json!({"session_id": "c", "cwd": p}),
        );
        fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
        let root = state.path().join("wikiknit");
        let vault_dir = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
        fs::write(vault_dir.join(corrupt), "{not json").unwrap();
        let out = hook(
            p,
            state.path(),
            "pre-commit",
            &commit_input(p, "c", "git commit -m x"),
        );
        assert_eq!(
            out.status.code(),
            Some(2),
            "{corrupt}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn a_session_without_a_record_still_holds_pending_notes() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "known", "cwd": p}),
    );
    fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
    // A session the hooks never saw start (plugin installed mid-session, state lost).
    let out = hook(
        p,
        state.path(),
        "pre-commit",
        &commit_input(p, "unknown", "git commit -m x"),
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn concurrent_marks_are_all_kept() {
    let names: Vec<String> = (0..20).map(|i| format!("n{i:02}")).collect();
    let files: Vec<(String, Vec<u8>)> = names
        .iter()
        .map(|n| {
            (
                format!("{n}.md"),
                format!("# {n}\n[[n00]] [[n01]]\n").into_bytes(),
            )
        })
        .collect();
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    for _round in 0..5 {
        let v = vault("", &refs);
        let state = TempDir::new().unwrap();
        let p = v.path();
        hook(
            p,
            state.path(),
            "session-start",
            &json!({"session_id": "m", "cwd": p}),
        );
        for n in &names {
            fs::write(
                p.join(format!("{n}.md")),
                format!("# {n}\nedited [[n00]] [[n01]]\n"),
            )
            .unwrap();
        }
        let spawn = |part: &[String]| {
            let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("wikiknit"));
            cmd.current_dir(p)
                .env("XDG_STATE_HOME", state.path())
                .args(["mark", "--session", "m"])
                .args(part)
                .stderr(std::process::Stdio::null());
            cmd.spawn().unwrap()
        };
        let mut a = spawn(&names[..10]);
        let mut b = spawn(&names[10..]);
        assert!(a.wait().unwrap().success() && b.wait().unwrap().success());
        let left = wk(p, state.path())
            .args(["changed", "--session", "m"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&left.stdout).trim(),
            "",
            "lost a mark"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_vault_folder_and_symlinks_inside_it() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    // The vault folder itself is a symlink: allowed.
    fs::create_dir_all(elsewhere.path().join("vault/real")).unwrap();
    fs::write(
        elsewhere.path().join("vault/a.md"),
        "---\naliases: [Ay]\n---\n# a\n[[real/r]]\n",
    )
    .unwrap();
    fs::write(
        elsewhere.path().join("vault/real/r.md"),
        "---\naliases: [Ar]\n---\n# r\n[[a]]\n",
    )
    .unwrap();
    symlink(elsewhere.path().join("vault"), root.path().join("notes")).unwrap();
    fs::write(root.path().join("wikiknit.toml"), "vault = \"notes\"\n").unwrap();
    // A folder symlink inside the vault (same notes again) and one pointing outside it.
    symlink(
        elsewhere.path().join("vault/real"),
        elsewhere.path().join("vault/again"),
    )
    .unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("o.md"), "# outside\n[[nowhere]]\n").unwrap();
    symlink(outside.path(), elsewhere.path().join("vault/out")).unwrap();
    let state = TempDir::new().unwrap();
    let f = json_of(wk(root.path(), state.path()).args(["--format", "json", "check"]));
    assert_eq!(f, json!([]), "duplicates or outside notes leaked in");
    let cat = json_of(wk(root.path(), state.path()).args(["--format", "json", "catalog"]));
    let ids: Vec<&str> = cat
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "real/r"]);
}

#[test]
fn md_suffix_link_to_an_ignored_note_is_not_ambiguous() {
    let v = vault(
        "ignore = [\"Archive/\"]",
        &[
            ("a.md", b"# a\n[[old.md]] [[b]]\n"),
            ("b.md", b"# b\n[[a]]\n"),
            ("Archive/old.md", b"# old\n"),
        ],
    );
    let state = TempDir::new().unwrap();
    let f = json_of(wk(v.path(), state.path()).args(["--format", "json", "check"]));
    assert_eq!(f, json!([]));
}

#[test]
fn guessing_the_session_refuses_when_several_are_active() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "x1", "cwd": p}),
    );
    wk(p, state.path()).arg("changed").assert().success();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "x2", "cwd": p}),
    );
    let out = wk(p, state.path()).arg("changed").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("several sessions"));
}

#[test]
fn inline_env_and_external_aliases_are_held() {
    let v = git_vault();
    let state = TempDir::new().unwrap();
    let p = v.path();
    hook(
        p,
        state.path(),
        "session-start",
        &json!({"session_id": "s", "cwd": p}),
    );
    fs::write(p.join("a.md"), "# a\nedited [[b]]\n").unwrap();
    for command in [
        "git -c alias.x=commit x -m y",
        "git -c 'alias.x=!git commit -m y' x",
        "GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.x GIT_CONFIG_VALUE_0=commit git x",
        "export GIT_CONFIG_COUNT=1; git x",
        "git --config-env=alias.x=CMD x",
        "git -c ALIAS.X=commit x -m y",
        "git -c include.path=/tmp/evil x",
        "HOME=/tmp/other git x",
        "PATH=/tmp/bin:$PATH git x",
    ] {
        let out = hook(
            p,
            state.path(),
            "pre-commit",
            &commit_input(p, "s", command),
        );
        assert_eq!(out.status.code(), Some(2), "not held: {command}");
    }
    // An external `git-<name>` program may commit: held.
    let bin = TempDir::new().unwrap();
    let tool = bin.path().join("git-absorbx");
    fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );
    let out = wk(p, state.path())
        .env("PATH", &path)
        .args(["hook", "pre-commit"])
        .write_stdin(commit_input(p, "s", "git absorbx").to_string())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "external git command not held");
    // Built-in read-only commands and unknown names with no alias or program are not commits.
    for command in [
        "git cat-file -t HEAD",
        "git for-each-ref",
        "git nosuchthing",
    ] {
        let out = hook(
            p,
            state.path(),
            "pre-commit",
            &commit_input(p, "s", command),
        );
        assert_eq!(out.status.code(), Some(0), "held: {command}");
    }
}

#[cfg(unix)]
#[test]
fn vault_symlink_to_a_folder_above_the_config_is_rejected() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink(root.path(), repo.join("notes")).unwrap();
    fs::write(repo.join("wikiknit.toml"), "vault = \"notes\"\n").unwrap();
    let state = TempDir::new().unwrap();
    let out = wk(&repo, state.path()).arg("check").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("folder above the config"));
}
