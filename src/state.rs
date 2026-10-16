//! Per-session baselines: which notes changed since a session started, and which
//! changes already had a link pass (`wikiknit mark`).
//!
//! State lives outside the vault, in `$XDG_STATE_HOME/wikiknit/<vault-hash>/<session>.json`,
//! so it never shows up in git and works for vaults that are not in git at all.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::vault::Vault;

pub const SESSION_ENV: &str = "WIKIKNIT_SESSION";
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub vault: PathBuf,
    pub created_unix: u64,
    /// note id -> content hash at baseline (or at last `mark`).
    pub hashes: BTreeMap<String, String>,
    /// Digest of the unmarked change set at the last Stop; used to tell a
    /// "settled" change set (untouched for a whole turn) from work in progress.
    #[serde(default)]
    pub last_stop_digest: Option<String>,
}

fn state_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("wikiknit");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state/wikiknit");
    }
    std::env::temp_dir().join("wikiknit-state")
}

/// Directory holding all baselines for one vault.
#[must_use]
pub fn vault_dir(vault_root: &Path) -> PathBuf {
    let hash = blake3::hash(vault_root.to_string_lossy().as_bytes()).to_hex();
    state_root().join(&hash.as_str()[..16])
}

fn file_name(session: &str) -> String {
    let safe = !session.is_empty()
        && session.len() <= 128
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if safe {
        format!("{session}.json")
    } else {
        format!(
            "{}.json",
            &blake3::hash(session.as_bytes()).to_hex().as_str()[..32]
        )
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Content hash of every note, by id.
#[must_use]
pub fn current_hashes(vault: &Vault) -> BTreeMap<String, String> {
    vault
        .notes
        .iter()
        .map(|n| {
            (
                n.id.clone(),
                blake3::hash(n.text.as_bytes()).to_hex().to_string(),
            )
        })
        .collect()
}

impl Baseline {
    #[must_use]
    pub fn path(vault_root: &Path, session: &str) -> PathBuf {
        vault_dir(vault_root).join(file_name(session))
    }

    pub fn load(vault_root: &Path, session: &str) -> Result<Option<Self>, Error> {
        let path = Self::path(vault_root, session);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| Error::Config {
                    path,
                    message: format!("corrupt baseline: {e}"),
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(&path, e)),
        }
    }

    /// Snapshot the vault as it is now.
    #[must_use]
    pub fn snapshot(vault: &Vault) -> Self {
        Self {
            vault: vault.config.vault.clone(),
            created_unix: now_unix(),
            hashes: current_hashes(vault),
            last_stop_digest: None,
        }
    }

    pub fn save(&self, session: &str) -> Result<(), Error> {
        let path = Self::path(&self.vault, session);
        let dir = path.parent().expect("state path has a parent");
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self).expect("baseline serializes");
        std::fs::write(&tmp, text).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))
    }

    /// Notes added or edited since the baseline (or since their last mark).
    #[must_use]
    pub fn changed(&self, vault: &Vault) -> BTreeSet<String> {
        current_hashes(vault)
            .into_iter()
            .filter(|(id, h)| self.hashes.get(id) != Some(h))
            .map(|(id, _)| id)
            .collect()
    }

    /// Record the current content of `ids` (all changed notes if `None`) as linked.
    pub fn mark(&mut self, vault: &Vault, ids: Option<&BTreeSet<String>>) -> BTreeSet<String> {
        let changed = self.changed(vault);
        let now = current_hashes(vault);
        let mut marked = BTreeSet::new();
        for id in changed {
            if ids.is_some_and(|s| !s.contains(&id)) {
                continue;
            }
            if let Some(h) = now.get(&id) {
                self.hashes.insert(id.clone(), h.clone());
                marked.insert(id);
            }
        }
        self.last_stop_digest = None;
        marked
    }

    /// Digest of the current unmarked change set (ids + content).
    #[must_use]
    pub fn change_digest(&self, vault: &Vault) -> Option<String> {
        let changed = self.changed(vault);
        if changed.is_empty() {
            return None;
        }
        let now = current_hashes(vault);
        let mut h = blake3::Hasher::new();
        for id in &changed {
            h.update(id.as_bytes());
            h.update(now.get(id).map_or(&b""[..], |s| s.as_bytes()));
        }
        Some(h.finalize().to_hex().to_string())
    }
}

/// Which session a CLI call refers to: the flag, then `$WIKIKNIT_SESSION`,
/// then the most recently updated baseline for this vault.
pub fn resolve_session(vault_root: &Path, flag: Option<&str>) -> Result<String, Error> {
    if let Some(s) = flag {
        return Ok(s.to_string());
    }
    if let Ok(s) = std::env::var(SESSION_ENV)
        && !s.is_empty()
    {
        return Ok(s);
    }
    let dir = vault_dir(vault_root);
    let newest = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(t, _)| *t);
    match newest {
        Some((_, path)) => Ok(path
            .file_stem()
            .expect("file stem")
            .to_string_lossy()
            .into_owned()),
        None => Err(Error::Usage(format!(
            "no session baseline for this vault; run `wikiknit baseline --session <id>` or pass note paths \
             (state dir: {})",
            dir.display()
        ))),
    }
}

/// Delete baselines older than 30 days.
pub fn prune(vault_root: &Path) {
    let dir = vault_dir(vault_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff = SystemTime::now() - MAX_AGE;
    for e in entries.filter_map(Result::ok) {
        if e.metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < cutoff)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
