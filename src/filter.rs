//! Note filters shared by `catalog` and `links`.

use regex::Regex;
use serde_json::Value;

use crate::Error;
use crate::vault::{Note, Vault};

/// Which notes to keep. Each kind of condition must hold; within a kind, any value may match
/// (`--type issue --type session` keeps both).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    types: Vec<String>,
    paths: Vec<String>,
    wheres: Vec<(String, Regex)>,
    tags: Vec<String>,
    /// Keep notes that link to this note (index into `Vault::notes`).
    linked_to: Option<usize>,
}

impl Filter {
    /// Build from CLI values. `wheres` are `key=regex` (case-insensitive); `linked_to` is any
    /// note reference [`Vault::find_note`] accepts.
    pub fn new(
        vault: &Vault,
        types: &[String],
        paths: &[String],
        wheres: &[String],
        tags: &[String],
        linked_to: Option<&str>,
    ) -> Result<Self, Error> {
        let wheres = wheres
            .iter()
            .map(|w| {
                let (k, pat) = w
                    .split_once('=')
                    .ok_or_else(|| Error::Usage(format!("--where `{w}`: expected key=regex")))?;
                let re = Regex::new(&format!("(?i){pat}"))
                    .map_err(|e| Error::Usage(format!("--where `{w}`: {e}")))?;
                Ok((k.trim().to_string(), re))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let linked_to = linked_to.map(|r| vault.find_note(r)).transpose()?;
        Ok(Self {
            types: types.iter().map(|t| t.to_lowercase()).collect(),
            paths: paths
                .iter()
                .map(|p| p.trim_start_matches("./").trim_end_matches('/').to_string())
                .collect(),
            wheres,
            tags: tags
                .iter()
                .filter_map(|t| crate::parse::normalize_tag(t))
                .collect(),
            linked_to,
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
            && self.paths.is_empty()
            && self.wheres.is_empty()
            && self.tags.is_empty()
            && self.linked_to.is_none()
    }

    #[must_use]
    pub fn matches(&self, vault: &Vault, note: &Note) -> bool {
        let fm = &note.parsed.frontmatter;
        if !self.types.is_empty() {
            let kind = fm.str_field("type").map(str::to_lowercase);
            if !kind.is_some_and(|k| self.types.contains(&k)) {
                return false;
            }
        }
        if !self.paths.is_empty()
            && !self
                .paths
                .iter()
                .any(|p| note.id == *p || note.id.starts_with(&format!("{p}/")))
        {
            return false;
        }
        for (key, re) in &self.wheres {
            let value = fm.value.as_ref().and_then(|v| v.get(key));
            if !value.is_some_and(|v| value_matches(v, re)) {
                return false;
            }
        }
        if !self.tags.is_empty() {
            let tags = note.tags();
            // `--tag project` also keeps nested `project/alpha`.
            let hit = self.tags.iter().any(|want| {
                tags.iter()
                    .any(|t| t == want || t.starts_with(&format!("{want}/")))
            });
            if !hit {
                return false;
            }
        }
        if let Some(target) = self.linked_to
            && !vault.outbound(note).contains(&target)
        {
            return false;
        }
        true
    }
}

/// A scalar matches as text; a list matches if any element does.
fn value_matches(v: &Value, re: &Regex) -> bool {
    match v {
        Value::String(s) => re.is_match(s),
        Value::Number(n) => re.is_match(&n.to_string()),
        Value::Bool(b) => re.is_match(&b.to_string()),
        Value::Array(items) => items.iter().any(|i| value_matches(i, re)),
        Value::Null | Value::Object(_) => false,
    }
}
