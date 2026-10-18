//! `suggest <note>`: ranked link candidates for a note, each with its reasons and the exact
//! edit an agent can apply. wikiknit never edits notes itself.
//!
//! Signals (each normalized to 0..1 across the candidates, then weighted):
//! - the note's text names the candidate (name or alias) without a link;
//! - the candidate links here, but the note does not link back;
//! - similar meaning (semantic index, if built) and similar text (BM25 on the note's title,
//!   aliases, and headings);
//! - shared neighbors in the link graph (Adamic-Adar, hubs left out);
//! - rare frontmatter values both notes have (`stack: cloudflare`).
//!
//! It also suggests back-links: a note this one links to that does not link back.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::Value;

use crate::config::LinkStyle;
use crate::filter::Filter;
use crate::mentions;
use crate::search;
use crate::vault::{Note, Vault};
use crate::vectors::{self, Index};

/// Frontmatter keys that describe a note's bookkeeping, not what it is about.
const SKIP_KEYS: &[&str] = &[
    "type",
    "aliases",
    "alias",
    "title",
    "summary",
    "description",
    "status",
    "date",
    "created",
    "updated",
    "opened",
    "closed",
    "last_verified",
    "last_updated",
    "verified_commit",
    "outcome",
    "path",
    "repo",
    "remote",
    "component",
];

/// Mean-vector cosine below which two notes are not "similar in meaning" (BGE-style scale).
const NOTE_SIMILARITY_LOW: f32 = 0.75;
/// A frontmatter value shared by more than this share of notes is not a signal (a value only
/// two notes have always is).
const COMMON_VALUE: f64 = 0.3;

#[derive(Debug, Clone, Serialize)]
pub struct Suggestion {
    /// The note to edit.
    pub note: String,
    /// `link` (add a link in `note`), `mention` (turn a mention in `note` into a link), or
    /// `backlink` (`note` is linked from the start note and should link back).
    pub action: &'static str,
    pub target: String,
    pub score: f64,
    pub reasons: Vec<String>,
    /// Where the edit goes (0 = end of the note).
    pub line: usize,
    /// The edit, ready to apply.
    pub edit: String,
}

/// How to write a link to `target` in the configured style.
#[must_use]
pub fn link_text(vault: &Vault, target: &Note) -> String {
    match vault.config.link_style {
        LinkStyle::FullPath => format!("[[{}]]", target.id),
        LinkStyle::Shortest => {
            let stem = target.stem();
            let unique = vault
                .notes
                .iter()
                .filter(|n| n.stem().eq_ignore_ascii_case(stem))
                .count()
                == 1;
            if unique {
                format!("[[{stem}]]")
            } else {
                format!("[[{}]]", target.id)
            }
        }
    }
}

/// Where and how to add a link to `target` in `editor`, following `link_section`:
/// a heading (`## Related`) or `frontmatter:<key>`.
#[must_use]
pub fn add_link_edit(vault: &Vault, editor: &Note, target: &Note) -> (usize, String) {
    let link = link_text(vault, target);
    let section = vault.config.link_section.trim();
    if let Some(key) = section.strip_prefix("frontmatter:") {
        let key = key.trim();
        if let Some(yaml) = &editor.parsed.frontmatter.yaml {
            let src = &editor.text[yaml.clone()];
            if let Some(i) = src
                .find(&format!("\n{key}:"))
                .map(|i| i + 1)
                .or_else(|| src.starts_with(&format!("{key}:")).then_some(0))
            {
                let line = editor.line_of(yaml.start + i);
                return (line, format!("add \"{link}\" to frontmatter `{key}`"));
            }
            let end = editor.line_of(yaml.end.saturating_sub(1).max(yaml.start));
            return (end, format!("add frontmatter line: {key}: [\"{link}\"]"));
        }
        return (
            1,
            format!("add frontmatter: ---\\n{key}: [\"{link}\"]\\n---"),
        );
    }
    // A heading: insert after the last non-blank line of that section.
    let body_start_line = editor.line_of(editor.parsed.body_start);
    let lines: Vec<&str> = editor.text[editor.parsed.body_start..].lines().collect();
    // Which lines are inside fenced code: a heading there is not a heading.
    let mut fence = crate::text::Fence::default();
    let in_code: Vec<bool> = lines.iter().map(|l| fence.line(l)).collect();
    if let Some(h) = lines
        .iter()
        .enumerate()
        .position(|(k, l)| !in_code[k] && l.trim() == section)
    {
        let level = section.chars().take_while(|c| *c == '#').count();
        let end = lines[h + 1..]
            .iter()
            .enumerate()
            .position(|(k, l)| {
                let t = l.trim_start();
                let lv = t.chars().take_while(|c| *c == '#').count();
                !in_code[h + 1 + k] && lv > 0 && lv <= level.max(1) && t[lv..].starts_with(' ')
            })
            .map_or(lines.len(), |p| h + 1 + p);
        let last = (h..end)
            .rev()
            .find(|&k| !lines[k].trim().is_empty())
            .unwrap_or(h);
        return (
            body_start_line + last,
            format!("insert after line {}: - {link}", body_start_line + last),
        );
    }
    (0, format!("append at the end: {section}\\n\\n- {link}"))
}

/// Headings (lowercase) that more than 5% of notes have: template headings like
/// `Related` or `How it works` say nothing about what a note is about.
fn common_headings(vault: &Vault) -> BTreeSet<String> {
    let mut count: HashMap<String, usize> = HashMap::new();
    for n in &vault.notes {
        let mut seen = BTreeSet::new();
        for line in n.text[n.parsed.body_start..].lines() {
            let l = line.trim_start();
            if l.starts_with("## ") || l.starts_with("### ") {
                seen.insert(l.trim_start_matches('#').trim().to_lowercase());
            }
        }
        for h in seen {
            *count.entry(h).or_default() += 1;
        }
    }
    let limit = (vault.notes.len() / 20).max(2);
    count
        .into_iter()
        .filter(|(_, c)| *c > limit)
        .map(|(h, _)| h)
        .collect()
}

fn frontmatter_values(note: &Note) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let Some(Value::Object(map)) = &note.parsed.frontmatter.value else {
        return out;
    };
    for (k, v) in map {
        if SKIP_KEYS.contains(&k.as_str()) {
            continue;
        }
        let mut push = |s: &str| {
            let s = s.trim().to_lowercase();
            if !s.is_empty() && !s.contains("[[") && s.len() <= 80 {
                out.insert((k.clone(), s));
            }
        };
        match v {
            Value::String(s) => push(s),
            Value::Array(items) => items.iter().filter_map(Value::as_str).for_each(&mut push),
            _ => {}
        }
    }
    for t in note.tags() {
        out.insert(("tags".into(), t));
    }
    out
}

#[derive(Default)]
struct Signals {
    mention: f64,
    links_here: f64,
    /// This note links to it, and it does not link back.
    no_link_back: f64,
    semantic: f64,
    lexical: f64,
    neighbors: f64,
    frontmatter: f64,
    reasons: Vec<String>,
    mention_edit: Option<(usize, String)>,
}

/// Link candidates for note `start` scoring at least `min_score`, best first (at most
/// `limit`). One strong signal (a mention, a link here) scores 2-3; a weak text match alone
/// scores under 0.5. For many notes, build one [`Suggester`] and reuse it.
#[must_use]
pub fn suggest(
    vault: &Vault,
    start: usize,
    index: Option<&Index>,
    limit: usize,
    min_score: f64,
) -> Vec<Suggestion> {
    Suggester::new(vault, index).suggest(start, limit, min_score)
}

/// What `suggest` needs from the whole vault, computed once: the mention matcher, template
/// headings, the hub-free link graph, frontmatter values and their document frequency, and
/// note vectors. Suggestions for many notes (`suggest --changed`) reuse it.
pub struct Suggester<'a> {
    vault: &'a Vault,
    index: Option<&'a Index>,
    matcher: mentions::Matcher,
    common: BTreeSet<String>,
    /// Undirected links between non-hub notes.
    adj: Vec<BTreeSet<usize>>,
    frontmatter: Vec<BTreeSet<(String, String)>>,
    /// Notes holding each frontmatter value.
    df: HashMap<(String, String), usize>,
    /// Mean vector per note (index order), when there is an index.
    vectors: Vec<Option<Vec<f32>>>,
}

impl<'a> Suggester<'a> {
    #[must_use]
    pub fn new(vault: &'a Vault, index: Option<&'a Index>) -> Self {
        use rayon::prelude::*;
        let cfg = &vault.config;
        let out_links = vault.outbound_all();
        let mut adj: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); vault.notes.len()];
        for (i, outs) in out_links.iter().enumerate() {
            if cfg.is_hub(&vault.notes[i].id) {
                continue;
            }
            for &j in outs {
                if !cfg.is_hub(&vault.notes[j].id) {
                    adj[i].insert(j);
                    adj[j].insert(i);
                }
            }
        }
        let frontmatter: Vec<BTreeSet<(String, String)>> =
            vault.notes.par_iter().map(frontmatter_values).collect();
        let mut df: HashMap<(String, String), usize> = HashMap::new();
        for vals in &frontmatter {
            for kv in vals {
                *df.entry(kv.clone()).or_default() += 1;
            }
        }
        let vectors = match index {
            Some(idx) => vault
                .notes
                .par_iter()
                .map(|n| idx.note_vector(&n.id))
                .collect(),
            None => Vec::new(),
        };
        Self {
            vault,
            index,
            matcher: mentions::Matcher::new(vault),
            common: common_headings(vault),
            adj,
            frontmatter,
            df,
            vectors,
        }
    }

    /// Suggestions for several notes, in the order given (computed in parallel).
    #[must_use]
    pub fn suggest_many(&self, starts: &[usize], limit: usize, min_score: f64) -> Vec<Suggestion> {
        use rayon::prelude::*;
        let per: Vec<Vec<Suggestion>> = starts
            .par_iter()
            .map(|&s| self.suggest(s, limit, min_score))
            .collect();
        per.into_iter().flatten().collect()
    }

    /// Link candidates for note `start`: see [`suggest`].
    #[must_use]
    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    pub fn suggest(&self, start: usize, limit: usize, min_score: f64) -> Vec<Suggestion> {
        let vault = self.vault;
        let cfg = &vault.config;
        let note = &vault.notes[start];
        let out_links = vault.outbound_all();
        let linked: &BTreeSet<usize> = &out_links[start];
        let eligible = |i: usize| i != start && !cfg.is_hub(&vault.notes[i].id);
        let mut sig: BTreeMap<usize, Signals> = BTreeMap::new();

        // 1. mentions in this note
        for m in self.matcher.in_note(vault, start) {
            let Some(t) = vault.index_of(&m.target) else {
                continue;
            };
            if !eligible(t) {
                continue;
            }
            let s = sig.entry(t).or_default();
            s.mention = 1.0;
            s.reasons
                .push(format!("mentions \"{}\" (line {})", m.term, m.line));
            let link = link_text(vault, &vault.notes[t]);
            let inner = link.trim_start_matches("[[").trim_end_matches("]]");
            // Inside a table row the alias pipe must be escaped, or it splits the cell.
            let in_table =
                m.context.trim_start().starts_with('|') || m.context.matches('|').count() >= 2;
            let pipe = if in_table { "\\|" } else { "|" };
            s.mention_edit = Some((
                m.line,
                format!(
                    "line {}: replace \"{}\" with [[{inner}{pipe}{}]]",
                    m.line, m.term, m.term
                ),
            ));
        }

        // 2. notes that link here without a link back
        // (A log, like a session note, links what it touched; that does not ask for a link
        // back.)
        for (i, outs) in out_links.iter().enumerate() {
            if outs.contains(&start)
                && !linked.contains(&i)
                && eligible(i)
                && !cfg.is_no_backlink_from(&vault.notes[i].id)
            {
                let s = sig.entry(i).or_default();
                s.links_here = 1.0;
                s.reasons.push("links here".into());
            }
        }

        // 2b. notes this one links to that do not link back
        for &t in linked {
            if eligible(t) && !out_links[t].contains(&start) {
                let s = sig.entry(t).or_default();
                s.no_link_back = 1.0;
                s.reasons.push("linked from here; no link back".into());
            }
        }

        // 3a. similar meaning
        if self.index.is_some()
            && let Some(me) = &self.vectors[start]
        {
            // Note-to-note similarity runs high (most notes in one vault share vocabulary), so
            // only clearly similar notes count: linear from NOTE_SIMILARITY_LOW (0) to +0.2 (1).
            for (i, v) in self.vectors.iter().enumerate() {
                let Some(v) = v else { continue };
                if !eligible(i) {
                    continue;
                }
                let sim = vectors::dot(me, v);
                let rel = ((sim - NOTE_SIMILARITY_LOW) / 0.2).clamp(0.0, 1.0);
                if rel > 0.0 {
                    let s = sig.entry(i).or_default();
                    s.semantic = f64::from(rel);
                    if rel >= 0.25 {
                        s.reasons.push(format!("similar meaning ({sim:.2})"));
                    }
                }
            }
        }

        // 3b. similar text: the note's title, aliases, and headings as a query
        let mut query = crate::catalog::title(note, &cfg.generic_names)
            .unwrap_or_else(|| note.name(&cfg.generic_names).to_string());
        for a in note.parsed.frontmatter.aliases() {
            query.push(' ');
            query.push_str(&a);
        }
        for line in note.text[note.parsed.body_start..].lines() {
            let l = line.trim_start();
            if l.starts_with("## ") || l.starts_with("### ") {
                let h = l.trim_start_matches('#').trim();
                if !self.common.contains(&h.to_lowercase()) {
                    query.push(' ');
                    query.push_str(h);
                }
            }
        }
        // Quotes in a title or heading must not turn into phrase rules.
        let query = query.replace('"', " ");
        let hits = search::search(
            vault,
            &query,
            &Filter::default(),
            &search::Options {
                include_hubs: false,
                limit: 25,
                semantic: None,
                lexical: true,
            },
        );
        let mut rank = 0usize;
        for h in &hits {
            let Some(i) = vault.index_of(&h.id) else {
                continue;
            };
            if !eligible(i) {
                continue;
            }
            rank += 1;
            let s = sig.entry(i).or_default();
            s.lexical = 1.0 / rank as f64;
            if rank <= 5 {
                s.reasons.push(format!("similar text (#{rank})"));
            }
        }

        // 4. shared neighbors (Adamic-Adar), hubs left out
        let adj = &self.adj;
        let mut shared: HashMap<usize, (f64, Vec<usize>)> = HashMap::new();
        for &z in &adj[start] {
            let degree = adj[z].len();
            if degree < 2 {
                continue;
            }
            let w = 1.0 / (degree as f64).ln();
            for &t in &adj[z] {
                if eligible(t) {
                    let e = shared.entry(t).or_default();
                    e.0 += w;
                    e.1.push(z);
                }
            }
        }
        for (t, (score, via)) in shared {
            // One shared neighbor is common and says little; two or more is a pattern.
            if via.len() < 2 {
                continue;
            }
            let s = sig.entry(t).or_default();
            s.neighbors = (score / 2.0).min(1.0);
            let names: Vec<&str> = via
                .iter()
                .take(3)
                .map(|&z| vault.notes[z].id.as_str())
                .collect();
            s.reasons.push(format!(
                "{} shared link(s): {}",
                via.len(),
                names.join(", ")
            ));
        }

        // 5. rare shared frontmatter values
        let mine = &self.frontmatter[start];
        if !mine.is_empty() {
            let df = &self.df;
            let n = vault.notes.len() as f64;
            for (t, vals) in self.frontmatter.iter().enumerate() {
                if !eligible(t) {
                    continue;
                }
                let common: Vec<&(String, String)> = vals
                    .intersection(mine)
                    .filter(|kv| df[*kv] <= 2 || (df[*kv] as f64) / n <= COMMON_VALUE)
                    .collect();
                if common.is_empty() {
                    continue;
                }
                let score: f64 = common.iter().map(|kv| (n / df[*kv] as f64).ln()).sum();
                let s = sig.entry(t).or_default();
                // Absolute: one value only these two notes share (~ln(n/2)) counts fully.
                s.frontmatter = (score / (n / 2.0).ln().max(1.0)).min(1.0);
                let shown: Vec<String> = common
                    .iter()
                    .take(3)
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect();
                s.reasons.push(format!("same {}", shown.join(", ")));
            }
        }

        // Normalize per signal, weight, rank.
        let max = |f: fn(&Signals) -> f64| sig.values().map(f).fold(0.0f64, f64::max).max(1e-9);
        let m_lex = max(|s| s.lexical);
        let mut ranked: Vec<(f64, usize, Signals)> = sig
            .into_iter()
            .map(|(t, s)| {
                let score = 3.0 * s.mention
                    + 2.0 * s.links_here
                    + 1.0 * s.no_link_back
                    + 1.5 * s.semantic
                    + 1.0 * s.lexical / m_lex
                    + 1.0 * s.neighbors
                    + 1.0 * s.frontmatter;
                ((score * 100.0).round() / 100.0, t, s)
            })
            // Back-links only where the other note does not already link here.
            .filter(|(_, t, _)| !(linked.contains(t) && out_links[*t].contains(&start)))
            .collect();
        let id_of = |t: usize, linked_t: bool| {
            if linked_t {
                note.id.as_str()
            } else {
                vault.notes[t].id.as_str()
            }
        };
        ranked.sort_by(|a, b| {
            b.0.total_cmp(&a.0).then_with(|| {
                id_of(a.1, linked.contains(&a.1)).cmp(id_of(b.1, linked.contains(&b.1)))
            })
        });
        ranked.retain(|(score, _, _)| *score >= min_score);
        ranked.truncate(limit);
        ranked
            .into_iter()
            .map(|(score, t, s)| {
                let target = &vault.notes[t];
                let (action, editor, (line, edit)) = if linked.contains(&t) {
                    // Already linked from here: suggest the link back, edited in the target.
                    ("backlink", target, add_link_edit(vault, target, note))
                } else if let Some(me) = s.mention_edit.clone() {
                    ("mention", note, me)
                } else {
                    ("link", note, add_link_edit(vault, note, target))
                };
                let target_id = if action == "backlink" {
                    note.id.clone()
                } else {
                    target.id.clone()
                };
                Suggestion {
                    note: editor.id.clone(),
                    action,
                    target: target_id,
                    score,
                    reasons: s.reasons,
                    line,
                    edit,
                }
            })
            .collect()
    }
}
