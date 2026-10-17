//! Unlinked mentions: prose that names another note (by name or alias) without linking it.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use serde::Serialize;

use crate::vault::{Note, Target, Vault};

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
    /// pattern index -> notes that use it as their name or an alias
    owners: Vec<Vec<usize>>,
}

/// Lowercased text plus, for each byte of it, the byte offset of the original char.
struct Folded {
    text: String,
    /// `orig[i]` = original offset for folded byte `i`; one extra entry for the end.
    orig: Vec<usize>,
}

/// Unicode lowercasing (per char, not full case folding) that keeps a map back to the
/// original offsets: `É` and `é` match, so do `Москва` and `москва`; `STRASSE` and `straße`
/// do not.
fn fold(text: &str) -> Folded {
    let mut out = String::with_capacity(text.len());
    let mut orig = Vec::with_capacity(text.len() + 1);
    for (i, c) in text.char_indices() {
        for l in c.to_lowercase() {
            let before = out.len();
            out.push(l);
            orig.extend(std::iter::repeat_n(i, out.len() - before));
        }
    }
    orig.push(text.len());
    Folded { text: out, orig }
}

/// The word that names a note in prose, if any: its stem, or its folder for generic stems
/// (`Projects/x/overview` -> `x`). A generic stem at the vault root (`index.md`) names nothing.
fn prose_name<'a>(note: &'a Note, generic: &[String]) -> Option<&'a str> {
    let stem = note.stem();
    if generic.iter().any(|g| g.eq_ignore_ascii_case(stem)) && !note.id.contains('/') {
        return None;
    }
    Some(note.name(generic))
}

fn build_terms(vault: &Vault) -> Option<Terms> {
    let cfg = &vault.config;
    // folded term -> owners
    let mut by_term: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (i, n) in vault.notes.iter().enumerate() {
        if cfg.is_hub(&n.id) {
            continue;
        }
        let mut terms = Vec::new();
        if !cfg.is_alias_only(&n.id)
            && let Some(name) = prose_name(n, &cfg.generic_names)
        {
            terms.push(name.to_string());
        }
        terms.extend(n.parsed.frontmatter.aliases());
        for t in terms {
            let t = t.trim();
            if t.chars().count() < cfg.min_alias_len {
                continue;
            }
            by_term.entry(fold(t).text).or_default().insert(i);
        }
    }
    if by_term.is_empty() {
        return None;
    }
    let (patterns, owners): (Vec<String>, Vec<Vec<usize>>) = by_term
        .into_iter()
        .map(|(t, o)| (t, o.into_iter().collect()))
        .unzip();
    let ac = AhoCorasickBuilder::new()
        .match_kind(MatchKind::LeftmostLongest)
        .build(&patterns)
        .expect("valid patterns");
    Some(Terms { ac, owners })
}

/// Letters, digits, `_`, and combining marks (so `café` written as `cafe` + U+0301 is one word).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
        || c == '_'
        || matches!(c as u32,
            0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

/// Scripts written without spaces between words (CJK, kana, Thai, ...) and Korean, whose
/// particles attach to nouns: a match there needs no word boundary on that side.
fn is_unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF      // Thai, Lao
        | 0x1100..=0x11FF    // Hangul Jamo
        | 0x3040..=0x30FF    // Hiragana, Katakana
        | 0x3130..=0x318F    // Hangul compatibility Jamo
        | 0x31F0..=0x31FF    // Katakana extensions
        | 0x3400..=0x4DBF    // CJK extension A
        | 0x4E00..=0x9FFF    // CJK unified
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK compatibility
        | 0x20000..=0x2FA1F) // CJK extensions B-F, compatibility supplement
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
        let linked: BTreeSet<usize> = vault
            .resolve_links(note)
            .iter()
            .filter_map(|r| match r.target {
                Target::Note(t) => Some(t),
                _ => None,
            })
            .collect();
        let p = &note.parsed;
        let mut skip: Vec<Range<usize>> = p.code.clone();
        skip.extend(p.links.iter().map(|l| l.span.clone()));
        skip.extend(p.code_links.iter().map(|l| l.span.clone()));
        skip.extend(p.html_links.iter().map(|l| l.span.clone()));
        skip.extend(p.non_prose.iter().cloned());
        skip.extend(URL.find_iter(&note.text).map(|m| m.range()));

        let body_start = p.body_start;
        let folded = fold(&note.text[body_start..]);
        // target -> (first match start, term, count)
        let mut hits: BTreeMap<usize, (usize, String, usize)> = BTreeMap::new();
        for m in terms.ac.find_iter(&folded.text) {
            let abs = body_start + folded.orig[m.start()]..body_start + folded.orig[m.end()];
            if abs.is_empty() || inside(&skip, &abs) {
                continue;
            }
            let matched = &note.text[abs.clone()];
            let first = matched.chars().next().expect("non-empty");
            let last = matched.chars().next_back().expect("non-empty");
            let before = note.text[..abs.start].chars().next_back();
            let after = note.text[abs.end..].chars().next();
            if (!is_unspaced(first) && before.is_some_and(is_word_char))
                || (!is_unspaced(last) && after.is_some_and(is_word_char))
            {
                continue;
            }
            if note.line_text(abs.start).starts_with("# ") {
                continue; // the H1 title names the note itself
            }
            for &target in &terms.owners[m.pattern().as_usize()] {
                if target == i || linked.contains(&target) {
                    continue;
                }
                hits.entry(target).and_modify(|h| h.2 += 1).or_insert((
                    abs.start,
                    matched.to_string(),
                    1,
                ));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_maps_offsets() {
        let f = fold("Émile ÉMILE");
        assert_eq!(f.text, "émile émile");
        assert_eq!(f.orig[0], 0);
        assert_eq!(*f.orig.last().unwrap(), "Émile ÉMILE".len());
    }

    #[test]
    fn unspaced_scripts() {
        assert!(is_unspaced('東'));
        assert!(is_unspaced('タ'));
        assert!(is_unspaced('서'));
        assert!(!is_unspaced('a'));
        assert!(!is_unspaced('é'));
    }
}
