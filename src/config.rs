//! `wikiknit.toml`: where the vault is and which link rules apply.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use crate::Error;

pub const CONFIG_FILE: &str = "wikiknit.toml";

/// How wikilinks must name their target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkStyle {
    /// Obsidian's default: the shortest unique name. Resolution is best-effort.
    #[default]
    Shortest,
    /// Every wikilink must be the full vault-relative path. Short or relative links are errors.
    FullPath,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Off,
    Warn,
    Error,
}

// Unknown keys land in `removed` (`deny_unknown_fields` does not work with `flatten`); `load`
// rejects any that is not a removed feature's.
#[derive(Debug, Clone, Deserialize)]
struct RawConfig {
    #[serde(default = "default_vault")]
    vault: PathBuf,
    #[serde(default)]
    link_style: LinkStyle,
    #[serde(default)]
    ignore: Vec<String>,
    #[serde(default)]
    hubs: Vec<String>,
    #[serde(default = "default_min_alias_len")]
    min_alias_len: usize,
    #[serde(default = "default_generic_names")]
    generic_names: Vec<String>,
    #[serde(default)]
    alias_only: Vec<String>,
    #[serde(default)]
    require_link: Vec<RawRequireLink>,
    #[serde(default)]
    require_inbound: Vec<RawRequireInbound>,
    #[serde(default)]
    orphan_exempt: Vec<String>,
    #[serde(default)]
    stale: Vec<RawStale>,
    #[serde(default)]
    drift: Vec<RawDrift>,
    #[serde(default)]
    forbid_link: Vec<RawForbidLink>,
    #[serde(default)]
    severity: BTreeMap<String, Severity>,
    /// Keys of features removed in 0.7 (search, suggest): read and ignored, with a note.
    #[serde(flatten)]
    removed: BTreeMap<String, toml::Value>,
}

/// Top-level keys that configured removed features: accepted so old configs still load.
const REMOVED_KEYS: &[&str] = &[
    "stemmer",
    "embedding_model",
    "link_section",
    "min_similarity",
    "semantic_margin",
    "no_backlink_from",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequireLink {
    when: RawWhen,
    target: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequireInbound {
    when: RawWhen,
    from: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStale {
    field: String,
    days: u32,
    path: Option<String>,
    #[serde(default)]
    required: bool,
    fix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDrift {
    path: Option<String>,
    #[serde(default = "default_repo_field")]
    repo_field: String,
    #[serde(default = "default_commit_field")]
    commit_field: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForbidLink {
    target: String,
    from: Option<String>,
    near: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWhen {
    field: Option<String>,
    matches: Option<String>,
    path: Option<String>,
}

fn default_vault() -> PathBuf {
    PathBuf::from(".")
}
fn default_repo_field() -> String {
    "path".into()
}

fn default_commit_field() -> String {
    "verified_commit".into()
}

fn default_min_alias_len() -> usize {
    4
}
fn default_generic_names() -> Vec<String> {
    ["overview", "index", "readme"].map(String::from).to_vec()
}

/// Condition that makes a note need a link.
#[derive(Debug, Clone)]
pub enum When {
    /// Frontmatter `field` is a string that matches `pattern`.
    Field { field: String, pattern: Regex },
    /// The note id (vault-relative path without `.md`) matches `pattern`.
    Path { pattern: Regex },
}

#[derive(Debug, Clone)]
pub struct RequireLink {
    pub when: When,
    /// Target note id; `$1`, `$2`, ... are capture groups from `when`.
    pub target: String,
}

/// Notes matching `when` must be linked from note `from`.
#[derive(Debug, Clone)]
pub struct RequireInbound {
    pub when: When,
    /// Note id that must link them; `$1`, `$2`, ... are capture groups from `when`.
    pub from: String,
}

/// A note whose date `field` is older than `days` is stale.
#[derive(Debug, Clone)]
pub struct Stale {
    /// Frontmatter field holding a date (`YYYY-MM-DD`, optionally followed by a time).
    pub field: String,
    pub days: u32,
    /// Only notes whose id matches; `None` = every note.
    pub path: Option<Regex>,
    /// A matching note without the field is reported too.
    pub required: bool,
    /// The fix to show instead of the default ("check the note against its source, ...").
    pub fix: Option<String>,
}

/// A note that records the commit it was last checked against (`verified_commit`) and the
/// repository it describes (`path`): the `drift` rule counts the commits made since.
#[derive(Debug, Clone)]
pub struct Drift {
    /// Only notes whose id matches; `None` = every note with both fields.
    pub path: Option<Regex>,
    /// Frontmatter field with the repository folder (`~/` is the home directory).
    pub repo_field: String,
    /// Frontmatter field with the commit the note was checked against.
    pub commit_field: String,
}

/// Links that must not be written: to notes matching `target`, from notes matching `from`,
/// on a line or under a heading matching `near` (`Issues/resolved/` under "open issues").
#[derive(Debug, Clone)]
pub struct ForbidLink {
    pub target: Regex,
    pub from: Option<Regex>,
    pub near: Option<Regex>,
    pub message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Absolute vault root.
    pub vault: PathBuf,
    /// File the config came from, if any.
    pub source: Option<PathBuf>,
    pub link_style: LinkStyle,
    /// Vault-relative path prefixes to skip, e.g. `Templates/`.
    pub ignore: Vec<String>,
    /// Note ids that link everything (indexes). They are not link targets for
    /// mentions and are left out of graph components.
    pub hubs: Vec<String>,
    pub min_alias_len: usize,
    /// File stems too generic to name a note (`overview`); the parent folder names it instead.
    pub generic_names: Vec<String>,
    /// Vault-relative prefixes whose notes are found in prose by their aliases only,
    /// not by their name (for notes named with common words, like `Systems/display`).
    pub alias_only: Vec<String>,
    pub require_link: Vec<RequireLink>,
    pub require_inbound: Vec<RequireInbound>,
    /// Prefixes of notes that need no inbound link (logs, inboxes): the `orphan` rule skips them.
    pub orphan_exempt: Vec<String>,
    /// Freshness rules; the first whose `path` matches a note applies to it.
    pub stale: Vec<Stale>,
    pub drift: Vec<Drift>,
    pub forbid_link: Vec<ForbidLink>,
    severity: BTreeMap<String, Severity>,
}

impl Config {
    /// Default config for a vault with no `wikiknit.toml`.
    #[must_use]
    pub fn for_vault(vault: PathBuf) -> Self {
        Self {
            vault,
            source: None,
            link_style: LinkStyle::default(),
            ignore: Vec::new(),
            hubs: Vec::new(),
            min_alias_len: default_min_alias_len(),
            generic_names: default_generic_names(),
            alias_only: Vec::new(),
            require_link: Vec::new(),
            require_inbound: Vec::new(),
            orphan_exempt: Vec::new(),
            stale: Vec::new(),
            drift: Vec::new(),
            forbid_link: Vec::new(),
            severity: BTreeMap::new(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        Self::load_with(path, true)
    }

    /// `load`, with notes about removed keys on stderr only when `notes` (a copy of the config
    /// from another git version is loaded quietly: the note is about the file on disk).
    pub fn load_with(path: &Path, notes: bool) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let raw: RawConfig = toml::from_str(&text).map_err(|e| Error::Config {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        // `wikiknit.toml` (a bare name) has the parent "": that is the current directory.
        let base = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        // The vault must be named inside the config's folder (a relative path that does not
        // climb out): a cloned repo's config must not point the hooks at `/` or the home
        // directory. The check is on the path as written, so a vault folder that is itself a
        // symlink still works.
        let mut depth = 0i32;
        let inside = raw.vault.components().all(|c| match c {
            std::path::Component::Normal(_) => {
                depth += 1;
                true
            }
            std::path::Component::CurDir => true,
            std::path::Component::ParentDir => {
                depth -= 1;
                depth >= 0
            }
            _ => false, // absolute path or drive prefix
        });
        if !inside {
            return Err(Error::Config {
                path: path.to_path_buf(),
                message: format!(
                    "vault `{}` is outside the config's folder; use a relative path inside it",
                    raw.vault.display()
                ),
            });
        }
        let vault = base.join(&raw.vault);
        let vault = vault.canonicalize().map_err(|e| Error::io(&vault, e))?;
        // A symlinked vault folder is fine, but not one that resolves to `/`, the home
        // directory, or a folder above the config (a cloned repo could plant such a link).
        let base_abs = base.canonicalize().map_err(|e| Error::io(base, e))?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let too_broad = vault.parent().is_none()
            || home.is_some_and(|h| h.canonicalize().is_ok_and(|h| h == vault))
            || (vault != base_abs && base_abs.starts_with(&vault));
        if too_broad {
            return Err(Error::Config {
                path: path.to_path_buf(),
                message: format!(
                    "vault resolves to {}, which is the filesystem root, the home directory, \
                     or a folder above the config",
                    vault.display()
                ),
            });
        }
        let bad = |message: String| Error::Config {
            path: path.to_path_buf(),
            message,
        };
        let when = |w: RawWhen, key: &str| -> Result<When, Error> {
            match (w.field, w.matches, w.path) {
                (Some(field), Some(m), None) => Ok(When::Field {
                    field,
                    pattern: Regex::new(&m).map_err(|e| bad(format!("{key}.when.matches: {e}")))?,
                }),
                (None, None, Some(p)) => Ok(When::Path {
                    pattern: Regex::new(&p).map_err(|e| bad(format!("{key}.when.path: {e}")))?,
                }),
                _ => Err(bad(format!(
                    "{key}.when needs either `field` + `matches`, or `path`"
                ))),
            }
        };
        let mut require_link = Vec::new();
        for r in raw.require_link {
            require_link.push(RequireLink {
                when: when(r.when, "require_link")?,
                target: note_ref(&r.target),
            });
        }
        let mut require_inbound = Vec::new();
        for r in raw.require_inbound {
            require_inbound.push(RequireInbound {
                when: when(r.when, "require_inbound")?,
                from: note_ref(&r.from),
            });
        }
        let mut stale = Vec::new();
        for s in raw.stale {
            if s.field.trim().is_empty() {
                return Err(bad("stale.field must name a frontmatter field".into()));
            }
            stale.push(Stale {
                field: s.field,
                days: s.days,
                path: s
                    .path
                    .map(|p| Regex::new(&p).map_err(|e| bad(format!("stale.path: {e}"))))
                    .transpose()?,
                required: s.required,
                fix: s.fix,
            });
        }
        // One note per key and run, though `--changed` loads a config up to three times.
        static WARNED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        for key in raw.removed.keys() {
            if !REMOVED_KEYS.contains(&key.as_str()) {
                return Err(bad(format!("unknown field `{key}`")));
            }
            let mut warned = WARNED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !notes || warned.contains(key) {
                continue;
            }
            warned.push(key.clone());
            eprintln!(
                "wikiknit: {}: `{key}` is ignored (its feature was removed in 0.7); delete it",
                path.display()
            );
        }
        let regex = |p: &str, key: &str| Regex::new(p).map_err(|e| bad(format!("{key}: {e}")));
        let mut drift = Vec::new();
        for d in raw.drift {
            if d.repo_field.trim().is_empty() || d.commit_field.trim().is_empty() {
                return Err(bad(
                    "drift.repo_field and drift.commit_field must name frontmatter fields".into(),
                ));
            }
            drift.push(Drift {
                path: d.path.map(|p| regex(&p, "drift.path")).transpose()?,
                repo_field: d.repo_field,
                commit_field: d.commit_field,
            });
        }
        let mut forbid_link = Vec::new();
        for f in raw.forbid_link {
            forbid_link.push(ForbidLink {
                target: regex(&f.target, "forbid_link.target")?,
                from: f.from.map(|p| regex(&p, "forbid_link.from")).transpose()?,
                near: f.near.map(|p| regex(&p, "forbid_link.near")).transpose()?,
                message: f.message,
            });
        }
        for key in raw.severity.keys() {
            if !crate::rules::RULES.iter().any(|(name, _)| name == key) {
                return Err(bad(format!("severity: unknown rule `{key}`")));
            }
        }
        Ok(Self {
            vault,
            source: Some(path.to_path_buf()),
            link_style: raw.link_style,
            ignore: raw.ignore.into_iter().map(|p| prefix_ref(&p)).collect(),
            hubs: raw.hubs.iter().map(|h| note_ref(h)).collect(),
            min_alias_len: raw.min_alias_len,
            generic_names: raw
                .generic_names
                .into_iter()
                .map(|s| s.to_lowercase())
                .collect(),
            alias_only: raw.alias_only.into_iter().map(|p| prefix_ref(&p)).collect(),
            require_link,
            require_inbound,
            orphan_exempt: raw
                .orphan_exempt
                .into_iter()
                .map(|p| prefix_ref(&p))
                .collect(),
            stale,
            drift,
            forbid_link,
            severity: raw.severity,
        })
    }

    /// Find `wikiknit.toml` in `start` or a parent directory, up to the top of the git work
    /// tree `start` is in (the folder holding `.git`): a config above the repository belongs
    /// to something else.
    #[must_use]
    pub fn find(start: &Path) -> Option<PathBuf> {
        for dir in start.ancestors() {
            let file = dir.join(CONFIG_FILE);
            if file.is_file() {
                return Some(file);
            }
            if dir.join(".git").exists() {
                return None;
            }
        }
        None
    }

    /// Effective severity for a rule (config override, else the rule default).
    #[must_use]
    pub fn severity(&self, rule: &str) -> Severity {
        if let Some(s) = self.severity.get(rule) {
            return *s;
        }
        crate::rules::RULES
            .iter()
            .find(|(name, _)| *name == rule)
            .map_or(Severity::Error, |(_, s)| *s)
    }

    #[must_use]
    pub fn is_ignored(&self, id_or_rel: &str) -> bool {
        under_any(&self.ignore, id_or_rel)
    }

    #[must_use]
    pub fn is_orphan_exempt(&self, id: &str) -> bool {
        under_any(&self.orphan_exempt, id)
    }

    /// The drift rule for a note: the first `[[drift]]` whose `path` matches it.
    #[must_use]
    pub fn drift_rule(&self, id: &str) -> Option<&Drift> {
        self.drift
            .iter()
            .find(|d| d.path.as_ref().is_none_or(|p| p.is_match(id)))
    }

    /// The freshness rule for a note: the first `[[stale]]` whose `path` matches it.
    #[must_use]
    pub fn stale_rule(&self, id: &str) -> Option<&Stale> {
        self.stale
            .iter()
            .find(|s| s.path.as_ref().is_none_or(|p| p.is_match(id)))
    }

    #[must_use]
    pub fn is_alias_only(&self, id: &str) -> bool {
        under_any(&self.alias_only, id)
    }

    #[must_use]
    pub fn is_hub(&self, id: &str) -> bool {
        self.hubs.iter().any(|h| h == id)
    }
}

/// A note named in config as written (`./INDEX.md`) -> its id form (`INDEX`).
fn note_ref(s: &str) -> String {
    let s = s.trim().trim_start_matches("./").trim_start_matches('/');
    s.strip_suffix(".md").unwrap_or(s).to_string()
}

/// A folder or path prefix as written (`./Sessions/`, `/Templates`) -> vault-relative form.
fn prefix_ref(s: &str) -> String {
    s.trim()
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

/// `path` equals one of the prefixes or sits under it.
fn under_any(prefixes: &[String], path: &str) -> bool {
    prefixes.iter().any(|p| {
        let p = p.trim_end_matches('/');
        path == p || path.starts_with(&format!("{p}/"))
    })
}
