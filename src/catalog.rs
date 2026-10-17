//! One line per note: what it is, so an agent can see the whole vault at once.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::vault::{Note, Vault};

static WIKILINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!?\[\[([^\[\]|\n]+)(?:\|([^\[\]\n]+))?\]\]").expect("valid regex")
});
static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("valid regex"));
static HTML_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("valid regex"));

const SUMMARY_LEN: usize = 140;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The H1 title, when it says more than the file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub aliases: Vec<String>,
    pub summary: String,
    pub hub: bool,
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
fn title(note: &Note, generic: &[String]) -> Option<String> {
    let body = &note.text[note.parsed.body_start..];
    let h1 = body
        .lines()
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
fn summary(note: &Note, generic: &[String]) -> String {
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
    let mut in_fence = false;
    for line in &lines[start..] {
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence
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

/// Display text for a link target: its alias, else the note's name
/// (`Projects/x/overview` -> `x`), without `#subpath`.
fn link_text(target: &str, alias: Option<&str>, generic: &[String]) -> String {
    if let Some(a) = alias {
        return a.trim().to_string();
    }
    let path = target.split('#').next().unwrap_or(target).trim();
    let mut parts = path.rsplit('/');
    let stem = parts.next().unwrap_or(path);
    if generic.iter().any(|g| g.eq_ignore_ascii_case(stem))
        && let Some(parent) = parts.next()
    {
        return parent.to_string();
    }
    stem.to_string()
}

fn clean(line: &str, generic: &[String]) -> String {
    let s = line.trim_start_matches(['-', '*', '>', ' ']);
    let s = WIKILINK.replace_all(s, |c: &regex::Captures<'_>| {
        link_text(&c[1], c.get(2).map(|a| a.as_str()), generic)
    });
    let s = MD_LINK.replace_all(&s, "$1");
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
    fn link_text_uses_alias_or_name() {
        let g = vec!["overview".to_string()];
        assert_eq!(link_text("Projects/x/overview", None, &g), "x");
        assert_eq!(link_text("A/b#Head", None, &g), "b");
        assert_eq!(link_text("A/b", Some("Bee"), &g), "Bee");
    }
}
