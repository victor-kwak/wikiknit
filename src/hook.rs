//! Claude Code hooks: JSON on stdin, exit code 2 + stderr to hand Claude a reason.
//!
//! - `session-start`: record the session start; export `WIKIKNIT_SESSION` via `CLAUDE_ENV_FILE`.
//! - `pre-commit` (PreToolUse on Bash): hold a `git commit` in the vault's repo while notes
//!   changed in this session lack a link pass.
//! - `stop`: backup nudge once a change set has sat untouched for a whole turn.

use std::collections::BTreeSet;
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
    let Some(config_path) = find_config(input) else {
        return Ok(Outcome::Allow);
    };
    let config = Config::load(&config_path)?;
    let session = input.session_id.as_str();
    match event {
        Event::SessionStart => {
            session_start(config, session, input.source.as_deref() == Some("clear"))
        }
        Event::PreCommit => pre_commit(config, session, input),
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

fn pre_commit(config: Config, session: &str, input: &Input) -> Result<Outcome, Error> {
    let command = input
        .tool_input
        .as_ref()
        .and_then(|t| t.get("command"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let calls = shell::git_calls(command);
    if calls.is_empty() {
        return Ok(Outcome::Allow);
    }
    // Only commits in the repo that holds the vault can carry vault changes.
    let Some(vault_repo) = git_toplevel(&config.vault) else {
        return Ok(Outcome::Allow);
    };
    let base_dir = input
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let touches_vault = calls.iter().any(|c| {
        let dir = c
            .dirs
            .iter()
            .fold(base_dir.clone(), |acc, d| expand_dir(&acc, d));
        let alias_dir = if dir.is_dir() { &dir } else { &base_dir };
        if !is_commit(c, &c.subcommand, alias_dir, 0) {
            return false;
        }
        // Unknown repo (dir missing, not a repo): hold to be safe.
        git_toplevel(&dir).is_none_or(|top| top == vault_repo)
    });
    if !touches_vault {
        return Ok(Outcome::Allow);
    }
    let vault = Vault::load(config)?;
    let store = Store::open(&vault.config.vault)?;
    let now = state::current_hashes(&vault);
    // No record of this session's start (plugin added mid-session, state lost): start it with
    // every pending note counted as its own, so the gate fails closed.
    let s = store.start_session(session, &now, true)?;
    let changed = store.changed(&s, &now)?;
    if changed.is_empty() {
        return Ok(Outcome::Allow);
    }
    Ok(Outcome::Block(format!(
        "Commit held by wikiknit. {}",
        describe(&vault, &changed, session)
    )))
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

/// `commit`, or something that may run it: an alias (from `-c alias.*`, or git config),
/// or an external `git-<name>` command. Fails closed when it cannot tell (config from the
/// environment, an external command).
fn is_commit(call: &shell::GitCall, subcommand: &str, dir: &Path, depth: usize) -> bool {
    if subcommand == "commit" {
        return true;
    }
    if depth > 3 || NOT_COMMIT.contains(&subcommand) || is_builtin(subcommand) {
        return false;
    }
    let alias = call
        .inline_aliases
        .iter()
        // The last definition wins, as in git.
        .rfind(|(name, _)| name.eq_ignore_ascii_case(subcommand))
        .map(|(_, v)| v.clone())
        .or_else(|| {
            if call.config_overridden {
                None
            } else {
                git_output(dir, &["config", "--get", &format!("alias.{subcommand}")])
            }
        });
    let Some(alias) = alias else {
        // No alias we can see: hold if config or command lookup is overridden (we cannot
        // know what runs), or if an external `git-<name>` program would run (it may commit).
        return call.config_overridden || external_git_command(subcommand);
    };
    let alias = alias.trim();
    if let Some(script) = alias.strip_prefix('!') {
        return shell::git_calls(script)
            .iter()
            .any(|c| is_commit(c, &c.subcommand, dir, depth + 1));
    }
    alias
        .split_whitespace()
        .next()
        .is_some_and(|first| is_commit(call, first, dir, depth + 1))
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

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
}

/// `git rev-parse --show-toplevel` for `dir`, canonicalized.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let top = git_output(dir, &["rev-parse", "--show-toplevel"])?;
    PathBuf::from(top.trim()).canonicalize().ok()
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
