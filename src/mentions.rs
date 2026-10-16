//! Unlinked mentions: prose that names another note (by name or alias) without linking it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use serde::Serialize;

use crate::vault::{Target, Vault};

/// Bare URLs and autolinks: a name inside a URL (`github.com/<user>/<repo>`) is not prose.
static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(?:https?://|www\.)[^\s<>()\[\]"'`]+"#).expect("valid regex")
});

#[derive(Debug, Clone, Serialize)]
pub struct Mention {
    pub note: String,
    pub target: String,
    /// The text that matched (as written in the note).
    pub term: String,
    /// First line with a match.
    pub line: usize,
    pub count: usize,
    pub context: String,
}

struct Terms {
    ac: AhoCorasick,
    /// pattern index -> note index
    owner: Vec<usize>,
}

fn build_terms(vault: &Vault) -> Option<Terms> {
    let cfg = &vault.config;
    let mut patterns = Vec::new();
    let mut owner = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, n) in vault.notes.iter().enumerate() {
        if cfg.is_hub(&n.id) {
            continue;
        }
        let mut terms = Vec::new();
        if !cfg.is_alias_only(&n.id) {
            terms.push(n.name(&cfg.generic_names).to_string());
        }
        terms.extend(n.parsed.frontmatter.aliases());
        for t in terms {
            let t = t.trim().to_string();
            if t.chars().count() < cfg.min_alias_len || !seen.insert((t.to_lowercase(), i)) {
                continue;
            }
            patterns.push(t);
            owner.push(i);
        }
    }
    if patterns.is_empty() {
        return None;
    }
    let ac = AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::LeftmostLongest)
        .build(&patterns)
        .expect("valid patterns");
    Some(Terms { ac, owner })
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn inside(ranges: &[Range<usize>], r: &Range<usize>) -> bool {
    ranges.iter().any(|x| r.start < x.end && x.start < r.end)
}

/// Find unlinked mentions in the selected notes (`None` = every note).
#[must_use]
pub fn find(vault: &Vault, selected: Option<&BTreeSet<String>>) -> Vec<Mention> {
    let Some(terms) = build_terms(vault) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, note) in vault.notes.iter().enumerate() {
        if selected.is_some_and(|s| !s.contains(&note.id)) || vault.config.is_hub(&note.id) {
            continue;
        }
        let resolved = vault.resolve_links(note);
        let linked: BTreeSet<usize> = resolved
            .iter()
            .filter_map(|r| match r.target {
                Target::Note(t) => Some(t),
                _ => None,
            })
            .collect();
        let mut skip: Vec<Range<usize>> = note.parsed.code.clone();
        skip.extend(note.parsed.links.iter().map(|l| l.span.clone()));
        skip.extend(note.parsed.code_links.iter().map(|l| l.span.clone()));
        skip.extend(URL.find_iter(&note.text).map(|m| m.range()));
        // Skip the H1 title line: it names the note itself.
        let body = &note.text[note.parsed.body_start..];
        // target -> (first match start, term, count)
        let mut hits: BTreeMap<usize, (usize, String, usize)> = BTreeMap::new();
        for m in terms.ac.find_iter(body) {
            let target = terms.owner[m.pattern().as_usize()];
            if target == i || linked.contains(&target) {
                continue;
            }
            let abs = note.parsed.body_start + m.start()..note.parsed.body_start + m.end();
            if inside(&skip, &abs) {
                continue;
            }
            let before = note.text[..abs.start].chars().next_back();
            let after = note.text[abs.end..].chars().next();
            if before.is_some_and(is_word_char) || after.is_some_and(is_word_char) {
                continue;
            }
            let line_text = note.line_text(abs.start);
            if line_text.starts_with("# ") {
                continue;
            }
            hits.entry(target).and_modify(|h| h.2 += 1).or_insert((
                abs.start,
                note.text[abs.clone()].to_string(),
                1,
            ));
        }
        for (target, (start, term, count)) in hits {
            let mut context: String = note.line_text(start).to_string();
            if context.chars().count() > 160 {
                context = context.chars().take(157).collect::<String>() + "...";
            }
            out.push(Mention {
                note: note.id.clone(),
                target: vault.notes[target].id.clone(),
                term,
                line: note.line_of(start),
                count,
                context,
            });
        }
    }
    out
}
