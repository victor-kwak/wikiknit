//! End to end: the PreToolUse commit gate (`wikiknit hook pre-commit`). Each test builds a git
//! repo with a vault folder inside, feeds the hook the JSON Claude Code sends, and checks the
//! exit code (0 = allow, 2 = hold).

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::json;
use tempfile::TempDir;

/// A temp folder with `repo/` (a git repo: `wikiknit.toml`, `vault/a.md`, `vault/b.md`, one
/// commit), `other/` (a plain folder), and `state/` (a private `XDG_STATE_HOME`).
struct Setup {
    root: TempDir,
}

impl Setup {
    fn new(config: &str) -> Self {
        let root = TempDir::new().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join("vault")).unwrap();
        fs::create_dir_all(root.path().join("other")).unwrap();
        fs::create_dir_all(root.path().join("state")).unwrap();
        fs::write(repo.join("wikiknit.toml"), config).unwrap();
        fs::write(repo.join("vault/a.md"), "# a\n[[b]]\n").unwrap();
        fs::write(repo.join("vault/b.md"), "# b\n[[a]]\n").unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        Self { root }
    }

    fn standard() -> Self {
        Self::new("vault = \"vault\"\n")
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn other(&self) -> PathBuf {
        self.root.path().join("other")
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn wk(&self, cwd: &Path) -> Command {
        let mut cmd = Command::cargo_bin("wikiknit").unwrap();
        cmd.current_dir(cwd)
            .env("XDG_STATE_HOME", self.state())
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("WIKIKNIT_SESSION")
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("CLAUDE_ENV_FILE")
            .env_remove("WIKIKNIT_HOOK_TIMEOUT_MS");
        cmd
    }

    fn session_start(&self, session: &str) {
        let out = self
            .wk(&self.repo())
            .args(["hook", "session-start"])
            .write_stdin(json!({"session_id": session, "cwd": self.repo()}).to_string())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
    }

    fn mark(&self, session: &str) {
        self.wk(&self.repo())
            .args(["mark", "--session", session])
            .assert()
            .success();
    }

    /// Run the pre-commit hook for `command`, run from `cwd`; `project` is `CLAUDE_PROJECT_DIR`.
    fn pre_commit_in(
        &self,
        cwd: &Path,
        project: Option<&Path>,
        command: &str,
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut cmd = self.wk(cwd);
        if let Some(p) = project {
            cmd.env("CLAUDE_PROJECT_DIR", p);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.args(["hook", "pre-commit"])
            .write_stdin(
                json!({"session_id": "s1", "cwd": cwd, "tool_input": {"command": command}})
                    .to_string(),
            )
            .output()
            .unwrap()
    }

    /// The pre-commit hook for `command` run in the repo, with the repo as the project dir.
    fn pre_commit(&self, command: &str) -> std::process::Output {
        let repo = self.repo();
        self.pre_commit_in(&repo, Some(&repo), command, &[])
    }

    /// Edit a note so that it needs a link pass.
    fn edit(&self) {
        fs::write(self.repo().join("vault/a.md"), "# a\nedited [[b]]\n").unwrap();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn code(out: &std::process::Output) -> Option<i32> {
    out.status.code()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// H1: the vault is read before the command runs, so a command that writes a note and commits
/// must be held, whatever is pending now.
#[test]
fn writing_and_committing_in_one_command_is_held() {
    let s = Setup::standard();
    s.session_start("s1");
    s.mark("s1");
    for command in [
        "printf '# C\\nnew\\n' > vault/c.md && git add -A && git commit -m x",
        "cat > vault/c.md <<'EOF'\n# C\nnew\nEOF\ngit add -A && git commit -m x",
        "cat <<'EOF' > vault/c.md\n# C\nEOF\ngit add -A && git commit -m x",
        "echo x >> vault/a.md; git commit -am x",
        "echo x | tee vault/c.md && git add -A && git commit -m x",
        "sed -i s/a/b/ vault/a.md && git commit -am x",
        "cp wikiknit.toml vault/c.md && git add -A && git commit -m x",
        "mv vault/a.md vault/z.md && git add -A && git commit -m x",
        "python3 -c 'open(\"vault/c.md\", \"w\").write(\"x\")' && git add -A && git commit -m x",
        "./gen.sh && git add -A && git commit -m x",
        "cd vault && echo x > c.md && cd .. && git add -A && git commit -m x",
        "echo x > \"$OUT\" && git commit -am x",
        "git restore vault/a.md && git commit -am x",
        "git stash pop && git commit -am x",
        "dd if=/dev/zero of=vault/c.md count=1 && git commit -am x",
        "> vault/c.md; git add -A; git commit -m x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}");
        assert!(
            stderr(&out).contains("separate command"),
            "{command}: {}",
            stderr(&out)
        );
    }
    let outside = s.other().join("log.txt");
    for command in [
        "git commit -m x 2>/dev/null".to_string(),
        "git commit -m x >&2".to_string(),
        "git commit -m x > /dev/null 2>&1".to_string(),
        "git commit -m x &>/dev/null".to_string(),
        format!(
            "git log --oneline > {} && git commit -m x",
            outside.display()
        ),
        format!(
            "cat > {} <<'EOF'\nmsg\nEOF\ngit commit -F {0}",
            outside.display()
        ),
        "git add -A && git commit -m \"$(cat <<'EOF'\nmsg\n\nbody\nEOF\n)\"".to_string(),
        "wikiknit check && git status && git commit -m x".to_string(),
        "git commit -m x && git push".to_string(),
        "git commit -F - <<'EOF'\nmsg\nEOF".to_string(),
        "git status --short && git diff --stat | cat && git log --oneline -3".to_string(),
        format!(
            "git -C {} add vault && git -C {0} commit -F {}",
            s.repo().display(),
            outside.display()
        ),
        // Writes with no commit are not held.
        "printf 'x' > vault/c.md".to_string(),
        "./gen.sh".to_string(),
    ] {
        let out = s.pre_commit(&command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
}

/// H2: an error while checking a commit (here: an unknown key in `wikiknit.toml`) must hold
/// the commit, not exit 1 (which Claude Code treats as "go ahead"). Other commands still pass.
#[test]
fn errors_hold_commits_but_not_other_commands() {
    let s = Setup::new("vault = \"vault\"\nbogus_key = 1\n");
    s.edit();
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("bogus_key"), "{}", stderr(&out));
    for command in ["git status", "ls", "echo git commit"] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "{command}: {}", stderr(&out));
    }
    // A broken config in an unrelated folder does not hold a commit elsewhere.
    let other = s.other();
    git(&other, &["init", "-q"]);
    let out = s.pre_commit_in(&other, Some(&s.repo()), "git commit -m x", &[]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// H2: running out of time while checking a commit holds it.
#[test]
fn a_timeout_holds_a_commit() {
    let s = Setup::standard();
    s.session_start("s1");
    let repo = s.repo();
    let slow = [("WIKIKNIT_HOOK_TIMEOUT_MS", "0")];
    let out = s.pre_commit_in(&repo, Some(&repo), "git commit -m x", &slow);
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("time"), "{}", stderr(&out));
    // The parse runs under the deadline too (with no time at all, even `git status` may be
    // held: whether its parse finishes first is a race); with time to parse it passes.
    let enough = [("WIKIKNIT_HOOK_TIMEOUT_MS", "5000")];
    let out = s.pre_commit_in(&repo, Some(&repo), "git status", &enough);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// M1: commits the parser used to miss.
#[test]
fn hidden_commits_are_held() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    for command in [
        "bash <<< 'git commit -m x'",
        "echo 'git commit -m x' | bash",
        "printf 'git add -A\\ngit commit -m x\\n' | sh",
        "curl -s https://example.invalid/x.sh | bash",
        "source <(echo git commit -m x)",
        ". <(echo git commit -m x)",
        "2>/dev/null git commit -m x",
        "time -p git commit -m x",
        "find . -maxdepth 0 -exec git commit -m x \\;",
        "find . -maxdepth 0 -execdir git commit -m x {} +",
        "$(which git) commit -m x",
        "`which git` commit -m x",
        "G=git; $G commit -m x",
        "git commit-tree HEAD^{tree} -m x",
        "git rebase --continue",
        "git cherry-pick --continue",
        "git merge --continue",
        "git revert --continue",
        "git am --continue",
        "git -c alias.rc='rebase --continue' rc",
        "command git commit -m x",
        "exec -a x git commit -m x",
        "(export GIT_DIR=/nowhere/.git); git commit -m x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    // From another repo, aimed at the vault's repo.
    let other = s.other();
    git(&other, &["init", "-q"]);
    for command in [
        "GIT_DIR=../repo/.git GIT_WORK_TREE=../repo git commit -m x",
        "export GIT_DIR=../repo/.git; git commit -m x",
        "git --git-dir=../repo/.git --work-tree=../repo commit -m x",
        "env -C ../repo git commit -m x",
        "env --chdir=../repo git commit -m x",
        "sudo -D ../repo git commit -m x",
    ] {
        let out = s.pre_commit_in(&other, Some(&s.repo()), command, &[]);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    for command in [
        "command -v git",
        "echo 'git commit -m x' | cat",
        "echo git commit",
        "git commit-graph write",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
    // A commit in the other repo stays allowed.
    let out = s.pre_commit_in(&other, Some(&s.repo()), "git commit -m x", &[]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// M2: the config is found from where the commit runs, not only the session's folders.
#[test]
fn the_config_is_found_from_the_commit_target() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    let other = s.other();
    let repo = s.repo();
    for command in [
        format!("git -C {} commit -am x", repo.display()),
        format!("cd {} && git commit -am x", repo.display()),
        format!("cd {}/vault && git commit -am x", repo.display()),
    ] {
        let out = s.pre_commit_in(&other, None, &command, &[]);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    git(&other, &["init", "-q"]);
    let out = s.pre_commit_in(&other, None, "git commit -m x", &[]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// L9: commands that cannot commit are not held.
#[test]
fn non_committing_commands_are_not_held() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    git(&s.repo(), &["config", "alias.lg", "log --oneline"]);
    let bin = TempDir::new().unwrap();
    let lfs = bin.path().join("git-lfs");
    fs::write(&lfs, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&lfs, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").unwrap()
    );
    let env = [("PATH", path.as_str())];
    let repo = s.repo();
    for command in [
        "git lfs status",
        "git lfs ls-files",
        "GIT_PAGER=cat git lg",
        "GIT_EDITOR=vim git lg -3",
    ] {
        let out = s.pre_commit_in(&repo, Some(&repo), command, &env);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
    for command in [
        "git lfs migrate import --everything",
        "GIT_CONFIG_GLOBAL=/tmp/x git lg",
        "GIT_EDITOR=true git commit -m x",
    ] {
        let out = s.pre_commit_in(&repo, Some(&repo), command, &env);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
}

/// L10: losing the state (or a new `XDG_STATE_HOME`) must not count changed notes as linked.
#[test]
fn lost_state_fails_closed() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    // A fresh state root: the gate still sees the note that differs from git HEAD.
    let fresh = s.root.path().join("fresh-state");
    let repo = s.repo();
    let env = [("XDG_STATE_HOME", fresh.to_str().unwrap())];
    let out = s.pre_commit_in(&repo, Some(&repo), "git commit -am x", &env);
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("vault/a") || stderr(&out).contains("a\n"));
    assert!(stderr(&out).contains("wikiknit mark"), "{}", stderr(&out));
    // Only linked.json lost, the session record kept.
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2));
    for e in fs::read_dir(s.state().join("wikiknit")).unwrap() {
        let _ = fs::remove_file(e.unwrap().path().join("linked.json"));
    }
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    // The way out: a link pass, then the commit goes through.
    s.mark("s1");
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// L10: first use, no state at all and no session start: notes that differ from git HEAD are
/// held until a link pass; a clean vault is not.
#[test]
fn first_use_holds_uncommitted_notes_until_marked() {
    let s = Setup::standard();
    let out = s.pre_commit("git commit -m x");
    assert_eq!(code(&out), Some(0), "clean vault held: {}", stderr(&out));
    let s = Setup::standard();
    s.edit();
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("wikiknit mark --session s1"));
    s.mark("s1");
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// Security: the hook runs git in folders named by the command (a cloned repo). Repo config
/// that runs programs (fsmonitor, clean filters, external diff) must not run.
#[cfg(unix)]
#[test]
fn repo_config_runs_no_programs() {
    use std::os::unix::fs::PermissionsExt as _;
    let s = Setup::standard();
    let repo = s.repo();
    let marker = s.root.path().join("ran");
    let tool = s.root.path().join("tool.sh");
    fs::write(
        &tool,
        format!("#!/bin/sh\necho x >> {}\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = fs::read_to_string(repo.join(".git/config")).unwrap();
    config.push_str(&format!(
        "[core]\n\tfsmonitor = {t}\n\thooksPath = {h}\n[filter \"evil\"]\n\tclean = {t}\n\tsmudge = cat\n\
         [diff]\n\texternal = {t}\n[diff \"evil\"]\n\ttextconv = {t}\n",
        t = tool.display(),
        h = s.root.path().display()
    ));
    fs::write(repo.join(".git/config"), config).unwrap();
    fs::write(repo.join(".gitattributes"), "*.md filter=evil diff=evil\n").unwrap();
    s.edit();
    // No session start and no state: the link record is rebuilt from git.
    for command in [
        "git commit -am x",
        "git ci -m x",
        "git status",
        "git -C vault commit",
    ] {
        let out = s.pre_commit(command);
        assert!(
            !marker.exists(),
            "{command} ran repo config programs: {}",
            stderr(&out)
        );
    }
    assert_eq!(code(&s.pre_commit("git commit -am x")), Some(2));
}

/// Item 3: a linked worktree of the vault's repo has its own copy of the vault; a commit
/// there is checked against that copy, even when its `wikiknit.toml` is not tracked.
#[test]
fn linked_worktrees_check_their_own_copy_of_the_vault() {
    let s = Setup::standard();
    let repo = s.repo();
    git(&repo, &["rm", "-q", "--cached", "wikiknit.toml"]);
    git(&repo, &["commit", "-q", "-m", "untrack config"]);
    let wt = s.root.path().join("wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    s.session_start("s1");
    fs::write(wt.join("vault/a.md"), "# a\nedited in wt [[b]]\n").unwrap();
    let out = s.pre_commit_in(&wt, Some(&repo), "git commit -am x", &[]);
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    let out = s.pre_commit_in(
        &repo,
        Some(&repo),
        &format!("git -C {} commit -am x", wt.display()),
        &[],
    );
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    // The main checkout is clean: its commits pass.
    let out = s.pre_commit("git commit -m x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// Item 2: which git commands can commit pending notes. merge, cherry-pick, revert, am,
/// rebase, and pull refuse a dirty index and leave unstaged files alone (checked against
/// git), so only their `--continue` commits the index; `stash` writes `refs/stash`, not
/// history. Commands from git's exec path (subtree, citool, ...) may commit.
#[test]
fn commands_that_can_commit_pending_notes() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    let exec_path = std::process::Command::new("git")
        .arg("--exec-path")
        .output()
        .unwrap();
    let exec_path = PathBuf::from(String::from_utf8_lossy(&exec_path.stdout).trim());
    let mut held = vec![
        "git am --resolved",
        "git am -r",
        "git merge --continue",
        "git -c alias.mc='merge --continue' mc",
    ];
    if exec_path.join("git-subtree").is_file() {
        held.push("git subtree add --prefix=x HEAD");
    }
    if exec_path.join("git-citool").is_file() {
        held.push("git citool");
    }
    for command in held {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    for command in [
        "git merge other",
        "git cherry-pick HEAD",
        "git revert HEAD",
        "git am x.patch",
        "git rebase main",
        "git pull",
        "git stash",
        "git cherry-pick -n HEAD",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
}

/// Items 4-6: parser differentials.
#[test]
fn substitutions_program_words_and_env_options() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    for command in [
        // A substitution must not shift pipe and heredoc bookkeeping.
        "echo 'git commit -m x' | x=$(true) bash",
        "cat <<'EOF' | x=$(true) sh\ngit commit -m x\nEOF",
        "bash -s <<'EOF' $(true)\ngit commit -m x\nEOF",
        "bash -s x <<'EOF'\ngit commit -m x\nEOF",
        // Any program word that is not a literal may be git.
        "G=git; $G st",
        "\"$(which git)\" st",
        "`which git` ci",
        "bash $S",
        // env options.
        "env -S'git commit -m x'",
        "env -iS 'git commit -m x'",
        "env -vS'git commit -m x'",
        "env --split-string='git commit -m x'",
        "env --bogus git status",
        "env -q git status",
        // Second review round.
        "env -S '${G} commit'",
        "env -S 'git commit; true'",
        "( echo 'git commit -m x' ) 2>/dev/null | bash",
        "echo 'git commit -m x' | { ( bash ) ; }",
        "echo 'git commit -m x' | echo $(bash)",
        "printf '%s' \"$X\" | bash",
        "{git,x} commit -m x",
        "git $CMD",
        "echo commit | xargs git",
        "alias g=git\ng commit -m x",
        "trap 'git commit -m x' EXIT",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    for command in [
        "env -i git status",
        "env -u X -C . git status",
        "x=$(true) git status",
        "[ -f x ] && git status",
        "ls | xargs grep x",
        "git ls-files | xargs -I{} git log -1 -- {}",
        "git log -- '*.md'",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
}

/// Final review 1: Claude Code's standard commit form, a heredoc inside `"$( ... )"`, must
/// pass when nothing is pending, whatever the message says (`1)`, `:)`, `it's`, backticks).
#[test]
fn heredoc_commit_message_in_double_quotes_is_not_a_write() {
    let s = Setup::standard();
    s.session_start("s1");
    s.mark("s1");
    let command = "git add -A && git commit -m \"$(cat <<'EOF'\nSteps:\n1) ran tests\n2) ran \
                   `cargo test` (it's green)\nDone :)\n\nCo-Authored-By: x <x@y>\nEOF\n)\"";
    let out = s.pre_commit(command);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    s.edit();
    let out = s.pre_commit(command);
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
}

/// Final review 2: a very long pipeline must not crash the hook (a crash lets the commit
/// through) and must stay fast.
#[test]
fn long_pipelines_are_held_not_crashed() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    let pipeline = vec!["cat"; 4000].join(" | ");
    let start = std::time::Instant::now();
    let out = s.pre_commit(&format!("{pipeline} ; git commit -m x"));
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    let out = s.pre_commit(&pipeline);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

/// Final review 3: `cd` that may not change this shell's folder (background, pipeline,
/// conditional, `cd -`) must not move later commits elsewhere.
#[test]
fn cd_that_may_not_run_does_not_move_the_commit() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    git(&s.other(), &["init", "-q"]);
    for command in [
        "cd ../other & git commit -am x",
        "cd ../other | cat; git commit -am x",
        "false && cd ../other; git commit -am x",
        "cd ../other || echo no; git commit -am x",
        "if false; then cd ../other; fi; git commit -am x",
        "for d in x; do cd ../other; done; git commit -am x",
        "cd ../other; cd -; git commit -am x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
    for command in [
        "cd ../other && git commit -m x",
        "cd ../other || exit 1; git commit -m x",
        "cd ../other; git commit -m x",
        "true && cd ../other && git commit -m x",
        "(cd ../other; cd -); cd ../other && git commit -m x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
}

/// Final review 4: commands that cannot change the notes a commit takes are not held.
#[test]
fn writes_that_cannot_reach_the_commit_are_not_held() {
    let s = Setup::standard();
    s.session_start("s1");
    s.mark("s1");
    for command in [
        "git checkout -b feat && git add -A && git commit -m x",
        "git switch -c feat2 && git commit -m x",
        "git checkout --orphan o && git commit -m x",
        "git commit -m x && gh pr create --fill",
        "git commit -m x && echo done > vault/log.md",
        "mkdir -p vault/Sessions/2026/10 && git commit -m x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
    for command in [
        "git checkout -b feat origin/x && git commit -m x",
        "git switch -c feat main && git commit -m x",
        "for i in 1 2; do git commit -m x; echo y > vault/c.md; done",
        "git commit -m x & echo y > vault/c.md",
        "echo y > vault/c.md & git commit -am x",
        "git commit -am x | tee vault/c.md",
        "git commit -m x; git commit -am y; echo z > vault/c.md; git commit -am z",
        "bash <<'EOF'\necho y > vault/c.md\nEOF\ngit commit -am x",
    ] {
        let out = s.pre_commit(command);
        assert_eq!(code(&out), Some(2), "not held: {command}: {}", stderr(&out));
    }
}

/// Final review 5a: a vault git ignores is not in git: its notes are not all pending.
#[test]
fn an_ignored_vault_is_outside_git() {
    let s = Setup::new("vault = \"notes\"\n");
    let repo = s.repo();
    fs::create_dir_all(repo.join("notes")).unwrap();
    fs::write(repo.join("notes/n.md"), "# n\n").unwrap();
    fs::write(repo.join(".gitignore"), "notes/\n").unwrap();
    let out = s.pre_commit("git add -A && git commit -m x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// Security review: an ignored vault folder whose notes were added with `git add -f` is
/// tracked, so its changed notes are gated; so is a force-added note in a tracked vault.
#[test]
fn force_added_notes_in_an_ignored_vault_are_gated() {
    let s = Setup::new("vault = \"notes\"\n");
    let repo = s.repo();
    fs::create_dir_all(repo.join("notes")).unwrap();
    fs::write(repo.join("notes/n.md"), "# n\n[[m]]\n").unwrap();
    fs::write(repo.join("notes/m.md"), "# m\n[[n]]\n").unwrap();
    fs::write(repo.join(".gitignore"), "notes/\n").unwrap();
    git(
        &repo,
        &["add", "-f", "notes/n.md", "notes/m.md", ".gitignore"],
    );
    git(&repo, &["commit", "-q", "-m", "notes"]);
    fs::write(repo.join("notes/n.md"), "# n\nedited [[m]]\n").unwrap();
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    // Force-adding an ignored folder in the same command as the commit is gated too.
    let s = Setup::new("vault = \"notes\"\n");
    let repo = s.repo();
    fs::create_dir_all(repo.join("notes")).unwrap();
    fs::write(repo.join(".gitignore"), "notes/\n").unwrap();
    s.session_start("s1");
    fs::write(repo.join("notes/n.md"), "# n\nnew\n").unwrap();
    let out = s.pre_commit("git add -f notes && git commit -m x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    let out = s.pre_commit("git add -A && git commit -m x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
    // A new ignored note that is staged with -f is pending too.
    let s = Setup::standard();
    let repo = s.repo();
    fs::write(repo.join(".gitignore"), "vault/secret.md\n").unwrap();
    fs::write(repo.join("vault/secret.md"), "# secret\n[[a]]\n").unwrap();
    git(&repo, &["add", "-f", "vault/secret.md"]);
    let out = s.pre_commit("git commit -m x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
}

/// Final review 5b: after a lost-state rebuild, a note put back to its committed content is
/// not pending any more.
#[test]
fn a_reverted_note_is_not_pending_after_a_rebuild() {
    let s = Setup::standard();
    s.edit();
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    fs::write(s.repo().join("vault/a.md"), "# a\n[[b]]\n").unwrap();
    let out = s.pre_commit("git commit -am x");
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// Final review 8: a hold for a worktree's copy of the vault names that copy in the
/// `wikiknit mark` it asks for, and that command clears the hold.
#[test]
fn a_worktree_hold_names_its_vault() {
    let s = Setup::standard();
    let repo = s.repo();
    git(&repo, &["rm", "-q", "--cached", "wikiknit.toml"]);
    git(&repo, &["commit", "-q", "-m", "untrack config"]);
    let wt = s.root.path().join("wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    s.session_start("s1");
    fs::write(wt.join("vault/a.md"), "# a\nedited in wt [[b]]\n").unwrap();
    let out = s.pre_commit_in(&wt, Some(&repo), "git commit -am x", &[]);
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    let copy = wt.join("vault").canonicalize().unwrap();
    let wanted = format!("wikiknit --vault {} mark --session s1", copy.display());
    assert!(stderr(&out).contains(&wanted), "{}", stderr(&out));
    s.wk(&repo)
        .args(["--vault", copy.to_str().unwrap(), "mark", "--session", "s1"])
        .assert()
        .success();
    let out = s.pre_commit_in(&wt, Some(&repo), "git commit -am x", &[]);
    assert_eq!(code(&out), Some(0), "{}", stderr(&out));
}

/// Final review 11: `cd "$HOME/..."` is followed; a variable target is unknown (its value
/// cannot be known for certain), and the hold says so.
#[test]
fn cd_targets_with_home_and_variables() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    git(&s.other(), &["init", "-q"]);
    let repo = s.repo();
    let home = s.root.path().to_str().unwrap().to_string();
    let env = [("HOME", home.as_str())];
    for command in [
        "cd \"$HOME/other\" && git commit -m x",
        "cd ${HOME}/other && git commit -m x",
        "cd ~/other && git commit -m x",
    ] {
        let out = s.pre_commit_in(&repo, Some(&repo), command, &env);
        assert_eq!(code(&out), Some(0), "held: {command}: {}", stderr(&out));
    }
    for command in [
        "cd $UNSET && git commit -am x",
        "R=../other; cd $R && git commit -m x",
    ] {
        let out = s.pre_commit_in(&repo, Some(&repo), command, &env);
        assert_eq!(code(&out), Some(2), "{command}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("cannot tell which repository"),
            "{}",
            stderr(&out)
        );
    }
}

/// Final review 13: `mark --session` with a session that does not exist refuses, instead of
/// starting it and marking every pending note.
#[test]
fn mark_refuses_an_unknown_session() {
    let s = Setup::standard();
    s.session_start("s1");
    s.edit();
    let out = s
        .wk(&s.repo())
        .args(["mark", "--session", "typo"])
        .output()
        .unwrap();
    assert_eq!(code(&out), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("s1"), "{}", stderr(&out));
    let out = s.pre_commit("git commit -am x");
    assert_eq!(
        code(&out),
        Some(2),
        "a typo marked the notes: {}",
        stderr(&out)
    );
}
