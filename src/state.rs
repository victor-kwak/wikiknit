//! Link-pass state: which notes changed and still need a link pass.
//!
//! Two records, both outside the vault in `$XDG_STATE_HOME/wikiknit/<vault-hash>/`, so they
//! never show up in git and work for vaults that are not in git at all:
//!
//! - `linked.json` (one per vault): each note's content hash at its last link pass. A note
//!   whose content differs is *pending*. A pass in any session counts for every session.
//!   When it is missing (first use, lost state, a new `XDG_STATE_HOME`), a vault in git
//!   starts from git: notes that differ from `HEAD` (or are not committed) are pending, the
//!   rest count as linked. A vault outside git starts from the oldest live session's start,
//!   else from the vault as it is.
//! - `sessions/<id>.json` (one per agent session): content hashes when the session started,
//!   so a session can tell which pending notes changed while it ran.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::vault::Vault;

pub const SESSION_ENV: &str = "WIKIKNIT_SESSION";
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
/// A session file touched this recently counts as "active" when guessing the session.
const ACTIVE_WINDOW: Duration = Duration::from_secs(12 * 3600);

/// note id -> content hash
pub type Hashes = BTreeMap<String, String>;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Linked {
    hashes: Hashes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub created_unix: u64,
    /// Content hashes when the session started.
    pub start: Hashes,
    /// Digest of the session's pending set at the last Stop; tells a change set that sat
    /// untouched for a whole turn from work in progress.
    #[serde(default)]
    pub last_stop_digest: Option<String>,
}

/// Per-user state root. No shared fallback (such as `/tmp`): another user could
/// plant files or symlinks there.
fn state_root() -> Result<PathBuf, Error> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("wikiknit"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && home.is_absolute()
    {
        return Ok(home.join(".local/state/wikiknit"));
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Ok(dir.join("wikiknit"));
    }
    Err(Error::Usage(
        "cannot find a state directory: set XDG_STATE_HOME or HOME".into(),
    ))
}

/// Create `dir` (and parents) readable only by the current user.
pub(crate) fn create_private_dir(dir: &Path) -> Result<(), Error> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|e| Error::io(dir, e))
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
pub fn current_hashes(vault: &Vault) -> Hashes {
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

/// Read JSON state; a missing file is `None`, a corrupt one is `None` with a warning
/// (the caller rebuilds it rather than failing, so hooks never silently switch off).
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(v) => Ok(Some(v)),
            Err(e) => {
                eprintln!(
                    "wikiknit: ignoring corrupt state file {} ({e}); rebuilding it",
                    path.display()
                );
                Ok(None)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(path, e)),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    use std::io::Write as _;
    let dir = path.parent().expect("state path has a parent");
    create_private_dir(dir)?;
    // Exclusive, randomly named temp file in the same dir, then an atomic rename.
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(|e| Error::io(dir, e))?;
    let text = serde_json::to_string(value).expect("state serializes");
    tmp.write_all(text.as_bytes())
        .map_err(|e| Error::io(tmp.path(), e))?;
    tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
    Ok(())
}

/// State for one vault.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    vault: PathBuf,
}

impl Store {
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        Ok(Self::open_at(&state_root()?, vault_root))
    }

    /// The store for `vault_root` under an explicit state root (tests, tools).
    #[must_use]
    pub fn open_at(state_root: &Path, vault_root: &Path) -> Self {
        let hash = blake3::hash(vault_root.to_string_lossy().as_bytes()).to_hex();
        Self {
            dir: state_root.join(&hash.as_str()[..16]),
            vault: vault_root.to_path_buf(),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn linked_path(&self) -> PathBuf {
        self.dir.join("linked.json")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.dir.join("sessions")
    }

    #[must_use]
    pub fn session_path(&self, session: &str) -> PathBuf {
        self.sessions_dir().join(file_name(session))
    }

    /// Exclusive lock on this vault's state, held until the returned file is dropped.
    fn lock(&self) -> Result<std::fs::File, Error> {
        create_private_dir(&self.dir)?;
        let path = self.dir.join("lock");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        f.lock().map_err(|e| Error::io(&path, e))?;
        Ok(f)
    }

    /// Hashes at each note's last link pass (caller holds the lock). A missing or corrupt
    /// record is rebuilt so that changed notes stay pending (fail closed): in git, every note
    /// that differs from `HEAD` is pending (a commit already passed the gate, uncommitted
    /// work may not have); outside git, notes take their content at the start of the oldest
    /// live session, and with no sessions the vault as it is now counts as linked.
    fn linked_locked(&self, now: &Hashes) -> Result<Hashes, Error> {
        if let Some(l) = read_json::<Linked>(&self.linked_path())? {
            return Ok(l.hashes);
        }
        if let Some(uncommitted) = uncommitted_notes(&self.vault) {
            let hashes: Hashes = now
                .iter()
                .map(|(id, h)| {
                    let linked = if uncommitted.contains(id) {
                        String::new()
                    } else {
                        h.clone()
                    };
                    (id.clone(), linked)
                })
                .collect();
            write_json(
                &self.linked_path(),
                &Linked {
                    hashes: hashes.clone(),
                },
            )?;
            return Ok(hashes);
        }
        let mut sessions: Vec<Session> = std::fs::read_dir(self.sessions_dir())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| read_json::<Session>(&e.path()).ok().flatten())
            .collect();
        sessions.sort_by_key(|s| s.created_unix);
        let mut hashes = now.clone();
        let mut taken = BTreeSet::new();
        for s in sessions {
            for (id, h) in s.start {
                if hashes.contains_key(&id) && taken.insert(id.clone()) {
                    hashes.insert(id, h);
                }
            }
        }
        write_json(
            &self.linked_path(),
            &Linked {
                hashes: hashes.clone(),
            },
        )?;
        Ok(hashes)
    }

    fn linked(&self, now: &Hashes) -> Result<Hashes, Error> {
        let _lock = self.lock()?;
        self.linked_locked(now)
    }

    /// Notes whose content changed since their last link pass (any session).
    pub fn pending(&self, now: &Hashes) -> Result<BTreeSet<String>, Error> {
        let linked = self.linked(now)?;
        Ok(now
            .iter()
            .filter(|(id, h)| linked.get(*id) != Some(*h))
            .map(|(id, _)| id.clone())
            .collect())
    }

    pub fn load_session(&self, session: &str) -> Result<Option<Session>, Error> {
        read_json(&self.session_path(session))
    }

    pub fn save_session(&self, session: &str, s: &Session) -> Result<(), Error> {
        write_json(&self.session_path(session), s)
    }

    /// Start a session if it has no record yet. With `carry_pending` (after `/clear`, which
    /// starts a new session id in the same terminal), notes already pending count as changed
    /// in this session, so a link pass owed from before the clear is not forgotten.
    pub fn start_session(
        &self,
        session: &str,
        now: &Hashes,
        carry_pending: bool,
    ) -> Result<Session, Error> {
        let _lock = self.lock()?;
        if let Some(s) = self.load_session(session)? {
            return Ok(s);
        }
        let linked = self.linked_locked(now)?; // also makes sure the vault record exists
        let mut start = now.clone();
        if carry_pending {
            for (id, h) in &mut start {
                if linked.get(id) != Some(h) {
                    // Pretend the note had its linked content when the session started.
                    *h = linked.get(id).cloned().unwrap_or_default();
                }
            }
        }
        let s = Session {
            created_unix: now_unix(),
            start,
            last_stop_digest: None,
        };
        self.save_session(session, &s)?;
        Ok(s)
    }

    /// Pending notes that changed while this session ran.
    pub fn changed(&self, session: &Session, now: &Hashes) -> Result<BTreeSet<String>, Error> {
        Ok(self
            .pending(now)?
            .into_iter()
            .filter(|id| session.start.get(id) != now.get(id))
            .collect())
    }

    /// Record the current content of `ids` as linked; returns the ids that were pending.
    pub fn mark(&self, ids: &BTreeSet<String>, now: &Hashes) -> Result<BTreeSet<String>, Error> {
        // Read, merge, and write under the lock so concurrent marks are all kept.
        let _lock = self.lock()?;
        let mut linked = self.linked_locked(now)?;
        let mut marked = BTreeSet::new();
        for id in ids {
            if let Some(h) = now.get(id)
                && linked.get(id) != Some(h)
            {
                linked.insert(id.clone(), h.clone());
                marked.insert(id.clone());
            }
        }
        write_json(&self.linked_path(), &Linked { hashes: linked })?;
        Ok(marked)
    }

    /// Delete session records older than 30 days (and pre-0.2 baseline files).
    pub fn prune(&self) {
        let cutoff = SystemTime::now() - MAX_AGE;
        for dir in [self.sessions_dir(), self.dir.clone()] {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.filter_map(Result::ok) {
                let path = e.path();
                let legacy = dir == self.dir
                    && path.extension().is_some_and(|x| x == "json")
                    && path.file_name().is_some_and(|n| n != "linked.json");
                let old = dir != self.dir
                    && e.metadata()
                        .and_then(|m| m.modified())
                        .is_ok_and(|t| t < cutoff);
                if legacy || old {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// Which session a CLI call refers to: the flag, then `$WIKIKNIT_SESSION`, then the one
    /// session active in the last 12 hours. Several active sessions is an error: guessing
    /// could report another session's work.
    pub fn resolve_session(&self, flag: Option<&str>) -> Result<String, Error> {
        if let Some(s) = flag {
            return Ok(s.to_string());
        }
        if let Ok(s) = std::env::var(SESSION_ENV)
            && !s.is_empty()
        {
            return Ok(s);
        }
        let cutoff = SystemTime::now() - ACTIVE_WINDOW;
        let mut active: Vec<(SystemTime, String)> = std::fs::read_dir(self.sessions_dir())
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let t = e.metadata().ok()?.modified().ok()?;
                let id = e.path().file_stem()?.to_string_lossy().into_owned();
                (t >= cutoff).then_some((t, id))
            })
            .collect();
        active.sort();
        match active.len() {
            1 => Ok(active.remove(0).1),
            0 => Err(Error::Usage(format!(
                "no active session for this vault; pass --session <id> (state: {})",
                self.dir.display()
            ))),
            _ => Err(Error::Usage(format!(
                "several sessions are active ({}); pass --session <id> or set {SESSION_ENV}",
                active
                    .iter()
                    .rev()
                    .map(|(_, id)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

/// Ids of the notes in `vault` that differ from git `HEAD`: changed, staged, or not committed
/// (all notes when there is no commit yet). `None` when the vault is not in a git work tree
/// or git fails.
fn uncommitted_notes(vault: &Path) -> Option<BTreeSet<String>> {
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(vault)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        out.status.success().then_some(out.stdout)
    };
    let inside = git(&["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim_ascii() != b"true" {
        return None;
    }
    let mut lists = Vec::new();
    if git(&["rev-parse", "--verify", "-q", "HEAD"]).is_some() {
        lists.push(git(&[
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-renames",
            "--relative",
            "HEAD",
            "--",
            ".",
        ])?);
        lists.push(git(&[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ])?);
    } else {
        lists.push(git(&[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ".",
        ])?);
    }
    Some(
        lists
            .iter()
            .flat_map(|l| l.split(|b| *b == 0))
            .filter_map(|p| {
                let p = String::from_utf8_lossy(p);
                let n = p.len();
                (n > 3 && p.is_char_boundary(n - 3) && p[n - 3..].eq_ignore_ascii_case(".md"))
                    .then(|| p[..n - 3].to_string())
            })
            .collect(),
    )
}

/// Digest of a set of notes and their current content.
#[must_use]
pub fn digest(ids: &BTreeSet<String>, now: &Hashes) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let mut h = blake3::Hasher::new();
    for id in ids {
        h.update(id.as_bytes());
        h.update(now.get(id).map_or(&b""[..], |s| s.as_bytes()));
    }
    Some(h.finalize().to_hex().to_string())
}
