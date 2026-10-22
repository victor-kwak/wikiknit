//! `wikiknit init` and `wikiknit snippet`: the real binary and real git in temp folders, with
//! the user's git config shut out (a global `core.hooksPath` would change what init does).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("wikiknit")
}

/// The environment of every command: no user git config, `wikiknit` on PATH (for the hooks).
fn isolate(c: &mut Command, global: &Path) {
    let dir = bin().parent().unwrap().to_path_buf();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(&path));
    c.env("PATH", std::env::join_paths(paths).unwrap())
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
    for v in ["GIT_DIR", "GIT_INDEX_FILE", "GIT_WORK_TREE"] {
        c.env_remove(v);
    }
}

/// A temp folder with an empty global git config of its own.
struct Sandbox {
    dir: TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("work")).unwrap();
        fs::write(dir.path().join("gitconfig"), "").unwrap();
        Self { dir }
    }

    /// The folder the tests work in.
    fn work(&self) -> PathBuf {
        self.dir.path().join("work").canonicalize().unwrap()
    }

    fn global(&self) -> PathBuf {
        self.dir.path().join("gitconfig")
    }

    fn git_out(&self, dir: &Path, args: &[&str]) -> Output {
        let mut c = Command::new("git");
        c.current_dir(dir).args(args);
        isolate(&mut c, &self.global());
        c.output().unwrap()
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git_out(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn wk(&self, dir: &Path, args: &[&str]) -> Output {
        let mut c = Command::new(bin());
        c.current_dir(dir).args(args);
        isolate(&mut c, &self.global());
        c.output().unwrap()
    }
}

fn write(dir: &Path, path: &str, body: &str) {
    let p = dir.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Every file under `dir` (but `.git/objects` etc.) with its content, to show a run wrote nothing.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if rel.starts_with(".git/") && !rel.starts_with(".git/hooks") && rel != ".git/config" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((rel, fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

/// An Obsidian vault in `notes/`: full-path links, an index linking every note, a templates
/// folder named by Obsidian's settings.
fn obsidian_vault(root: &Path) {
    write(root, "notes/.obsidian/app.json", "{}");
    write(
        root,
        "notes/.obsidian/templates.json",
        r#"{"folder": "Tpl"}"#,
    );
    write(root, "notes/Tpl/daily.md", "# {{date}}\n[[nowhere]]\n");
    let mut index = String::from("# Index\n");
    for i in 0..6 {
        index.push_str(&format!("- [[Projects/p{i}]]\n"));
        write(
            root,
            &format!("notes/Projects/p{i}.md"),
            &format!("# p{i}\n[[INDEX]] [[Projects/p{}]]\n", (i + 1) % 6),
        );
    }
    write(root, "notes/INDEX.md", &index);
}

#[test]
fn init_sets_up_a_vault_in_a_subfolder_and_the_hook_blocks_a_broken_link() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(&w, "README.md", "# project\n");
    obsidian_vault(&w);
    s.git(&w, &["add", "-A"]);
    s.git(&w, &["commit", "-qm", "base"]);

    let out = s.wk(&w, &["init"]);
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("vault   notes (it holds .obsidian/)"),
        "{text}"
    );
    assert!(
        text.contains("hook    wrote: .git/hooks/pre-commit"),
        "{text}"
    );
    assert!(
        text.contains(
            "Commit wikiknit.toml with the instruction file from step 1, and nothing else."
        ),
        "{text}"
    );

    let config = fs::read_to_string(w.join("wikiknit.toml")).unwrap();
    let table: toml::Table = config.parse().unwrap();
    assert_eq!(table["vault"].as_str(), Some("notes"));
    assert_eq!(table["link_style"].as_str(), Some("full-path"));
    assert_eq!(
        table["ignore"].as_array().unwrap()[0].as_str(),
        Some("Tpl/")
    );
    let hubs: Vec<&str> = table["hubs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    assert_eq!(hubs, ["INDEX"]);
    for hook in ["pre-commit", "pre-merge-commit"] {
        let body = fs::read_to_string(w.join(".git/hooks").join(hook)).unwrap();
        assert!(
            body.contains("\nwikiknit check --staged || exit $?\n"),
            "{body}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(w.join(".git/hooks").join(hook))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }
    // The config loads, and the vault is clean (the template's broken link is ignored).
    let check = s.wk(&w, &["check"]);
    assert!(
        check.status.success(),
        "{}{}",
        stdout(&check),
        stderr(&check)
    );

    // A second run changes nothing and says so.
    let before = snapshot(&w);
    let again = s.wk(&w, &["init"]);
    assert!(again.status.success());
    assert!(
        stdout(&again).contains("Nothing to change: this vault is set up."),
        "{}",
        stdout(&again)
    );
    assert_eq!(snapshot(&w), before);

    // The installed hook blocks a commit that adds a broken link, and passes a clean one.
    s.git(&w, &["add", "wikiknit.toml"]);
    s.git(&w, &["commit", "-qm", "wikiknit"]);
    write(
        &w,
        "notes/Projects/p0.md",
        "# p0\n[[INDEX]] [[Projects/gone]]\n",
    );
    s.git(&w, &["add", "-A"]);
    let blocked = s.git_out(&w, &["commit", "-qm", "broken"]);
    assert!(!blocked.status.success(), "the hook let a broken link in");
    assert!(
        stderr(&blocked).contains("unresolved-link"),
        "{}",
        stderr(&blocked)
    );
    write(
        &w,
        "notes/Projects/p0.md",
        "# p0\n[[INDEX]] [[Projects/p1]] [[Projects/p2]]\n",
    );
    s.git(&w, &["add", "-A"]);
    s.git(&w, &["commit", "-qm", "fine"]);
}

#[test]
fn init_at_the_repo_top_without_obsidian() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    for i in 0..6 {
        write(
            &w,
            &format!("topics/t{i}.md"),
            &format!("# t{i}\n[[t{}]]\n", (i + 1) % 6),
        );
    }
    write(&w, "home.md", "# home\n[[t0]]\n");
    // Without .obsidian/ it does not guess: it names the folder with the linked notes.
    let before = snapshot(&w);
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains(
            "the 7 Markdown file(s) with a [[wikilink]] are all in .: if that is the vault, run \
             `wikiknit --vault . init`"
        ),
        "{}",
        stderr(&out)
    );
    assert_eq!(snapshot(&w), before);
    let out = s.wk(&w, &["--vault", ".", "init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let table: toml::Table = fs::read_to_string(w.join("wikiknit.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(table["vault"].as_str(), Some("."));
    assert_eq!(table["link_style"].as_str(), Some("shortest"));
    assert!(!table.contains_key("hubs"), "{table}");
    assert!(!table.contains_key("ignore"), "{table}");
}

#[test]
fn init_with_too_few_links_sets_no_link_style() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(&w, "a.md", "# a\n[[b]]\n");
    write(&w, "b.md", "# b\n");
    let out = s.wk(&w, &["--vault", ".", "init"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("link_style: not set"),
        "{}",
        stdout(&out)
    );
    let config = fs::read_to_string(w.join("wikiknit.toml")).unwrap();
    assert!(!config.contains("link_style ="), "{config}");
}

#[test]
fn init_writes_into_a_local_hooks_path_and_lists_the_hooks_to_commit() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    s.git(&w, &["config", "core.hooksPath", ".githooks"]);
    obsidian_vault(&w);
    let out = s.wk(&w, &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(w.join(".githooks/pre-commit").is_file());
    assert!(w.join(".githooks/pre-merge-commit").is_file());
    assert!(!w.join(".git/hooks/pre-commit").exists());
    assert!(
        stdout(&out).contains(
            "Commit wikiknit.toml and .githooks/pre-commit and .githooks/pre-merge-commit"
        ),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("in a new clone"), "{}", stdout(&out));
}

#[test]
fn init_leaves_a_foreign_hook_alone_and_says_what_to_add() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    let foreign = "#!/bin/sh\nexec make lint\n";
    write(&w, ".git/hooks/pre-commit", foreign);
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let text = stdout(&out);
    assert!(
        text.contains(
            "hook    not changed: .git/hooks/pre-commit exists and wikiknit did not write it"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "command -v wikiknit >/dev/null 2>&1 && { wikiknit check --staged || exit $? ; }"
        ),
        "{text}"
    );
    assert_eq!(
        fs::read_to_string(w.join(".git/hooks/pre-commit")).unwrap(),
        foreign
    );
    // The other hook is still installed, and the config written.
    assert!(w.join(".git/hooks/pre-merge-commit").is_file());
    assert!(w.join("wikiknit.toml").is_file());

    // After the user adds the line, init sees the hook runs wikiknit.
    write(
        &w,
        ".git/hooks/pre-commit",
        "#!/bin/sh\ncommand -v wikiknit >/dev/null 2>&1 && { wikiknit check --staged || exit $? ; }\nexec make lint\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            w.join(".git/hooks/pre-commit"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    let again = s.wk(&w, &["init"]);
    assert!(again.status.success(), "{}", stdout(&again));
    assert!(
        stdout(&again).contains("Nothing to change"),
        "{}",
        stdout(&again)
    );
}

#[test]
fn init_points_the_pre_commit_framework_at_its_config() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, ".pre-commit-config.yaml", "repos: []\n");
    write(
        &w,
        ".git/hooks/pre-commit",
        "#!/usr/bin/env python3\n# generated by pre-commit\n",
    );
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains(".pre-commit-config.yaml"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn init_does_not_edit_a_global_hooks_path() {
    let s = Sandbox::new();
    let shared = s.dir.path().join("shared-hooks");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        s.global(),
        format!("[core]\n\thooksPath = {}\n", shared.display()),
    )
    .unwrap();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out)
            .contains("in your global git config: those hooks serve other repositories too"),
        "{}",
        stdout(&out)
    );
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 0);
    assert!(!w.join(".git/hooks/pre-commit").exists());
}

#[test]
fn init_does_not_edit_a_local_hooks_path_outside_the_repo() {
    let s = Sandbox::new();
    let shared = s.dir.path().join("shared-hooks");
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    s.git(&w, &["config", "core.hooksPath", shared.to_str().unwrap()]);
    obsidian_vault(&w);
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("outside this repository"),
        "{}",
        stdout(&out)
    );
    assert!(!shared.exists());
}

#[test]
fn init_without_git_writes_the_config_and_skips_hooks() {
    let s = Sandbox::new();
    let w = s.work();
    obsidian_vault(&w);
    let out = s.wk(&w.join("notes"), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("hooks   skipped"), "{text}");
    assert!(!text.contains("Commit "), "{text}");
    // Run from inside the vault: the config goes in the current folder.
    let table: toml::Table = fs::read_to_string(w.join("notes/wikiknit.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(table["vault"].as_str(), Some("."));
}

#[test]
fn init_with_a_config_in_the_vault_folder_names_it_in_the_hook() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, "notes/wikiknit.toml", "link_style = \"full-path\"\n");
    s.git(&w, &["add", "-A"]);
    s.git(&w, &["commit", "-qm", "base"]);
    // From the top: the config is not found from here, so it is named.
    let out = s.wk(&w, &["--config", "notes/wikiknit.toml", "init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("config  ok: notes/wikiknit.toml exists"),
        "{}",
        stdout(&out)
    );
    let hook = fs::read_to_string(w.join(".git/hooks/pre-commit")).unwrap();
    assert!(
        hook.contains("\nwikiknit --config notes/wikiknit.toml check --staged || exit $?\n"),
        "{hook}"
    );
    // The same result from inside the vault, where the config is found.
    let again = s.wk(&w.join("notes"), &["init"]);
    assert!(
        stdout(&again).contains("Nothing to change"),
        "{}",
        stdout(&again)
    );
    // The hook works from the top, where git runs it.
    write(&w, "notes/INDEX.md", "# Index\n[[Projects/gone]]\n");
    s.git(&w, &["add", "-A"]);
    let blocked = s.git_out(&w, &["commit", "-qm", "broken"]);
    assert!(!blocked.status.success());
    assert!(
        stderr(&blocked).contains("unresolved-link"),
        "{}",
        stderr(&blocked)
    );
}

#[test]
fn init_adds_a_second_vault_to_its_own_hook() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, "other/a.md", "# a\n[[b]]\n");
    write(&w, "other/b.md", "# b\n[[a]]\n");
    assert!(s.wk(&w, &["init"]).status.success());
    let out = s.wk(
        &w,
        &[
            "--vault",
            "other",
            "--config",
            "other/wikiknit.toml",
            "init",
        ],
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("hook    added this config's check to: .git/hooks/pre-commit"),
        "{}",
        stdout(&out)
    );
    let hook = fs::read_to_string(w.join(".git/hooks/pre-commit")).unwrap();
    assert!(
        hook.contains("\nwikiknit check --staged || exit $?\n"),
        "{hook}"
    );
    assert!(
        hook.contains("\nwikiknit --config other/wikiknit.toml check --staged || exit $?\n"),
        "{hook}"
    );
}

#[test]
fn init_refuses_a_top_config_for_another_vault() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, "other/a.md", "# a\n[[b]]\n");
    assert!(s.wk(&w, &["init"]).status.success());
    let before = snapshot(&w);
    let out = s.wk(&w, &["--vault", "other", "init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("names another vault"),
        "{}",
        stderr(&out)
    );
    assert_eq!(snapshot(&w), before);
}

#[test]
fn init_dry_run_writes_nothing() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    let before = snapshot(&w);
    let out = s.wk(&w, &["init", "--dry-run"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("config  would write: wikiknit.toml"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("hook    would write: .git/hooks/pre-commit"),
        "{}",
        stdout(&out)
    );
    assert_eq!(snapshot(&w), before);
}

#[test]
fn init_refuses_to_guess_between_vaults() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(&w, "a/.obsidian/app.json", "{}");
    write(&w, "a/x.md", "# x\n");
    write(&w, "b/.obsidian/app.json", "{}");
    write(&w, "b/y.md", "# y\n");
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("several Obsidian vaults here (a, b)"),
        "{}",
        stderr(&out)
    );
    assert!(!w.join("wikiknit.toml").exists());
}

#[test]
fn init_finds_no_vault_without_notes() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(&w, "README.md", "# no links here\n");
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("no Obsidian vault"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn snippet_prints_and_writes_one_block() {
    let s = Sandbox::new();
    let w = s.work();
    let printed = stdout(&s.wk(&w, &["snippet"]));
    assert!(
        printed.starts_with("<!-- wikiknit:start -->\n"),
        "{printed}"
    );
    assert!(printed.ends_with("<!-- wikiknit:end -->\n"), "{printed}");
    assert!(printed.contains(env!("CARGO_PKG_VERSION")));
    assert!(printed.contains("wikiknit check --changed"));

    // A new file.
    let out = s.wk(&w, &["snippet", "--into", "AGENTS.md"]);
    assert!(stdout(&out).contains("created"), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(w.join("AGENTS.md")).unwrap(), printed);

    // An existing file without the block: added at the end, after a blank line.
    write(&w, "CLAUDE.md", "# Project\n\nRules.");
    let out = s.wk(&w, &["snippet", "--into", "CLAUDE.md"]);
    assert!(stdout(&out).contains("added at the end"));
    assert_eq!(
        fs::read_to_string(w.join("CLAUDE.md")).unwrap(),
        format!("# Project\n\nRules.\n\n{printed}")
    );

    // An old block is replaced in place; the text around it is kept.
    write(
        &w,
        "CLAUDE.md",
        "# Project\n<!-- wikiknit:start -->\nold text\n<!-- wikiknit:end -->\nAfter.\n",
    );
    let out = s.wk(&w, &["snippet", "--into", "CLAUDE.md"]);
    assert!(stdout(&out).contains("replaced"));
    assert_eq!(
        fs::read_to_string(w.join("CLAUDE.md")).unwrap(),
        format!("# Project\n{printed}After.\n")
    );
    let out = s.wk(&w, &["snippet", "--into", "CLAUDE.md"]);
    assert!(stdout(&out).contains("already current"));

    // CRLF files stay CRLF.
    write(&w, "win.md", "# Win\r\n");
    s.wk(&w, &["snippet", "--into", "win.md"]);
    let win = fs::read_to_string(w.join("win.md")).unwrap();
    assert!(!win.replace("\r\n", "").contains('\n'), "{win:?}");

    // Broken markers: an error, and the file is not changed.
    let broken = "# P\n<!-- wikiknit:start -->\nno end\n";
    write(&w, "broken.md", broken);
    let out = s.wk(&w, &["snippet", "--into", "broken.md"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("do not make one block"),
        "{}",
        stderr(&out)
    );
    assert_eq!(fs::read_to_string(w.join("broken.md")).unwrap(), broken);
}

#[test]
fn init_from_a_subfolder_finds_the_vault_below_the_top() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, "src/main.rs", "fn main() {}\n");
    let out = s.wk(&w.join("src"), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("vault   ../notes") || stdout(&out).contains("/notes (it holds"),
        "{}",
        stdout(&out)
    );
    assert!(w.join("wikiknit.toml").is_file());
    assert!(w.join(".git/hooks/pre-commit").is_file());
}

#[test]
fn init_in_a_linked_worktree_uses_the_shared_hooks() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    s.git(&w, &["add", "-A"]);
    s.git(&w, &["commit", "-qm", "base"]);
    let linked = s.dir.path().join("linked");
    s.git(
        &w,
        &[
            "worktree",
            "add",
            "-q",
            linked.to_str().unwrap(),
            "-b",
            "side",
        ],
    );
    let linked = linked.canonicalize().unwrap();
    let out = s.wk(&linked, &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(linked.join("wikiknit.toml").is_file());
    assert!(
        w.join(".git/hooks/pre-commit").is_file(),
        "{}",
        stdout(&out)
    );
    // The hook blocks a broken link committed in the linked worktree.
    s.git(&linked, &["add", "wikiknit.toml"]);
    s.git(&linked, &["commit", "-qm", "wikiknit"]);
    write(&linked, "notes/INDEX.md", "# Index\n[[Projects/gone]]\n");
    s.git(&linked, &["add", "-A"]);
    let blocked = s.git_out(&linked, &["commit", "-qm", "broken"]);
    assert!(!blocked.status.success());
    assert!(
        stderr(&blocked).contains("unresolved-link"),
        "{}",
        stderr(&blocked)
    );
}

#[cfg(unix)]
#[test]
fn init_does_not_write_through_a_symlinked_hook() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    let target = s.dir.path().join("elsewhere-hook");
    let body = format!(
        "#!/bin/sh\n{}: not really\nexit 0\n",
        "# Written by `wikiknit init`"
    );
    fs::write(&target, &body).unwrap();
    fs::create_dir_all(w.join(".git/hooks")).unwrap();
    std::os::unix::fs::symlink(&target, w.join(".git/hooks/pre-commit")).unwrap();
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert_eq!(fs::read_to_string(&target).unwrap(), body);
}

#[test]
fn init_finds_a_config_in_the_vault_folder_from_the_top() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(&w, "notes/wikiknit.toml", "link_style = \"full-path\"\n");
    let first = s.wk(&w, &["init"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stdout(&first).contains("config  ok: notes/wikiknit.toml exists"),
        "{}",
        stdout(&first)
    );
    assert!(!w.join("wikiknit.toml").exists());
    let hook = fs::read_to_string(w.join(".git/hooks/pre-commit")).unwrap();
    assert!(
        hook.contains("wikiknit --config notes/wikiknit.toml check --staged"),
        "{hook}"
    );
    let before = snapshot(&w);
    let again = s.wk(&w, &["init"]);
    assert!(
        stdout(&again).contains("Nothing to change"),
        "{}",
        stdout(&again)
    );
    assert_eq!(snapshot(&w), before);
}

#[test]
fn init_does_not_accept_a_hook_for_another_config() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    write(
        &w,
        ".git/hooks/pre-commit",
        "#!/bin/sh\nexec wikiknit --config other/wikiknit.toml check --staged\n",
    );
    let out = s.wk(&w, &["init"]);
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    assert!(
        stdout(&out).contains("hook    not changed"),
        "{}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("Nothing to change"),
        "{}",
        stdout(&out)
    );
    // A second run: still incomplete, and it does not say the vault is set up.
    let again = s.wk(&w, &["init"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        !stdout(&again).contains("Nothing to change"),
        "{}",
        stdout(&again)
    );
}

/// The line init gives for a shared hooks folder does nothing in a repository without the config.
#[test]
fn the_line_for_a_shared_hook_does_not_block_other_repositories() {
    let s = Sandbox::new();
    let shared = s.dir.path().join("shared-hooks");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        s.global(),
        format!("[core]\n\thooksPath = {}\n", shared.display()),
    )
    .unwrap();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    obsidian_vault(&w);
    let out = s.wk(&w, &["init"]);
    let text = stdout(&out);
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Add to its pre-commit: "))
        .unwrap_or_else(|| panic!("{text}"))
        .to_string();
    write(
        &shared,
        "pre-commit",
        &format!("#!/bin/sh\n{line}\nexit 0\n"),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(shared.join("pre-commit"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Another repository: its commits go through.
    let other = s.dir.path().join("other");
    fs::create_dir_all(&other).unwrap();
    s.git(&other, &["init", "-q", "-b", "main"]);
    write(&other, "x.md", "[[nowhere]]\n");
    s.git(&other, &["add", "-A"]);
    s.git(&other, &["commit", "-qm", "other"]);
    // This one: the setup goes in, then a broken link is blocked.
    s.git(&w, &["add", "-A"]);
    s.git(&w, &["commit", "-qm", "base"]);
    write(&w, "notes/INDEX.md", "# Index\n[[Projects/gone]]\n");
    s.git(&w, &["add", "-A"]);
    let blocked = s.git_out(&w, &["commit", "-qm", "broken"]);
    assert!(!blocked.status.success(), "{}", stderr(&blocked));
}

#[test]
fn init_trusts_the_notes_over_obsidian_settings_and_finds_no_hubs_in_a_dense_vault() {
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(
        &w,
        "v/.obsidian/app.json",
        r#"{"newLinkFormat": "absolute"}"#,
    );
    // Short links into a folder, too few to tell a style, but none full-path.
    for i in 0..4 {
        write(&w, &format!("v/P/p{i}.md"), &format!("# p{i}\n[[root]]\n"));
    }
    write(&w, "v/root.md", "# root\n[[p0]] [[p1]] [[p2]] [[p3]]\n");
    let out = s.wk(&w, &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let config = fs::read_to_string(w.join("wikiknit.toml")).unwrap();
    assert!(!config.contains("link_style ="), "{config}");

    // Ten notes each linking five others: no note stands out as an index.
    let s = Sandbox::new();
    let w = s.work();
    s.git(&w, &["init", "-q", "-b", "main"]);
    write(&w, "v/.obsidian/app.json", "{}");
    for i in 0..10 {
        let links: String = (1..=5).map(|k| format!("[[n{}]] ", (i + k) % 10)).collect();
        write(&w, &format!("v/n{i}.md"), &format!("# n{i}\n{links}\n"));
    }
    let out = s.wk(&w, &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let config = fs::read_to_string(w.join("wikiknit.toml")).unwrap();
    assert!(!config.contains("hubs ="), "{config}");
}

#[test]
fn init_leaves_no_config_that_does_not_load() {
    let s = Sandbox::new();
    let home = s.dir.path().join("home");
    write(&home, ".obsidian/app.json", "{}");
    write(&home, "a.md", "# a\n");
    // Outside git, run from the home folder: a vault that is the home folder is refused.
    let mut c = Command::new(bin());
    c.current_dir(&home).arg("init");
    isolate(&mut c, &s.global());
    c.env("HOME", &home);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stdout(&out));
    assert!(stderr(&out).contains("home directory"), "{}", stderr(&out));
    let left: Vec<_> = fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".toml"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn snippet_ignores_markers_in_code_and_keeps_a_byte_order_mark() {
    let s = Sandbox::new();
    let w = s.work();
    let printed = stdout(&s.wk(&w, &["snippet"]));
    let doc = "# P\n```md\n<!-- wikiknit:start -->\nexample\n<!-- wikiknit:end -->\n```\n";
    write(&w, "A.md", doc);
    let out = s.wk(&w, &["snippet", "--into", "A.md"]);
    assert!(
        stdout(&out).contains("added at the end"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(w.join("A.md")).unwrap(),
        format!("{doc}\n{printed}")
    );
    // Run again: the real block is found, the example is left alone.
    let out = s.wk(&w, &["snippet", "--into", "A.md"]);
    assert!(stdout(&out).contains("already current"), "{}", stdout(&out));

    write(
        &w,
        "B.md",
        "\u{feff}<!-- wikiknit:start -->\nold\n<!-- wikiknit:end -->\n",
    );
    let out = s.wk(&w, &["snippet", "--into", "B.md"]);
    assert!(
        stdout(&out).contains("replaced"),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(w.join("B.md")).unwrap(),
        format!("\u{feff}{printed}")
    );
}

#[cfg(unix)]
#[test]
fn snippet_writes_through_a_symlinked_file() {
    let s = Sandbox::new();
    let w = s.work();
    write(&w, "AGENTS.md", "# Agents\n");
    std::os::unix::fs::symlink("AGENTS.md", w.join("CLAUDE.md")).unwrap();
    let out = s.wk(&w, &["snippet", "--into", "CLAUDE.md"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        fs::symlink_metadata(w.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(w.join("AGENTS.md"))
            .unwrap()
            .contains("wikiknit:start")
    );
}
