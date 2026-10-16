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

const SUMMARY_LEN: usize = 140;

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub aliases: Vec<String>,
    pub summary: String,
    pub hub: bool,
}

#[must_use]
pub fn build(vault: &Vault) -> Vec<Entry> {
    vault
        .notes
        .iter()
        .map(|n| Entry {
            id: n.id.clone(),
            kind: n.parsed.frontmatter.str_field("type").map(str::to_string),
            aliases: n.parsed.frontmatter.aliases(),
            summary: summary(n),
            hub: vault.config.is_hub(&n.id),
        })
        .collect()
}

/// First prose line under a `Purpose`/`Summary` heading, else the first prose line.
fn summary(note: &Note) -> String {
    let body = &note.text[note.parsed.body_start..];
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
        {
            continue;
        }
        return clean(line);
    }
    String::new()
}

fn clean(line: &str) -> String {
    let s = line.trim_start_matches(['-', '*', '>', ' ']);
    let s = WIKILINK.replace_all(s, |c: &regex::Captures<'_>| {
        c.get(2).map_or_else(
            || c[1].rsplit('/').next().unwrap_or(&c[1]).to_string(),
            |a| a.as_str().to_string(),
        )
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
