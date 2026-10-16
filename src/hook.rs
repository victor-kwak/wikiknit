//! Claude Code hooks: JSON on stdin, exit code 2 + stderr to hand Claude a reason.
//!
//! - `session-start`: record the session baseline; export `WIKIKNIT_SESSION` via `CLAUDE_ENV_FILE`.
//! - `pre-commit` (PreToolUse on Bash): hold a `git commit` while changed notes lack a link pass.
//! - `stop`: backup nudge once a change set has sat untouched for a whole turn.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::state::{self, Baseline};
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
        Event::SessionStart => session_start(config, session),
        Event::PreCommit => {
            let command = input
                .tool_input
                .as_ref()
                .and_then(|t| t.get("command"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let commits = shell::git_commits(command);
            if commits.is_empty() {
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
            let touches_vault = commits.iter().any(|c| {
                let dir = c
                    .dirs
                    .iter()
                    .fold(base_dir.clone(), |acc, d| expand_dir(&acc, d));
                // Unknown repo (dir missing, not a repo): hold to be safe.
                git_toplevel(&dir).is_none_or(|top| top == vault_repo)
            });
            if !touches_vault {
                return Ok(Outcome::Allow);
            }
            let vault = Vault::load(config)?;
            let Some(base) = load_or_create(&vault, session)? else {
                return Ok(Outcome::Allow);
            };
            let changed = base.changed(&vault);
            if changed.is_empty() {
                return Ok(Outcome::Allow);
            }
            Ok(Outcome::Block(format!(
                "Commit held by wikiknit. {}",
                describe(&vault, &changed, session)
            )))
        }
        Event::Stop => {
            if input.stop_hook_active {
                return Ok(Outcome::Allow);
            }
            let vault = Vault::load(config)?;
            let Some(mut base) = load_or_create(&vault, session)? else {
                return Ok(Outcome::Allow);
            };
            let digest = base.change_digest(&vault);
            let outcome = match (&digest, &base.last_stop_digest) {
                (None, _) => {
                    base.last_stop_digest = None;
                    Outcome::Allow
                }
                (Some(d), Some(last)) if last == d => {
                    // Settled for a whole turn: nudge once.
                    base.last_stop_digest = Some(format!("nudged:{d}"));
                    Outcome::Block(format!(
                        "wikiknit: these vault notes have not changed for a full turn. {}",
                        describe(&vault, &base.changed(&vault), session)
                    ))
                }
                (Some(d), Some(last)) if last.strip_prefix("nudged:") == Some(d.as_str()) => {
                    Outcome::Allow
                }
                (Some(d), _) => {
                    base.last_stop_digest = Some(d.clone());
                    Outcome::Allow
                }
            };
            base.save(session)?;
            Ok(outcome)
        }
    }
}

fn session_start(config: Config, session: &str) -> Result<Outcome, Error> {
    let root = config.vault.clone();
    state::prune(&root);
    if Baseline::load(&root, session)?.is_none() {
        let vault = Vault::load(config)?;
        Baseline::snapshot(&vault).save(session)?;
    }
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

/// `git rev-parse --show-toplevel` for `dir`, canonicalized.
fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let top = String::from_utf8(out.stdout).ok()?;
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

/// The session baseline; created now (and `None` returned) if the session has none,
/// since then there is no record of what changed.
fn load_or_create(vault: &Vault, session: &str) -> Result<Option<Baseline>, Error> {
    if let Some(b) = Baseline::load(&vault.config.vault, session)? {
        return Ok(Some(b));
    }
    Baseline::snapshot(vault).save(session)?;
    Ok(None)
}

fn describe(vault: &Vault, changed: &BTreeSet<String>, session: &str) -> String {
    let mut msg = format!(
        "{} vault note(s) changed in this session and have had no link pass:\n",
        changed.len()
    );
    for id in changed.iter().take(MAX_LISTED) {
        msg.push_str(&format!("  - {id}\n"));
    }
    if changed.len() > MAX_LISTED {
        msg.push_str(&format!(
            "  - ... and {} more\n",
            changed.len() - MAX_LISTED
        ));
    }
    let errors = rules::check(vault, Some(changed))
        .into_iter()
        .filter(rules::Finding::is_error)
        .count();
    if errors > 0 {
        msg.push_str(&format!(
            "`wikiknit check --changed` reports {errors} rule error(s) in them.\n"
        ));
    }
    msg.push_str(&format!(
        "The wikiknit `link` skill handles this: it links these notes to related notes in both \
         directions, then records the pass with `wikiknit mark --session {session}` (run it as \
         its own command, before the commit)."
    ));
    msg
}
