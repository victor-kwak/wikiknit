//! Checks against git: `--staged` and `--changed` (only what a change adds), a real git
//! pre-commit hook, and the `drift` and `forbid_link` rules. Every test runs the real binary
//! and real git in temp folders.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;
use tempfile::TempDir;

/// git isolated from the user's config and hooks.
fn git_cmd(dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    for v in ["GIT_DIR", "GIT_INDEX_FILE", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
    c
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let out = git_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// A git repo holding `wikiknit.toml` (with `config`) and a vault in `vault_dir`.
fn repo(vault_dir: &str, config: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "core.hooksPath", ".githooks"]);
    write(
        dir.path(),
        "wikiknit.toml",
        &format!("vault = \"{vault_dir}\"\n{config}"),
    );
    fs::create_dir_all(dir.path().join(vault_dir)).unwrap();
    dir
}

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("wikiknit")
}

fn wk(dir: &Path, args: &[&str]) -> Output {
    let mut c = std::process::Command::new(bin());
    c.current_dir(dir);
    for v in ["GIT_DIR", "GIT_INDEX_FILE", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
    c.args(args).output().unwrap()
}

/// `wikiknit --format json <args>`: the items.
fn items(dir: &Path, args: &[&str]) -> Vec<Value> {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let out = wk(dir, &full);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    v["items"].as_array().unwrap().clone()
}

/// (note, rule) of the errors.
fn rules_of(items: &[Value]) -> Vec<(String, String)> {
    items
        .iter()
        .filter(|i| i["severity"] == "error")
        .map(|i| {
            (
                i["note"].as_str().unwrap().to_string(),
                i["rule"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pair(note: &str, rule: &str) -> (String, String) {
    (note.to_string(), rule.to_string())
}

/// A git pre-commit hook that runs `wikiknit check --staged`.
fn install_hook(dir: &Path) {
    let hook = dir.join(".githooks/pre-commit");
    write(
        dir,
        ".githooks/pre-commit",
        &format!("#!/bin/sh\nexec '{}' check --staged\n", bin().display()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn staged_reports_only_what_the_commit_adds() {
    let r = repo("notes", "");
    let d = r.path();
    // Already committed: one broken link. It is not this commit's problem.
    write(d, "notes/a.md", "# a\n[[b]] [[gone]]\n");
    write(d, "notes/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    assert_eq!(items(d, &["check", "--staged"]), Vec::<Value>::new());
    // A new note with a broken link, staged.
    write(d, "notes/c.md", "# c\n[[a]] [[nowhere]]\n");
    git(d, &["add", "notes/c.md"]);
    let found = items(d, &["check", "--staged"]);
    assert_eq!(rules_of(&found), vec![pair("c", "unresolved-link")]);
    let broken = found
        .iter()
        .find(|f| f["rule"] == "unresolved-link")
        .unwrap();
    assert_eq!(broken["line"], 2);
    // Nothing links the new note yet: a warning, which does not block.
    assert!(
        found
            .iter()
            .any(|f| f["rule"] == "orphan" && f["severity"] == "warn")
    );
    let out = wk(d, &["check", "--staged"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("this commit adds 1 link error"));
}

#[test]
fn staged_reads_the_index_not_the_work_tree() {
    let r = repo(".", "");
    let d = r.path();
    write(d, "a.md", "# a\n[[b]]\n");
    write(d, "b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    // Staged: broken. On disk: fixed, but not staged.
    write(d, "a.md", "# a\n[[b]] [[typo]]\n");
    git(d, &["add", "a.md"]);
    write(d, "a.md", "# a\n[[b]]\n");
    assert_eq!(
        rules_of(&items(d, &["check", "--staged"])),
        vec![pair("a", "unresolved-link")]
    );
    assert_eq!(items(d, &["check", "--changed"]), Vec::<Value>::new());
    // The other way round: staged clean, disk broken.
    git(d, &["add", "a.md"]);
    write(d, "a.md", "# a\n[[b]] [[typo]]\n");
    assert_eq!(items(d, &["check", "--staged"]), Vec::<Value>::new());
    assert_eq!(
        rules_of(&items(d, &["check", "--changed"])),
        vec![pair("a", "unresolved-link")]
    );
}

#[test]
fn deleting_a_note_reports_the_links_it_breaks() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, "v/c.md", "# c\n[[a]] [[b]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    git(d, &["rm", "-q", "v/b.md"]);
    let found = rules_of(&items(d, &["check", "--staged"]));
    assert!(found.contains(&pair("a", "unresolved-link")), "{found:?}");
    assert!(found.contains(&pair("c", "unresolved-link")), "{found:?}");
    // The same from the work tree, before staging.
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    fs::remove_file(d.join("v/b.md")).unwrap();
    // `a` linked only `b`: now it links nothing, too.
    assert_eq!(
        rules_of(&items(d, &["check", "--changed"])),
        vec![pair("a", "no-outbound-links"), pair("a", "unresolved-link")]
    );
}

#[test]
fn before_the_first_commit_everything_is_new() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[missing]]\n");
    git(d, &["add", "-A"]);
    let found = rules_of(&items(d, &["check", "--staged"]));
    assert!(found.contains(&pair("a", "unresolved-link")), "{found:?}");
    assert_eq!(wk(d, &["check", "--staged"]).status.code(), Some(1));
}

#[test]
fn untracked_notes_count_as_changed() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/new.md", "# new\n[[a]] [[nope]]\n");
    assert_eq!(
        rules_of(&items(d, &["check", "--changed"])),
        vec![pair("new", "unresolved-link")]
    );
    // Not staged: the commit does not add it.
    assert_eq!(items(d, &["check", "--staged"]), Vec::<Value>::new());
}

#[test]
fn vault_folder_with_glob_characters_and_spaces() {
    let r = repo("my [notes] *", "");
    let d = r.path();
    write(d, "my [notes] */a.md", "# a\n[[b]]\n");
    write(d, "my [notes] */b.md", "# b\n[[a]]\n");
    // A sibling folder the glob would match: not part of the vault.
    write(d, "my nnotes x/z.md", "# z\n[[nothing]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "my [notes] */a.md", "# a\n[[b]] [[x]]\n");
    write(d, "my nnotes x/z.md", "# z\n[[nothing]] [[more]]\n");
    git(d, &["add", "-A"]);
    assert_eq!(
        rules_of(&items(d, &["check", "--staged"])),
        vec![pair("a", "unresolved-link")]
    );
}

#[test]
fn git_hook_blocks_commits_that_add_errors_in_every_commit_form() {
    let r = repo("notes", "");
    let d = r.path();
    install_hook(d);
    write(d, "notes/a.md", "# a\n[[b]]\n");
    write(d, "notes/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    let commit = |args: &[&str]| git_cmd(d).arg("commit").args(args).output().unwrap();

    // `commit -a`: git hands the hook a temporary index (GIT_INDEX_FILE).
    write(d, "notes/a.md", "# a\n[[b]] [[broken]]\n");
    let out = commit(&["-qam", "x"]);
    assert!(!out.status.success(), "commit -a went through");
    let err =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("[[broken]]"), "{err}");

    // `commit <path>`: also a temporary index.
    let out = commit(&["-qm", "x", "notes/a.md"]);
    assert!(!out.status.success(), "commit <path> went through");

    // Staged and committed plainly.
    git(d, &["add", "notes/a.md"]);
    assert!(!commit(&["-qm", "x"]).status.success());

    // Fixed: the commit goes through. A pre-existing problem elsewhere does not block it.
    write(d, "notes/a.md", "# a\n[[b]] fixed\n");
    git(d, &["add", "notes/a.md"]);
    let out = commit(&["-qm", "fix"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A commit from a subfolder, and one that does not touch the vault.
    write(d, "src/x.txt", "x\n");
    git(d, &["add", "src/x.txt"]);
    let out = git_cmd(&d.join("src"))
        .args(["commit", "-qm", "code"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn git_hook_in_a_linked_worktree() {
    let r = repo("notes", "");
    let d = r.path();
    // The hook is committed, so the worktree has it too.
    install_hook(d);
    write(d, "notes/a.md", "# a\n[[b]]\n");
    write(d, "notes/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    let wt = TempDir::new().unwrap();
    let wt = wt.path().join("wt");
    git(
        d,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "side"],
    );
    write(&wt, "notes/a.md", "# a\n[[b]] [[broken]]\n");
    let out = git_cmd(&wt).args(["commit", "-qam", "x"]).output().unwrap();
    assert!(!out.status.success(), "worktree commit went through");
}

/// A repo with three commits; returns it and the first commit's id.
fn project() -> (TempDir, String) {
    let p = TempDir::new().unwrap();
    git(p.path(), &["init", "-q", "-b", "main"]);
    for n in ["one", "two", "three"] {
        write(p.path(), "f.txt", n);
        git(p.path(), &["add", "f.txt"]);
        git(p.path(), &["commit", "-qm", n]);
    }
    let first = git(p.path(), &["rev-list", "--max-parents=0", "HEAD"]);
    let first = String::from_utf8_lossy(&first.stdout).trim()[..7].to_string();
    (p, first)
}

#[test]
fn drift_counts_commits_since_the_verified_commit() {
    let (p, first) = project();
    let pp = p.path().display().to_string();
    let r = repo("v", "[[drift]]\npath = \"^Projects/\"\n");
    let d = r.path();
    let note = |commit: &str, path: &str| {
        format!("---\npath: {path}\nverified_commit: {commit}\n---\n# p\n[[other]]\n")
    };
    write(d, "v/other.md", "# other\n[[Projects/a]]\n");
    write(d, "v/Projects/a.md", &note(&first, &pp));
    write(d, "v/Projects/b.md", &note("deadbee", &pp));
    write(
        d,
        "v/Projects/c.md",
        &note("none (not a git repo)", "/nowhere"),
    );
    write(d, "v/Projects/d.md", &note("abcdef1", "/nowhere/at/all"));
    write(d, "v/Elsewhere.md", &note(&first, &pp));
    let found = items(d, &["check"]);
    let drift: Vec<&Value> = found.iter().filter(|f| f["rule"] == "drift").collect();
    let msg = |id: &str| {
        drift
            .iter()
            .find(|f| f["note"] == id)
            .map(|f| f["message"].as_str().unwrap().to_string())
    };
    assert!(
        msg("Projects/a").unwrap().starts_with("2 commit(s) in "),
        "{drift:?}"
    );
    assert_eq!(
        drift.iter().find(|f| f["note"] == "Projects/a").unwrap()["line"],
        3
    );
    assert!(
        msg("Projects/b").unwrap().contains("is not a commit in"),
        "{drift:?}"
    );
    assert_eq!(msg("Projects/c"), None);
    assert!(
        msg("Projects/d")
            .unwrap()
            .contains("is not a git repository")
    );
    assert_eq!(
        msg("Elsewhere"),
        None,
        "path rule limits drift to Projects/"
    );
    assert!(
        found
            .iter()
            .all(|f| f["rule"] != "drift" || f["severity"] == "warn")
    );
    // Up to date: no finding.
    let head = git(p.path(), &["rev-parse", "--short", "HEAD"]);
    write(
        d,
        "v/Projects/a.md",
        &note(String::from_utf8_lossy(&head.stdout).trim(), &pp),
    );
    assert_eq!(
        items(d, &["check", "Projects/a"])
            .iter()
            .filter(|f| f["rule"] == "drift")
            .count(),
        0
    );
}

#[test]
fn drift_ignores_the_vault_repo_env_of_a_git_hook() {
    let (p, first) = project();
    let r = repo("v", "[[drift]]\n");
    let d = r.path();
    write(
        d,
        "v/a.md",
        &format!(
            "---\npath: {}\nverified_commit: {first}\n---\n# a\n[[b]]\n",
            p.path().display()
        ),
    );
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    // As a hook runs it: GIT_DIR names the vault's repo, relative to the hook's folder.
    let out = std::process::Command::new(bin())
        .current_dir(d)
        .env("GIT_DIR", ".git")
        .args(["--format", "json", "check"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let drift: Vec<&Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == "drift")
        .collect();
    assert_eq!(drift.len(), 1, "{v}");
    assert!(
        drift[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("2 commit(s)")
    );
}

#[test]
fn drift_never_passes_an_option_to_git() {
    let (p, _) = project();
    let r = repo("v", "[[drift]]\n");
    let d = r.path();
    write(
        d,
        "v/a.md",
        &format!(
            "---\npath: {}\nverified_commit: \"--output=/tmp/x\"\n---\n# a\n[[b]]\n",
            p.path().display()
        ),
    );
    write(d, "v/b.md", "# b\n[[a]]\n");
    assert!(items(d, &["check"]).iter().all(|f| f["rule"] != "drift"));
}

#[test]
fn forbid_link_catches_a_resolved_issue_listed_as_open() {
    let config = "[[forbid_link]]\ntarget = \"^Issues/resolved/\"\nnear = \"(?i)\\\\bopen\\\\b\"\nmessage = \"a resolved issue is listed as open\"\n";
    let r = repo("v", config);
    let d = r.path();
    write(d, "v/Issues/resolved/r.md", "# r\n[[INDEX]]\n");
    write(d, "v/Issues/open/o.md", "# o\n[[INDEX]]\n");
    write(
        d,
        "v/INDEX.md",
        "# Index\n\n## Open issues\n\n- [[Issues/resolved/r]]\n- [[Issues/open/o]]\n\n## Resolved\n\n- [[Issues/resolved/r]]\n",
    );
    write(
        d,
        "v/P.md",
        "# P\n- Known open issues: [[Issues/resolved/r]], [[Issues/open/o]]\n- Fixed: [[Issues/resolved/r]] (was [[Issues/open/o]])\n",
    );
    let found: Vec<(String, u64)> = items(d, &["check"])
        .iter()
        .filter(|f| f["rule"] == "forbidden-link")
        .map(|f| {
            assert_eq!(f["severity"], "error");
            assert_eq!(f["message"], "a resolved issue is listed as open");
            (
                f["note"].as_str().unwrap().to_string(),
                f["line"].as_u64().unwrap(),
            )
        })
        .collect();
    // INDEX line 5 (under "Open issues"), P line 2 ("open" on the line). Not INDEX line 10
    // (under "Resolved"), not P line 3 (only the link target says open).
    assert_eq!(found, vec![("INDEX".to_string(), 5), ("P".to_string(), 2)]);
}

#[test]
fn removed_config_keys_warn_but_load() {
    let r = repo(
        ".",
        "stemmer = \"english\"\nno_backlink_from = [\"Sessions/\"]\n",
    );
    let d = r.path();
    write(d, "a.md", "# a\n[[b]]\n");
    write(d, "b.md", "# b\n[[a]]\n");
    let out = wk(d, &["check"]);
    assert_eq!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("`stemmer` is ignored"), "{err}");
    assert!(err.contains("`no_backlink_from` is ignored"), "{err}");
    // A key that never existed is still an error.
    write(d, "wikiknit.toml", "vault = \".\"\nstemer = \"english\"\n");
    let out = wk(d, &["check"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown field `stemer`"));
}

#[test]
fn changed_outside_git_says_so() {
    let d = TempDir::new().unwrap();
    write(d.path(), "wikiknit.toml", "vault = \".\"\n");
    write(d.path(), "a.md", "# a\n");
    let out = std::process::Command::new(bin())
        .current_dir(d.path())
        .env("GIT_CEILING_DIRECTORIES", d.path().parent().unwrap())
        .args(["check", "--changed"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not in a git work tree"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn mentions_changed_lists_only_new_mentions() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/Gatehouse.md", "# Gatehouse\n[[a]]\n");
    write(d, "v/a.md", "# a\nGatehouse is old news.\n[[Gatehouse]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/b.md", "# b\n[[a]] and Gatehouse again.\n");
    let found = items(d, &["mentions", "--changed"]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["note"], "b");
}

#[test]
fn old_plugin_hooks_do_not_block() {
    // A session started with the 0.6 plugin still runs `wikiknit hook <event>` with JSON on
    // stdin; exit 2 would block every Bash command there.
    for event in ["session-start", "pre-commit", "stop"] {
        let out = std::process::Command::new(bin())
            .args(["hook", event])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{event}");
    }
}

// Review findings (2026-10-18): each test reproduces one, through real git.

fn hook_script(dir: &Path, name: &str, body: &str) {
    write(
        dir,
        &format!(".githooks/{name}"),
        &format!("#!/bin/sh\n{body}\n"),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            dir.join(".githooks").join(name),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}

fn commit_ok(dir: &Path, args: &[&str]) -> bool {
    git_cmd(dir)
        .arg("commit")
        .args(args)
        .output()
        .unwrap()
        .status
        .success()
}

/// Review 1: a hook that `cd`s into the vault before running wikiknit. Git's relative
/// `GIT_INDEX_FILE` is relative to the top, not to the hook's folder.
#[test]
fn hook_that_changes_folder_still_reads_the_index() {
    let r = repo("notes", "");
    let d = r.path();
    // The config sits in the vault folder here, so the hook runs from there.
    fs::rename(d.join("wikiknit.toml"), d.join("notes/wikiknit.toml")).unwrap();
    write(d, "notes/wikiknit.toml", "vault = \".\"\n");
    hook_script(
        d,
        "pre-commit",
        &format!(
            "cd notes || exit 1\nexec '{}' check --staged",
            bin().display()
        ),
    );
    write(d, "notes/a.md", "# a\n[[b]]\n");
    write(d, "notes/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    assert!(commit_ok(d, &["-qm", "base"]));
    write(d, "notes/a.md", "# a\n[[b]] [[broken]]\n");
    git(d, &["add", "notes/a.md"]);
    assert!(!commit_ok(d, &["-qm", "x"]), "plain commit went through");
    assert!(!commit_ok(d, &["-qam", "x"]), "commit -a went through");
}

/// Review 2: a linked worktree, vault in a subfolder, `full-path` links. Git sets `GIT_DIR`
/// without `GIT_WORK_TREE` there; the vault must still be the subfolder.
#[test]
fn linked_worktree_with_the_vault_in_a_subfolder() {
    let r = repo("v", "link_style = \"full-path\"\n");
    let d = r.path();
    install_hook(d);
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, "README.md", "# readme\n[[not-a-note]]\n");
    git(d, &["add", "-A"]);
    assert!(commit_ok(d, &["-qm", "base"]));
    let wt_dir = TempDir::new().unwrap();
    let wt = wt_dir.path().join("wt");
    git(
        d,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "side"],
    );
    // A valid full-path link: must pass.
    write(&wt, "v/c.md", "# c\n[[a]]\n");
    git(&wt, &["add", "v/c.md"]);
    let out = git_cmd(&wt).args(["commit", "-qm", "c"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    // A broken one: must be blocked.
    write(&wt, "v/c.md", "# c\n[[a]] [[gone]]\n");
    assert!(!commit_ok(&wt, &["-qam", "x"]));
    // `GIT_DIR=.git git commit` at the top of the main work tree: the same.
    write(d, "v/d.md", "# d\n[[a]]\n");
    git(d, &["add", "v/d.md"]);
    let out = git_cmd(d)
        .env("GIT_DIR", ".git")
        .args(["commit", "-qm", "d"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Review 3: `git add -N` puts an empty entry in the index that is not committed.
#[test]
fn intent_to_add_notes_are_not_part_of_the_commit() {
    let r = repo("v", "[severity]\nno-outbound-links = \"off\"\n");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/new.md", "# new\n[[a]]\n");
    git(d, &["add", "-N", "v/new.md"]);
    write(d, "v/b.md", "# b\n[[a]] [[new]]\n");
    git(d, &["add", "v/b.md"]);
    assert_eq!(
        rules_of(&items(d, &["check", "--staged"])),
        vec![pair("b", "unresolved-link")]
    );
}

/// Review 4: a symlinked folder in the vault resolves in the copies too.
#[cfg(unix)]
#[test]
fn symlinked_folders_resolve_in_staged_copies() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/real/n.md", "# n\n[[a]]\n");
    write(d, "v/a.md", "# a\n[[real/n]] [[alias/n]]\n");
    std::os::unix::fs::symlink("real", d.join("v/alias")).unwrap();
    git(d, &["add", "-A"]);
    // The copies agree with the vault on disk: the link through the symlink resolves.
    let broken = |args: &[&str]| {
        items(d, args)
            .into_iter()
            .filter(|f| f["rule"] == "unresolved-link")
            .count()
    };
    assert_eq!(broken(&["check"]), 0);
    assert_eq!(broken(&["check", "--staged"]), 0);
    assert_eq!(broken(&["check", "--changed"]), 0);
}

/// Review 5: findings are compared by what they are about, not by their wording.
#[test]
fn old_findings_reworded_by_the_change_are_not_new() {
    let r = repo("v", "");
    let d = r.path();
    let note = |name: &str, aliases: &str, extra: &str| {
        format!("---\naliases: [{aliases}]\n{extra}---\n# {name}\n[[x]]\n")
    };
    write(d, "v/x.md", "# x\n[[a]] [[b]] [[c]] [[r]]\n");
    // a, b, c share an alias; r has a broken link; f has broken YAML on its last line.
    write(d, "v/a.md", &note("a", "shared", ""));
    write(d, "v/b.md", &note("b", "shared", ""));
    write(d, "v/c.md", &note("c", "shared", ""));
    write(d, "v/r.md", "# r\n[[x]] [[gone]]\n");
    write(
        d,
        "v/f.md",
        "---\ntitle: f\nbad: [unclosed\n---\n# f\n[[x]]\n",
    );
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    // Remove the alias from b only: a and c still share it, only the wording changes.
    write(d, "v/b.md", &note("b", "", ""));
    // A line above the broken YAML: its line number changes.
    write(
        d,
        "v/f.md",
        "---\nnew: 1\ntitle: f\nbad: [unclosed\n---\n# f\n[[x]]\n",
    );
    // Rename a note that already had a broken link (and keep links to it working).
    git(d, &["add", "-A"]);
    git(d, &["mv", "v/r.md", "v/r2.md"]);
    write(d, "v/x.md", "# x\n[[a]] [[b]] [[c]] [[r2]]\n");
    git(d, &["add", "-A"]);
    assert_eq!(items(d, &["check", "--staged"]), Vec::<Value>::new());
}

/// Review 5: with a custom message, two forbidden links in one note differ by target.
#[test]
fn a_new_forbidden_link_is_not_hidden_by_a_fixed_one() {
    let config = "[[forbid_link]]\ntarget = \"^Issues/resolved/\"\nnear = \"(?i)open\"\nmessage = \"listed as open\"\n";
    let r = repo("v", config);
    let d = r.path();
    write(d, "v/Issues/resolved/x.md", "# x\n[[INDEX]]\n");
    write(d, "v/Issues/resolved/y.md", "# y\n[[INDEX]]\n");
    write(
        d,
        "v/INDEX.md",
        "# Index\n## Open\n- [[Issues/resolved/x]]\n## Done\n- [[Issues/resolved/y]]\n",
    );
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(
        d,
        "v/INDEX.md",
        "# Index\n## Open\n- [[Issues/resolved/y]]\n## Done\n- [[Issues/resolved/x]]\n",
    );
    git(d, &["add", "-A"]);
    assert_eq!(
        rules_of(&items(d, &["check", "--staged"])),
        vec![pair("INDEX", "forbidden-link")]
    );
}

/// Review 6: the config is looked up only inside the repository.
#[test]
fn a_config_above_the_repository_is_not_used() {
    let outer = TempDir::new().unwrap();
    write(outer.path(), "wikiknit.toml", "vault = \"inner\"\n");
    let inner = outer.path().join("inner");
    fs::create_dir_all(&inner).unwrap();
    git(&inner, &["init", "-q", "-b", "main"]);
    write(&inner, "a.md", "# a\n");
    let out = wk(&inner, &["check"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no wikiknit.toml"));
}

/// Review 7: a merge commit runs `pre-merge-commit`, not `pre-commit`; the same check there
/// blocks a merge whose result has a broken link though each side was clean.
#[test]
fn pre_merge_commit_hook_checks_the_merge_result() {
    let r = repo("v", "");
    let d = r.path();
    install_hook(d);
    let cmd = format!("exec '{}' check --staged", bin().display());
    hook_script(d, "pre-merge-commit", &cmd);
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, "v/x.md", "# x\n[[a]]\n");
    git(d, &["add", "-A"]);
    assert!(commit_ok(d, &["-qm", "base"]));
    git(d, &["switch", "-q", "-c", "side"]);
    write(d, "v/a.md", "# a\n[[b]] [[x]]\n");
    git(d, &["add", "-A"]);
    assert!(commit_ok(d, &["-qm", "link x"]));
    git(d, &["switch", "-q", "main"]);
    git(d, &["rm", "-q", "v/x.md"]);
    assert!(commit_ok(d, &["-qm", "drop x"]));
    let out = git_cmd(d)
        .args(["merge", "--no-edit", "-q", "side"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "the merge went through");
}

/// Review 9: `--staged` applies the staged config, not an unstaged edit of it.
#[test]
fn staged_uses_the_staged_config() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/n/a.md", "# a\n[[b]]\n");
    write(d, "v/n/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/n/c.md", "# c\n[[a]]\n");
    git(d, &["add", "v/n/c.md"]);
    // Not staged: a stricter link style.
    write(
        d,
        "wikiknit.toml",
        "vault = \"v\"\nlink_style = \"full-path\"\n",
    );
    assert_eq!(rules_of(&items(d, &["check", "--staged"])), vec![]);
    // The unstaged config does apply to the work tree.
    assert!(rules_of(&items(d, &["check", "--changed"])).contains(&pair("n/c", "link-style")));
}

/// Review 10: an object git cannot read is an error, not a hang.
#[test]
fn a_missing_object_fails_instead_of_hanging() {
    let r = repo(".", "");
    let d = r.path();
    for i in 0..3000 {
        write(
            d,
            &format!("n{i}.md"),
            &format!("# n{i}\n[[n0]]\n{}\n", "0".repeat(200)),
        );
    }
    git(d, &["add", "-A"]);
    git(
        d,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "100644,1234567890123456789012345678901234567890,0000.md",
        ],
    );
    let mut child = std::process::Command::new(bin())
        .current_dir(d)
        .args(["check", "--staged"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s.code();
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            let _ = child.kill();
            panic!("check --staged hung");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(code, Some(2));
}

/// Review 11: files git ignores are not part of a change.
#[test]
fn ignored_notes_are_not_changes() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, ".gitignore", "v/scratch.md\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/scratch.md", "# scratch\n[[nowhere]]\n");
    assert_eq!(items(d, &["check", "--changed"]), Vec::<Value>::new());
}

/// Review 12: `drift` must not run a program that the target repository's config names
/// (here a GPG program for signature display).
#[cfg(unix)]
#[test]
fn drift_runs_no_program_from_the_target_repository() {
    let (p, first) = project();
    let pd = p.path();
    let tree = String::from_utf8_lossy(&git(pd, &["write-tree"]).stdout)
        .trim()
        .to_string();
    let parent = String::from_utf8_lossy(&git(pd, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let raw = format!(
        "tree {tree}\nparent {parent}\nauthor t <t@t> 1700000000 +0000\ncommitter t <t@t> 1700000000 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nsigned\n"
    );
    write(pd, "c.txt", &raw);
    let signed =
        String::from_utf8_lossy(&git(pd, &["hash-object", "-t", "commit", "-w", "c.txt"]).stdout)
            .trim()
            .to_string();
    git(pd, &["update-ref", "refs/heads/main", &signed]);
    let marker = pd.join("ran.txt");
    let gpg = pd.join("fakegpg");
    write(
        pd,
        "fakegpg",
        &format!(
            "#!/bin/sh\necho ran >> '{}'\ncat >/dev/null\nexit 1\n",
            marker.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&gpg, fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(pd, &["config", "log.showSignature", "true"]);
    git(pd, &["config", "gpg.program", gpg.to_str().unwrap()]);
    let r = repo("v", "[[drift]]\n");
    let d = r.path();
    write(
        d,
        "v/a.md",
        &format!(
            "---\npath: {}\nverified_commit: {first}\n---\n# a\n[[b]]\n",
            pd.display()
        ),
    );
    write(d, "v/b.md", "# b\n[[a]]\n");
    let found = items(d, &["check"]);
    assert!(found.iter().any(|f| f["rule"] == "drift"), "{found:?}");
    assert!(!marker.exists(), "drift ran the repository's gpg.program");
}

/// Reviews 13 and 14: a YAML comment is not a heading, and Markdown link targets are not
/// the line's words.
#[test]
fn forbid_link_near_ignores_yaml_comments_and_markdown_targets() {
    let config = "[[forbid_link]]\ntarget = \"^Issues/resolved/\"\nnear = \"(?i)\\\\bopen\\\\b\"\n";
    let r = repo("v", config);
    let d = r.path();
    write(d, "v/Issues/resolved/b.md", "# b\n[[P]]\n");
    write(d, "v/Issues/open/a.md", "# a\n[[P]]\n");
    write(
        d,
        "v/P.md",
        "---\n# open questions below\ntype: x\n---\n# P\n\n## Resolved\n\n- [a](Issues/open/a.md) and [b](Issues/resolved/b.md)\n- [[Issues/resolved/b]]\n",
    );
    let found: Vec<Value> = items(d, &["check"])
        .into_iter()
        .filter(|f| f["rule"] == "forbidden-link")
        .collect();
    assert_eq!(found, Vec::<Value>::new());
}

// Second review (2026-10-18).

/// A superproject whose config names a vault inside a submodule: its commits record nothing
/// of the vault, so they are not blocked; nor is an outer repository's commit by an untracked
/// inner repository holding the vault.
#[test]
fn commits_of_another_repository_are_not_checked_against_the_vault() {
    let sub_src = TempDir::new().unwrap();
    let s = sub_src.path();
    git(s, &["init", "-q", "-b", "main"]);
    write(s, "notes/a.md", "# a\n[[b]]\n");
    write(s, "notes/b.md", "# b\n[[a]]\n");
    git(s, &["add", "-A"]);
    git(s, &["commit", "-qm", "sub"]);
    let r = repo("sub/notes", "");
    let d = r.path();
    install_hook(d);
    fs::remove_dir_all(d.join("sub")).unwrap();
    git(
        d,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            s.to_str().unwrap(),
            "sub",
        ],
    );
    // The submodule has a staged broken link; the superproject commit does not record it.
    write(&d.join("sub"), "notes/a.md", "# a\n[[b]] [[broken]]\n");
    git(&d.join("sub"), &["add", "notes/a.md"]);
    write(d, "README.md", "x\n");
    git(d, &["add", "-A"]);
    let out = git_cmd(d)
        .args(["commit", "-qm", "super"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Committing in the submodule itself is checked (its own hook would run wikiknit there).
    assert_eq!(
        rules_of(&items(
            &d.join("sub"),
            &["--vault", "notes", "check", "--staged"]
        )),
        vec![pair("a", "unresolved-link")]
    );
}

/// A bare repository with its work tree elsewhere (dotfiles), vault in a subfolder, config
/// named with `--config`.
#[test]
fn separate_git_dir_with_the_vault_in_a_subfolder() {
    let home = TempDir::new().unwrap();
    let h = home.path();
    let bare = TempDir::new().unwrap();
    let b = bare.path().join("dot.git");
    git(
        h,
        &["init", "-q", "--bare", "-b", "main", b.to_str().unwrap()],
    );
    write(
        &b,
        "hooks/pre-commit",
        &format!(
            "#!/bin/sh\nexec '{}' --config notes/wikiknit.toml check --staged\n",
            bin().display()
        ),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            b.join("hooks/pre-commit"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    write(h, "notes/wikiknit.toml", "link_style = \"full-path\"\n");
    write(h, "notes/n/a.md", "# a\n[[n/b]]\n");
    write(h, "notes/n/b.md", "# b\n[[n/a]]\n");
    let d = |args: &[&str]| {
        git_cmd(h)
            .arg(format!("--git-dir={}", b.display()))
            .arg(format!("--work-tree={}", h.display()))
            .args(args)
            .output()
            .unwrap()
    };
    assert!(d(&["add", "-A"]).status.success());
    let out = d(&["commit", "-qm", "base"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    write(h, "notes/n/c.md", "# c\n[[n/a]]\n");
    d(&["add", "-A"]);
    let out = d(&["commit", "-qm", "valid"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    write(h, "notes/n/c.md", "# c\n[[n/a]] [[zz]]\n");
    d(&["add", "-A"]);
    assert!(!d(&["commit", "-qm", "broken"]).status.success());
}

/// A vault folder whose name git would read as pathspec magic.
#[test]
fn vault_folder_starting_with_a_colon() {
    let r = repo(":v x", "");
    let d = r.path();
    write(d, ":v x/a.md", "# a\n[[b]]\n");
    write(d, ":v x/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, ":v x/a.md", "# a\n[[b]] [[zz]]\n");
    git(d, &["add", "-A"]);
    assert_eq!(
        rules_of(&items(d, &["check", "--staged"])),
        vec![pair("a", "unresolved-link")]
    );
}

/// The vault moved (`git mv v w`, config `vault = "w"`): the last commit is read with its
/// own vault folder, so old findings stay old.
#[test]
fn a_moved_vault_keeps_its_old_findings_old() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]] [[gone]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    git(d, &["mv", "v", "w"]);
    write(d, "wikiknit.toml", "vault = \"w\"\n");
    git(d, &["add", "-A"]);
    let out = wk(d, &["check", "--staged"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

/// A vault reached through a symlink (`notes` -> `real/notes`).
#[cfg(unix)]
#[test]
fn a_symlinked_vault_folder_uses_each_versions_config() {
    let r = repo("notes", "");
    let d = r.path();
    fs::remove_dir_all(d.join("notes")).unwrap();
    write(d, "real/notes/a.md", "# a\n[[b]]\n");
    write(d, "real/notes/b.md", "# b\n[[a]]\n");
    std::os::unix::fs::symlink("real/notes", d.join("notes")).unwrap();
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "real/notes/a.md", "# a\n[[b]] [[zz]]\n");
    git(d, &["add", "-A"]);
    let out = wk(d, &["--format", "json", "check", "--staged"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("skipping") && !err.contains("does not load"),
        "{err}"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        rules_of(v["items"].as_array().unwrap()),
        vec![pair("a", "unresolved-link")]
    );
}

/// A short link that was already a `link-style` error stays old when its target moves.
#[test]
fn link_style_errors_are_keyed_by_the_link_as_written() {
    let r = repo("v", "link_style = \"full-path\"\n");
    let d = r.path();
    write(d, "v/INDEX.md", "# Index\n[[x]]\n");
    write(d, "v/A/x.md", "# x\n[[INDEX]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    fs::create_dir_all(d.join("v/B")).unwrap();
    git(d, &["mv", "v/A/x.md", "v/B/x.md"]);
    assert_eq!(items(d, &["check", "--staged"]), Vec::<Value>::new());
}

/// Commit ids YAML reads as numbers: `89e8573` (an infinite float) does not break the
/// frontmatter, and `1234567` / `0e12345` (numbers) still drive `drift` as written.
#[test]
fn commit_ids_that_look_like_numbers() {
    let p = TempDir::new().unwrap();
    git(p.path(), &["init", "-q", "-b", "main"]);
    let r = repo("v", "[[drift]]\n");
    let d = r.path();
    for (name, commit) in [("a", "89e8573"), ("b", "1234567"), ("c", "0e12345")] {
        write(
            d,
            &format!("v/{name}.md"),
            &format!(
                "---\npath: {}\nverified_commit: {commit}\n---\n# {name}\n[[x]]\n",
                p.path().display()
            ),
        );
    }
    write(d, "v/x.md", "# x\n[[a]] [[b]] [[c]]\n");
    let found = items(d, &["check"]);
    assert!(
        found.iter().all(|f| f["rule"] != "frontmatter-invalid"),
        "{found:?}"
    );
    let drift: Vec<String> = found
        .iter()
        .filter(|f| f["rule"] == "drift")
        .map(|f| f["message"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(drift.len(), 3, "{found:?}");
    for c in ["89e8573", "1234567", "0e12345"] {
        assert!(drift.iter().any(|m| m.contains(c)), "{drift:?}");
    }
}

/// `--changed` sees a note moved with plain `mv` (not staged) as the same note.
#[test]
fn changed_follows_a_plain_mv() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, "v/lonely.md", "# lonely\n[[a]] [[gone]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    fs::create_dir_all(d.join("v/sub")).unwrap();
    fs::rename(d.join("v/lonely.md"), d.join("v/sub/lonely.md")).unwrap();
    assert_eq!(items(d, &["check", "--changed"]), Vec::<Value>::new());
}

/// `--staged` writes no objects into the repository.
#[test]
fn staged_leaves_the_object_store_alone() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/c.md", "# c\n[[a]]\n");
    git(d, &["add", "-A"]);
    let count = || {
        String::from_utf8_lossy(&git(d, &["count-objects"]).stdout)
            .trim()
            .to_string()
    };
    let before = count();
    assert_eq!(wk(d, &["check", "--staged"]).status.code(), Some(0));
    assert_eq!(count(), before);
}

/// A staged config that does not load fails the check (the commit would record it), even
/// when the one on disk is fine.
#[test]
fn a_broken_staged_config_fails() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "wikiknit.toml", "vault = \"v\"\nbogus = 1\n");
    git(d, &["add", "wikiknit.toml"]);
    write(d, "wikiknit.toml", "vault = \"v\"\n");
    let out = wk(d, &["check", "--staged"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("staged wikiknit.toml does not load"));
}

/// A symlink from the vault to a folder elsewhere in the repository resolves in the copies.
#[cfg(unix)]
#[test]
fn symlinks_out_of_the_vault_resolve_in_copies() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "shared/s.md", "# s\n");
    write(d, "v/a.md", "# a\n[[shared/s]]\n");
    std::os::unix::fs::symlink("../shared", d.join("v/shared")).unwrap();
    git(d, &["add", "-A"]);
    let plain = rules_of(&items(d, &["check"]));
    let out = wk(d, &["--format", "json", "check", "--staged"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.contains("skipping") && !err.contains("does not load"),
        "{err}"
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(rules_of(v["items"].as_array().unwrap()), plain);
}

// Third review (2026-10-18).

/// A crafted tree with a symlink `x` and a folder `x/` must not write through the link.
#[cfg(unix)]
#[test]
fn a_crafted_tree_cannot_write_outside_the_copy() {
    let victim = TempDir::new().unwrap();
    let v = victim.path();
    write(v, "keep.md", "# precious\n");
    let r = repo(".", "");
    let d = r.path();
    write(d, "a.md", "# a\n[[b]]\n");
    write(d, "b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    let stdin_git = |args: &[&str], input: &str| {
        let mut child = git_cmd(d)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let link = stdin_git(&["hash-object", "-w", "--stdin"], &v.display().to_string());
    let pwned = stdin_git(&["hash-object", "-w", "--stdin"], "# pwned\n");
    let sub = stdin_git(&["mktree"], &format!("100644 blob {pwned}\tkeep.md\n"));
    let blob = |p: &str| {
        String::from_utf8_lossy(&git(d, &["rev-parse", &format!("HEAD:{p}")]).stdout)
            .trim()
            .to_string()
    };
    let tree = stdin_git(
        &["mktree", "--missing"],
        &format!(
            "100644 blob {}\ta.md\n100644 blob {}\tb.md\n100644 blob {}\twikiknit.toml\n120000 blob {link}\tx\n040000 tree {sub}\tx\n",
            blob("a.md"),
            blob("b.md"),
            blob("wikiknit.toml")
        ),
    );
    let head = String::from_utf8_lossy(&git(d, &["rev-parse", "HEAD"]).stdout)
        .trim()
        .to_string();
    let evil =
        String::from_utf8_lossy(&git(d, &["commit-tree", &tree, "-p", &head, "-m", "evil"]).stdout)
            .trim()
            .to_string();
    git(d, &["update-ref", "refs/heads/main", &evil]);
    let _ = wk(d, &["check", "--changed"]);
    let _ = wk(d, &["check", "--staged"]);
    assert_eq!(
        fs::read_to_string(v.join("keep.md")).unwrap(),
        "# precious\n"
    );
}

/// `--changed` runs no clean filter the repository configures.
#[test]
fn changed_runs_no_clean_filter() {
    let r = repo("v", "");
    let d = r.path();
    let marker = d.join("filter-ran");
    write(
        d,
        "filter.sh",
        &format!("#!/bin/sh\necho ran >> '{}'\ncat\n", marker.display()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(d.join("filter.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    git(
        d,
        &[
            "config",
            "filter.x.clean",
            d.join("filter.sh").to_str().unwrap(),
        ],
    );
    write(d, ".git/info/attributes", "*.md filter=x\n");
    write(d, "v/a.md", "# a\n[[b]] more\n");
    fs::remove_file(d.join("v/b.md")).unwrap();
    write(d, "v/c.md", "# b\n[[a]]\n");
    let _ = wk(d, &["check", "--changed"]);
    assert!(!marker.exists(), "a clean filter ran");
}

/// A repository whose path has a `:` (it would split an alternates variable).
#[test]
fn staged_works_in_a_path_with_a_colon() {
    let outer = TempDir::new().unwrap();
    let d = outer.path().join("a:b repo");
    fs::create_dir_all(&d).unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    write(&d, "wikiknit.toml", "vault = \".\"\n");
    write(&d, "a.md", "# a\n[[b]]\n");
    write(&d, "b.md", "# b\n[[a]]\n");
    git(&d, &["add", "-A"]);
    git(&d, &["commit", "-qm", "base"]);
    write(&d, "a.md", "# a\n[[b]] [[zz]]\n");
    git(&d, &["add", "-A"]);
    let out = wk(&d, &["check", "--staged"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An untracked note that is a symlink to a device does not hang `--changed`.
#[cfg(unix)]
#[test]
fn an_untracked_symlink_to_a_device_does_not_hang() {
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    write(d, "v/gone.md", "# gone\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    fs::remove_file(d.join("v/gone.md")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", d.join("v/zero.md")).unwrap();
    let mut child = std::process::Command::new(bin())
        .current_dir(d)
        .args(["check", "--changed"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if start.elapsed() > std::time::Duration::from_secs(20) {
            let _ = child.kill();
            panic!("check --changed hung");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A stray `GIT_DIR` in the shell does not change what a manual `--changed` reads.
#[test]
fn a_stray_git_dir_does_not_change_a_manual_check() {
    let elsewhere = TempDir::new().unwrap();
    git(elsewhere.path(), &["init", "-q", "-b", "main"]);
    let r = repo("v", "");
    let d = r.path();
    write(d, "v/a.md", "# a\n[[b]]\n");
    write(d, "v/b.md", "# b\n[[a]]\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-qm", "base"]);
    write(d, "v/a.md", "# a\n[[b]] [[zz]]\n");
    let out = std::process::Command::new(bin())
        .current_dir(d)
        .env("GIT_DIR", elsewhere.path().join(".git"))
        .args(["check", "--changed"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
