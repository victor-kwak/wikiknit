//! Claude Code hooks: JSON on stdin, exit code 2 + stderr to hand Claude a reason.
//!
//! - `session-start`: record the session start; export `WIKIKNIT_SESSION` via `CLAUDE_ENV_FILE`.
//! - `pre-commit` (PreToolUse on Bash): hold a `git commit` in the vault's repo while notes
//!   changed in this session lack a link pass, or when the same command may also write vault
//!   files (the vault is read before the command runs). Once a command is known to commit,
//!   any failure holds it (exit 2): exit 1 would let the commit through.
//! - `stop`: backup nudge once a change set has sat untouched for a whole turn.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::state::{self, Store};
use crate::vault::Vault;
use crate::{Error, rules, shell};

const MAX_LISTED: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Event {
    SessionStart,
    PreCommit,
    Stop,
}

#[derive(Debug, Deserialize)]
pub struct Input {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub stop_hook_active: bool,
    #[serde(default)]
    pub tool_input: Option<Value>,
    /// SessionStart: `startup`, `resume`, `clear`, or `compact`.
    #[serde(default)]
    pub source: Option<String>,
}

/// What the hook process should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    /// Exit 2 with this message on stderr.
    Block(String),
}

fn find_config(input: &Input) -> Option<PathBuf> {
    let mut starts = Vec::new();
    if let Some(cwd) = &input.cwd {
        starts.push(cwd.clone());
    }
    if let Some(dir) = std::env::var_os("CLAUDE_PROJECT_DIR") {
        starts.push(PathBuf::from(dir));
    }
    starts.iter().find_map(|s| Config::find(s))
}

pub fn run(event: Event, input: &Input) -> Result<Outcome, Error> {
    if event == Event::PreCommit {
        // Finds its configs from where each commit runs; never fails open.
        return Ok(pre_commit(input));
    }
    let Some(config_path) = find_config(input) else {
        return Ok(Outcome::Allow);
    };
    let config = Config::load(&config_path)?;
    let session = input.session_id.as_str();
    match event {
        Event::SessionStart => {
            session_start(config, session, input.source.as_deref() == Some("clear"))
        }
        Event::PreCommit => unreachable!("handled above"),
        Event::Stop => {
            if input.stop_hook_active {
                return Ok(Outcome::Allow);
            }
            stop(config, session)
        }
    }
}

fn session_start(config: Config, session: &str, after_clear: bool) -> Result<Outcome, Error> {
    let store = Store::open(&config.vault)?;
    store.prune();
    let vault = Vault::load(config)?;
    store.start_session(session, &state::current_hashes(&vault), after_clear)?;
    if let Some(env_file) = std::env::var_os("CLAUDE_ENV_FILE").filter(|f| !f.is_empty()) {
        let env_file = Path::new(&env_file);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(env_file)
            .map_err(|e| Error::io(env_file, e))?;
        writeln!(
            f,
            "export {}='{}'",
            state::SESSION_ENV,
            session.replace('\'', "")
        )
        .map_err(|e| Error::io(env_file, e))?;
    }
    Ok(Outcome::Allow)
}

/// The command of a PreToolUse Bash input.
fn command_of(input: &Input) -> &str {
    input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// From the command text alone (no git, no files): could it commit? Used when the hook runs
/// out of time, so it errs toward yes.
#[must_use]
pub fn may_commit(input: &Input) -> bool {
    shell::analyze(command_of(input)).calls.iter().any(|c| {
        c.opaque
            || commit_like(&c.subcommand, &c.args)
            || !NOT_COMMIT.contains(&c.subcommand.as_str())
    })
}

/// Where one git call runs.
struct Target {
    /// The directory git starts in (`cd`, `env -C`, `-C` applied).
    dir: PathBuf,
    git_dir: Option<PathBuf>,
    work_tree: Option<PathBuf>,
    /// Cannot be known (`curl ... | sh`).
    opaque: bool,
}

impl Target {
    fn of(call: &shell::GitCall, base: &Path) -> Self {
        let dir = call
            .dirs
            .iter()
            .fold(base.to_path_buf(), |acc, d| expand_dir(&acc, d));
        Self {
            git_dir: call.git_dir.as_deref().map(|g| expand_dir(&dir, g)),
            work_tree: call.work_tree.as_deref().map(|w| expand_dir(&dir, w)),
            dir,
            opaque: call.opaque,
        }
    }

    /// The repository it commits to: its git directory, canonicalized (`None`: unknown).
    fn repo(&self) -> Option<PathBuf> {
        if self.opaque {
            return None;
        }
        match &self.git_dir {
            Some(g) => {
                let mut args: Vec<&OsStr> = vec![OsStr::new("--git-dir"), g.as_os_str()];
                args.extend(["rev-parse", "--absolute-git-dir"].map(OsStr::new));
                canonical_output(&self.dir, &args)
            }
            None => repo_of_dir(&self.dir),
        }
    }

    /// Folders to look for a `wikiknit.toml` from.
    fn starts(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.dir)
            .chain(self.git_dir.as_ref())
            .chain(self.work_tree.as_ref())
    }
}

/// A commit (or something that may commit) the command runs.
struct Commit {
    target: Target,
    repo: Option<PathBuf>,
}

/// The PreToolUse gate. Cheap until the command is known to commit; after that, any failure
/// holds the commit (exit 1 would let it through).
fn pre_commit(input: &Input) -> Outcome {
    let analysis = shell::analyze(command_of(input));
    if analysis.calls.is_empty() {
        return Outcome::Allow;
    }
    let base = input
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let kinds: Vec<(Target, Kind)> = analysis
        .calls
        .iter()
        .map(|c| {
            let t = Target::of(c, &base);
            let alias_dir = if t.dir.is_dir() { &t.dir } else { &base };
            let k = classify(c, &c.subcommand, &c.args, alias_dir, 0);
            (t, k)
        })
        .collect();
    if !kinds.iter().any(|(_, k)| k.commit) {
        return Outcome::Allow;
    }
    match gate(input, &base, &analysis, kinds) {
        Ok(o) => o,
        Err(e) => Outcome::Block(format!(
            "Commit held by wikiknit: it could not check this commit: {e}\n\
             wikiknit holds a vault commit it cannot check. Fix the problem (`wikiknit check` \
             shows config errors), then commit again."
        )),
    }
}

fn gate(
    input: &Input,
    base: &Path,
    analysis: &shell::Analysis,
    kinds: Vec<(Target, Kind)>,
) -> Result<Outcome, Error> {
    let mut commits = Vec::new();
    let mut writers = Vec::new();
    for ((target, kind), call) in kinds.into_iter().zip(&analysis.calls) {
        if kind.writes {
            writers.push((call.subcommand.clone(), target.repo()));
        }
        if kind.commit {
            let repo = target.repo();
            commits.push(Commit { target, repo });
        }
    }
    // Configs from where each commit runs, and from the session's folders.
    let mut starts: Vec<PathBuf> = commits
        .iter()
        .flat_map(|c| c.target.starts().cloned())
        .collect();
    starts.extend(input.cwd.clone());
    starts.extend(std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from));
    let configs: BTreeSet<PathBuf> = starts
        .iter()
        .filter_map(|s| Config::find(s))
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    let session = input.session_id.as_str();
    for path in configs {
        let config = match Config::load(&path) {
            Ok(c) => c,
            Err(e) => {
                if broken_config_matters(&path, &commits) {
                    return Err(e);
                }
                continue;
            }
        };
        // A vault outside git cannot be committed.
        let Some(vault_repo) = repo_of_dir(&config.vault) else {
            continue;
        };
        let into_vault_repo =
            |repo: &Option<PathBuf>| repo.as_ref().is_none_or(|r| *r == vault_repo);
        if !commits.iter().any(|c| into_vault_repo(&c.repo)) {
            continue;
        }
        let git_write = writers
            .iter()
            .find(|(_, repo)| into_vault_repo(repo))
            .map(|(sub, _)| format!("git {sub}"));
        if let Some(what) = git_write.or_else(|| vault_write(analysis, base, &config.vault)) {
            return Ok(Outcome::Block(format!(
                "Commit held by wikiknit: this command commits in the repository of the vault \
                 {} and may also change files in the vault (`{what}`). wikiknit reads the vault \
                 before a command runs, so it cannot check notes this command writes. Run the \
                 command that writes as one command and the commit as a separate command; the \
                 commit is then checked against the notes as they are.",
                config.vault.display()
            )));
        }
        let vault = Vault::load(config)?;
        let store = Store::open(&vault.config.vault)?;
        let now = state::current_hashes(&vault);
        // No record of this session's start (plugin added mid-session, state lost): start it
        // with every pending note counted as its own, so the gate fails closed.
        let s = store.start_session(session, &now, true)?;
        let changed = store.changed(&s, &now)?;
        if !changed.is_empty() {
            return Ok(Outcome::Block(format!(
                "Commit held by wikiknit. {}",
                describe(&vault, &changed, session)
            )));
        }
    }
    Ok(Outcome::Allow)
}

/// A config that does not load holds a commit unless the commit is provably elsewhere: in a
/// known repository other than the config's, and outside the config's folder.
fn broken_config_matters(path: &Path, commits: &[Commit]) -> bool {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let dir_repo = repo_of_dir(dir);
    commits.iter().any(|c| {
        let Some(repo) = &c.repo else {
            return true;
        };
        dir_repo.as_ref() == Some(repo)
            || c.target
                .dir
                .canonicalize()
                .is_ok_and(|d| d.starts_with(dir))
    })
}

/// The first non-git write that may land in `vault` (or cannot be placed).
fn vault_write(analysis: &shell::Analysis, base: &Path, vault: &Path) -> Option<String> {
    analysis.writes.iter().find_map(|w| match &w.target {
        shell::WriteTarget::Unknown => Some(w.what.clone()),
        shell::WriteTarget::Paths { dirs, paths } => {
            let dir = dirs
                .iter()
                .fold(base.to_path_buf(), |acc, d| expand_dir(&acc, d));
            paths
                .iter()
                .any(|p| {
                    is_dynamic(p) || dir.is_relative() || lands_in(&expand_dir(&dir, p), vault)
                })
                .then(|| w.what.clone())
        }
    })
}

/// A path the shell expands at run time (variables, substitutions, braces).
fn is_dynamic(p: &str) -> bool {
    p.contains(['$', '`']) || (p.contains('{') && (p.contains(',') || p.contains("..")))
}

/// Whether `path` is in `vault` once symlinks in its existing part are resolved.
fn lands_in(path: &Path, vault: &Path) -> bool {
    let mut existing = path;
    let mut rest = Vec::new();
    let resolved = loop {
        if let Ok(c) = existing.canonicalize() {
            break c;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(p), Some(n)) => {
                rest.push(n.to_owned());
                existing = p;
            }
            // `..` at the end, or nothing exists: cannot tell.
            _ => return true,
        }
    };
    let mut full = resolved;
    for part in rest.iter().rev() {
        if part == ".." {
            full.pop();
        } else if part != "." {
            full.push(part);
        }
    }
    full.starts_with(vault)
}

fn stop(config: Config, session: &str) -> Result<Outcome, Error> {
    let vault = Vault::load(config)?;
    let store = Store::open(&vault.config.vault)?;
    let now = state::current_hashes(&vault);
    let mut s = store.start_session(session, &now, true)?;
    let changed = store.changed(&s, &now)?;
    let digest = state::digest(&changed, &now);
    let outcome = match (&digest, &s.last_stop_digest) {
        (None, _) => {
            s.last_stop_digest = None;
            Outcome::Allow
        }
        (Some(d), Some(last)) if last == d => {
            // Settled for a whole turn: nudge once.
            s.last_stop_digest = Some(format!("nudged:{d}"));
            Outcome::Block(format!(
                "wikiknit: these vault notes have not changed for a full turn. {}",
                describe(&vault, &changed, session)
            ))
        }
        (Some(d), Some(last)) if last.strip_prefix("nudged:") == Some(d.as_str()) => Outcome::Allow,
        (Some(d), _) => {
            s.last_stop_digest = Some(d.clone());
            Outcome::Allow
        }
    };
    store.save_session(session, &s)?;
    Ok(outcome)
}

/// Git's own subcommands that are not `commit`; anything else may be an alias.
const NOT_COMMIT: &[&str] = &[
    "add",
    "am",
    "apply",
    "archive",
    "bisect",
    "blame",
    "branch",
    "bundle",
    "checkout",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "config",
    "describe",
    "diff",
    "fetch",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "help",
    "init",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "merge",
    "mv",
    "notes",
    "pull",
    "push",
    "range-diff",
    "rebase",
    "reflog",
    "remote",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "shortlog",
    "show",
    "sparse-checkout",
    "stash",
    "status",
    "submodule",
    "switch",
    "tag",
    "version",
    "worktree",
];

/// What a git call may do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Kind {
    /// Creates a commit from the index or work tree.
    commit: bool,
    /// Changes files in the work tree.
    writes: bool,
}

impl Kind {
    const ANYTHING: Self = Self {
        commit: true,
        writes: true,
    };
}

impl std::ops::BitOrAssign for Kind {
    fn bitor_assign(&mut self, o: Self) {
        self.commit |= o.commit;
        self.writes |= o.writes;
    }
}

/// Built-in subcommands that create a commit from the index: `commit`, `commit-tree`, and
/// the `--continue` of a rebase, cherry-pick, merge, revert, or am (also `rebase --exec`,
/// which runs commands that may commit).
fn commit_like(sub: &str, args: &[String]) -> bool {
    let has = |o: &str| args.iter().any(|a| a == o);
    match sub {
        "commit" | "commit-tree" => true,
        "rebase" => {
            has("--continue")
                || has("-x")
                || args
                    .iter()
                    .any(|a| a == "--exec" || a.starts_with("--exec="))
        }
        "cherry-pick" | "merge" | "revert" | "am" => has("--continue"),
        _ => false,
    }
}

/// Built-in subcommands that change files in the work tree.
fn changes_worktree(sub: &str, args: &[String]) -> bool {
    let has = |o: &str| args.iter().any(|a| a == o);
    match sub {
        "checkout" | "switch" | "merge" | "pull" | "rebase" | "cherry-pick" | "revert" | "am"
        | "mv" | "checkout-index" | "sparse-checkout" | "bisect" | "filter-branch" => true,
        "restore" => !(has("--staged") || has("-S")) || has("--worktree") || has("-W"),
        "reset" => has("--hard") || has("--merge") || has("--keep"),
        "stash" => !matches!(args.first().map(String::as_str), Some("list" | "show")),
        "apply" => !["--cached", "--check", "--stat", "--numstat", "--summary"]
            .iter()
            .any(|o| has(o)),
        "rm" => !has("--cached"),
        "clean" => !(has("-n") || has("--dry-run")),
        "read-tree" => has("-u"),
        "submodule" => args.first().is_some_and(|a| a == "update"),
        _ => false,
    }
}

/// What `git <sub> <args>` may do, resolving aliases (from `-c alias.*` or git config) and
/// external `git-<name>` commands. Fails closed when it cannot tell (config from the
/// environment, an unknown external command, aliases nested too deep).
fn classify(call: &shell::GitCall, sub: &str, args: &[String], dir: &Path, depth: usize) -> Kind {
    if call.opaque || depth > 8 {
        return Kind::ANYTHING;
    }
    let kind = Kind {
        commit: commit_like(sub, args),
        writes: changes_worktree(sub, args),
    };
    // Git ignores an alias that has a built-in command's name.
    if kind.commit || kind.writes || NOT_COMMIT.contains(&sub) || is_builtin(sub) {
        return kind;
    }
    let alias = call
        .inline_aliases
        .iter()
        // The last definition wins, as in git.
        .rfind(|(name, _)| name.eq_ignore_ascii_case(sub))
        .map(|(_, v)| v.clone())
        .or_else(|| {
            if call.config_overridden {
                None
            } else {
                git_output(dir, &["config", "--get", &format!("alias.{sub}")])
            }
        });
    let Some(alias) = alias else {
        // No alias we can see: anything if config or command lookup is overridden (we cannot
        // know what runs), else what an external `git-<name>` program may do.
        if call.config_overridden {
            return Kind::ANYTHING;
        }
        return external(sub, args);
    };
    let alias = alias.trim();
    if let Some(script) = alias.strip_prefix('!') {
        // A shell alias: git appends the arguments to the script.
        let mut script = script.to_string();
        for a in args {
            script.push(' ');
            script.push_str(a);
        }
        let inner = shell::analyze(&script);
        let mut kind = Kind {
            commit: false,
            writes: !inner.writes.is_empty(),
        };
        for c in &inner.calls {
            kind |= classify(c, &c.subcommand, &c.args, dir, depth + 1);
        }
        return kind;
    }
    let mut words = alias.split_whitespace().map(str::to_string);
    let Some(first) = words.next() else {
        return Kind::default();
    };
    let rest: Vec<String> = words.chain(args.iter().cloned()).collect();
    classify(call, &first, &rest, dir, depth + 1)
}

/// An external `git-<name>` program: it may do anything, except well-known ones.
fn external(sub: &str, args: &[String]) -> Kind {
    if !external_git_command(sub) {
        return Kind::default(); // git fails: no such command
    }
    match sub {
        // Git LFS: only `migrate` rewrites commits; `pull`/`checkout` write the work tree.
        "lfs" => {
            let first = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str);
            Kind {
                commit: first == Some("migrate"),
                writes: matches!(first, Some("migrate" | "pull" | "checkout")),
            }
        }
        _ => Kind::ANYTHING,
    }
}

/// Git's built-in commands (other than `commit`), from `git --list-cmds=builtins`.
fn is_builtin(subcommand: &str) -> bool {
    static BUILTINS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
        git_output(Path::new("."), &["--list-cmds=builtins"])
            .map(|s| s.lines().map(str::to_string).collect())
            .unwrap_or_default()
    });
    subcommand != "commit" && BUILTINS.iter().any(|b| b == subcommand)
}

/// An executable `git-<name>` on `PATH` (git runs it for `git <name>`).
fn external_git_command(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let file = format!("git-{name}");
    std::env::split_paths(&path).any(|dir| dir.join(&file).is_file())
}

fn git_output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
}

/// A path printed by git, canonicalized.
fn canonical_output<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Option<PathBuf> {
    let out = git_output(dir, args)?;
    PathBuf::from(out.trim_end_matches(['\n', '\r']))
        .canonicalize()
        .ok()
}

/// The git directory of the repository (or worktree) holding `dir`, canonicalized.
fn repo_of_dir(dir: &Path) -> Option<PathBuf> {
    canonical_output(dir, &["rev-parse", "--absolute-git-dir"])
}

/// Apply a `cd`/`-C` argument to `base` (handles `~` and absolute paths).
fn expand_dir(base: &Path, d: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if d == "~" {
        return home.unwrap_or_else(|| base.to_path_buf());
    }
    if let (Some(rest), Some(home)) = (d.strip_prefix("~/"), home) {
        return home.join(rest);
    }
    base.join(d)
}

fn describe(vault: &Vault, changed: &BTreeSet<String>, session: &str) -> String {
    use std::fmt::Write as _;
    let mut msg = format!(
        "{} vault note(s) changed in this session and have had no link pass:\n",
        changed.len()
    );
    for id in changed.iter().take(MAX_LISTED) {
        let _ = writeln!(msg, "  - {id}");
    }
    if changed.len() > MAX_LISTED {
        let _ = writeln!(msg, "  - ... and {} more", changed.len() - MAX_LISTED);
    }
    let errors = rules::check(vault, Some(changed))
        .into_iter()
        .filter(rules::Finding::is_error)
        .count();
    if errors > 0 {
        let _ = writeln!(
            msg,
            "`wikiknit check --changed --session {session}` reports {errors} rule error(s) in them."
        );
    }
    let _ = write!(
        msg,
        "Some may have been changed by another session or editor; any session can do their \
         link pass. The wikiknit `link` skill handles this: it links these notes to related \
         notes in both directions, then records the pass with `wikiknit mark --session \
         {session}` (run it as its own command, before the commit)."
    );
    msg
}
