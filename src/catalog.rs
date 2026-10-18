//! One line per note: what it is, so an agent can see the whole vault at once.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::text::{self, Fence};
use crate::vault::{Note, Vault};

static HTML_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("valid regex"));

const SUMMARY_LEN: usize = 140;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub hub: bool,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The H1 title, when it says more than the file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub summary: String,
}

#[must_use]
pub fn build(vault: &Vault) -> Vec<Entry> {
    let generic = &vault.config.generic_names;
    vault
        .notes
        .iter()
        .map(|n| Entry {
            id: n.id.clone(),
            kind: n.parsed.frontmatter.str_field("type").map(str::to_string),
            title: title(n, generic),
            tags: n.tags(),
            aliases: n.parsed.frontmatter.aliases(),
            summary: summary(n, generic),
            hub: vault.config.is_hub(&n.id),
        })
        .collect()
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The first `# ` heading, unless it only repeats the note's name.
#[must_use]
pub fn title(note: &Note, generic: &[String]) -> Option<String> {
    let body = &note.text[note.parsed.body_start..];
    // A `# comment` in a fenced shell block is not a heading.
    let mut fence = Fence::default();
    let h1 = body
        .lines()
        .filter(|l| !fence.line(l))
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))?
        .trim();
    let h1 = clean(h1, generic);
    let name = note.name(generic);
    (!h1.is_empty()
        && normalize(&h1) != normalize(name)
        && normalize(&h1) != normalize(note.stem()))
    .then_some(h1)
}

/// Frontmatter `summary`/`description`, else the first prose line under a
/// `Purpose`/`Summary`-style heading, else the first prose line.
#[must_use]
pub fn summary(note: &Note, generic: &[String]) -> String {
    for key in ["summary", "description"] {
        if let Some(s) = note.parsed.frontmatter.str_field(key)
            && !s.trim().is_empty()
        {
            return clean(s, generic);
        }
    }
    let body = HTML_COMMENT.replace_all(&note.text[note.parsed.body_start..], "");
    let lines: Vec<&str> = body.lines().map(str::trim).collect();
    let preferred = lines.iter().position(|l| {
        let h = l.trim_start_matches('#').trim().to_lowercase();
        l.starts_with("##")
            && ["purpose", "summary", "what", "symptom", "goal / symptom"].contains(&h.as_str())
    });
    let start = preferred.map_or(0, |i| i + 1);
    let mut fence = Fence::default();
    for (k, line) in lines.iter().enumerate() {
        let in_code = fence.line(line);
        if k < start {
            continue;
        }
        if in_code
            || line.is_empty()
            || line.starts_with('#')
            || line.starts_with("---")
            || line.starts_with('|')
            || line.starts_with('<')
        {
            continue;
        }
        return clean(line, generic);
    }
    String::new()
}

fn clean(line: &str, generic: &[String]) -> String {
    let s = line.trim_start_matches(['-', '*', '>', ' ']);
    let s = text::links_to_text(s, generic);
    let s = s.replace("**", "").replace('`', "");
    let s = s.trim();
    if s.chars().count() > SUMMARY_LEN {
        s.chars().take(SUMMARY_LEN - 3).collect::<String>() + "..."
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_uses_link_text() {
        let g = vec!["overview".to_string()];
        assert_eq!(
            clean("- **What**: behind [[Projects/x/overview]].", &g),
            "What: behind x."
        );
    }
}
