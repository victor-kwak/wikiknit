//! `search <query>`: ranked full-text search over the vault (BM25F), computed per run.
//!
//! Each note has fields with their own weight: title (H1 + name), aliases, headings,
//! frontmatter values, body. Terms are Unicode words, lowercased and stemmed; a word like
//! `snapshot.sh` or `id_ed25519` is also split into its parts. Runs of Chinese, Japanese,
//! Korean, or Thai characters are indexed as single characters and overlapping pairs.
//! Links count as their display text (`[[Projects/x/overview]]` -> `x`), not their path.
//! A quoted phrase must appear as written, within one line. A query that is exactly a note's
//! name, alias, or title ranks that note first; notes many others link to get a small boost.
//! Term statistics come from the whole vault, so scores do not change with filters.

use std::collections::{BTreeSet, HashMap};

use rayon::prelude::*;
use rust_stemmers::{Algorithm, Stemmer};
use serde::Serialize;
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::catalog;
use crate::filter::Filter;
use crate::text::{self, Fence};
use crate::vault::{Note, Vault};

const K1: f64 = 1.2;
const SNIPPET_LEN: usize = 160;
/// Characters that join parts of an identifier inside one Unicode word.
const JOINERS: &[char] = &['.', '_', ':', '@'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Aliases,
    Headings,
    Frontmatter,
    Body,
}

const FIELDS: [Field; 5] = [
    Field::Title,
    Field::Aliases,
    Field::Headings,
    Field::Frontmatter,
    Field::Body,
];

impl Field {
    fn weight(self) -> f64 {
        match self {
            Field::Title | Field::Aliases => 4.0,
            Field::Headings => 2.0,
            Field::Frontmatter => 1.5,
            Field::Body => 1.0,
        }
    }

    /// Length normalization strength (BM25 `b`).
    fn b(self) -> f64 {
        match self {
            Field::Title => 0.3,
            Field::Aliases => 0.0,
            Field::Headings | Field::Frontmatter => 0.5,
            Field::Body => 0.75,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Aliases => "aliases",
            Field::Headings => "headings",
            Field::Frontmatter => "frontmatter",
            Field::Body => "body",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub score: f64,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Fields the query matched in, best first.
    pub matched: Vec<&'static str>,
    /// Line of the snippet (0 when the match is only in title/aliases/frontmatter and the
    /// snippet is the note's summary).
    pub line: usize,
    pub snippet: String,
}

/// Splits text into search terms.
pub struct Analyzer {
    stemmer: Option<Stemmer>,
    cache: HashMap<String, String>,
}

fn algorithm(name: &str) -> Option<Algorithm> {
    Some(match name {
        "arabic" => Algorithm::Arabic,
        "danish" => Algorithm::Danish,
        "dutch" => Algorithm::Dutch,
        "english" => Algorithm::English,
        "finnish" => Algorithm::Finnish,
        "french" => Algorithm::French,
        "german" => Algorithm::German,
        "greek" => Algorithm::Greek,
        "hungarian" => Algorithm::Hungarian,
        "italian" => Algorithm::Italian,
        "norwegian" => Algorithm::Norwegian,
        "portuguese" => Algorithm::Portuguese,
        "romanian" => Algorithm::Romanian,
        "russian" => Algorithm::Russian,
        "spanish" => Algorithm::Spanish,
        "swedish" => Algorithm::Swedish,
        "tamil" => Algorithm::Tamil,
        "turkish" => Algorithm::Turkish,
        _ => return None,
    })
}

fn is_unspaced(c: char) -> bool {
    matches!(c as u32,
        0x0E00..=0x0EFF | 0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x31F0..=0x31FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF
        | 0x20000..=0x2FA1F)
}

impl Analyzer {
    #[must_use]
    pub fn new(stemmer: &str) -> Self {
        Self {
            stemmer: algorithm(stemmer).map(Stemmer::create),
            cache: HashMap::new(),
        }
    }

    fn stem(&mut self, lower: String) -> String {
        if let Some(t) = self.cache.get(&lower) {
            return t.clone();
        }
        let term = match &self.stemmer {
            Some(s) => s.stem(&lower).into_owned(),
            None => lower.clone(),
        };
        self.cache.insert(lower, term.clone());
        term
    }

    /// Terms of `text`, in order, each with the byte offset of the word it came from.
    pub fn terms_at(&mut self, text: &str) -> Vec<(usize, String)> {
        self.tokens(text, true)
            .into_iter()
            .map(|t| (t.at, t.term))
            .collect()
    }

    /// Tokens of `text`. With `split_parts` (documents), an identifier like `snapshot.sh` is
    /// indexed whole and by its parts, the parts at the same position as the whole; queries
    /// keep identifiers whole, so `id_ed25519` does not match every `id`.
    fn tokens(&mut self, text: &str, split_parts: bool) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::new();
        // Consecutive unspaced-script chars (with offsets), across the word breaks the
        // segmenter chooses: `東京` + `タワー` -> 東 東京 京 京タ タ タワ ワ ワー ー.
        let mut run: Vec<(usize, char)> = Vec::new();
        let flush = |run: &mut Vec<(usize, char)>, out: &mut Vec<Token>| {
            for (i, &(at, c)) in run.iter().enumerate() {
                out.push(Token::new(at, c.to_string(), true));
                if let Some(&(_, next)) = run.get(i + 1) {
                    out.push(Token::new(at, format!("{c}{next}"), true));
                }
            }
            run.clear();
        };
        let mut run_end = usize::MAX;
        for (start, word) in text.unicode_word_indices() {
            if word.chars().all(is_unspaced) {
                if start != run_end {
                    flush(&mut run, &mut out);
                }
                run.extend(word.char_indices().map(|(i, c)| (start + i, c)));
                run_end = start + word.len();
                continue;
            }
            flush(&mut run, &mut out);
            run_end = usize::MAX;
            let lower = word.to_lowercase();
            if word.contains(JOINERS) {
                // The whole identifier, then (documents) its parts at the same position.
                out.push(Token::new(start, lower.clone(), true));
                if split_parts {
                    for part in lower.split(JOINERS).filter(|p| !p.is_empty()) {
                        let term = self.stem(part.to_string());
                        out.push(Token::new(start, term, false));
                    }
                    // Leading sub-identifiers, so a query for `id_ed25519` also finds
                    // `id_ed25519_vectorpoint`: every prefix that ends before a joiner.
                    for (i, c) in lower.char_indices() {
                        if i > 0 && JOINERS.contains(&c) {
                            let prefix = &lower[..i];
                            if prefix.contains(JOINERS) {
                                out.push(Token::new(start, prefix.to_string(), false));
                            }
                        }
                    }
                }
            } else {
                let term = self.stem(lower);
                out.push(Token::new(start, term, true));
            }
        }
        flush(&mut run, &mut out);
        out
    }

    /// Terms of `text` as a document, in order.
    pub fn terms(&mut self, text: &str) -> Vec<String> {
        self.terms_at(text).into_iter().map(|(_, t)| t).collect()
    }

    /// Terms of `text` as a query (identifiers kept whole).
    pub fn query_terms(&mut self, text: &str) -> Vec<String> {
        self.tokens(text, false)
            .into_iter()
            .map(|t| t.term)
            .collect()
    }
}

struct Token {
    /// Byte offset of the word in the text.
    at: usize,
    term: String,
    /// Starts a new position (false for an identifier's parts).
    advance: bool,
}

impl Token {
    fn new(at: usize, term: String, advance: bool) -> Self {
        Self { at, term, advance }
    }
}

/// What scoring needs from one note: field lengths, and per query term its count in each
/// field. Only query terms are kept, so memory stays small on large vaults.
struct Stats {
    lens: [usize; 5],
    /// counts[query term][field]
    counts: Vec<[u32; 5]>,
    /// Every quoted phrase appears in some field.
    phrases_ok: bool,
}

fn frontmatter_text(note: &Note, generic: &[String]) -> String {
    fn walk(v: &Value, generic: &[String], out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(&text::links_to_text(s, generic));
                out.push('\n');
            }
            Value::Number(n) => {
                out.push_str(&n.to_string());
                out.push('\n');
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, generic, out)),
            Value::Object(map) => map.values().for_each(|i| walk(i, generic, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    if let Some(Value::Object(map)) = &note.parsed.frontmatter.value {
        for (k, v) in map {
            if k != "aliases" && k != "alias" {
                walk(v, generic, &mut out);
            }
        }
    }
    out
}

/// The text of each field: title (name, stem, H1), aliases, headings, frontmatter, body.
/// Lines stay separate, so phrases do not match across them.
fn field_texts(note: &Note, generic: &[String]) -> [String; 5] {
    let body = &note.text[note.parsed.body_start..];
    let mut headings = String::new();
    // The name; plus the file stem when it differs and is not a generic word like `overview`.
    let mut title = note.name(generic).to_string();
    let stem = note.stem();
    if stem != title && !generic.iter().any(|g| g.eq_ignore_ascii_case(stem)) {
        title.push('\n');
        title.push_str(stem);
    }
    let mut body_text = String::new();
    let mut fence = Fence::default();
    let mut h1_seen = false;
    for line in body.lines() {
        let in_code = fence.line(line);
        let l = line.trim_start();
        if !in_code && l.starts_with('#') && l.trim_start_matches('#').starts_with(' ') {
            let h = text::links_to_text(l.trim_start_matches('#').trim(), generic);
            if !h1_seen && l.starts_with("# ") {
                h1_seen = true;
                title.push('\n');
                title.push_str(&h);
            } else {
                headings.push_str(&h);
                headings.push('\n');
            }
            continue;
        }
        body_text.push_str(&text::links_to_text(line, generic));
        body_text.push('\n');
    }
    [
        title,
        note.parsed.frontmatter.aliases().join("\n"),
        headings,
        frontmatter_text(note, generic),
        body_text,
    ]
}

/// The query, as term indexes for fast lookup.
struct Query {
    terms: Vec<String>,
    index: HashMap<String, usize>,
    /// Each phrase as a sequence of term indexes.
    phrases: Vec<Vec<usize>>,
}

/// Split a query into loose terms and quoted phrases.
fn parse_query(query: &str, an: &mut Analyzer) -> Query {
    let mut terms: Vec<String> = Vec::new();
    let mut phrase_terms = Vec::new();
    for (i, part) in query.split('"').enumerate() {
        let t = an.query_terms(part);
        if i % 2 == 1 && t.len() > 1 {
            phrase_terms.push(t.clone());
        }
        terms.extend(t);
    }
    let mut seen = BTreeSet::new();
    terms.retain(|t| seen.insert(t.clone()));
    let index: HashMap<String, usize> = terms
        .iter()
        .enumerate()
        .map(|(i, t)| (t.clone(), i))
        .collect();
    let phrases = phrase_terms
        .iter()
        .map(|p| p.iter().map(|t| index[t]).collect())
        .collect();
    Query {
        terms,
        index,
        phrases,
    }
}

fn build_stats(note: &Note, an: &mut Analyzer, q: &Query, generic: &[String]) -> Stats {
    let mut stats = Stats {
        lens: [0; 5],
        counts: vec![[0; 5]; q.terms.len()],
        phrases_ok: q.phrases.is_empty(),
    };
    let mut found = vec![false; q.phrases.len()];
    for (f, field_text) in field_texts(note, generic).iter().enumerate() {
        // query terms at each position (an identifier and its parts share one position)
        let mut at: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut pos = 0usize;
        for line in field_text.lines() {
            let mut first = true;
            for tok in an.tokens(line, true) {
                if tok.advance && !first {
                    pos += 1;
                }
                first = false;
                stats.lens[f] += 1;
                if let Some(&qi) = q.index.get(&tok.term) {
                    stats.counts[qi][f] += 1;
                    if !q.phrases.is_empty() {
                        at.entry(pos).or_default().push(qi);
                    }
                }
            }
            pos += 2; // next line: a gap, so phrases do not span lines
        }
        for (p, phrase) in q.phrases.iter().enumerate() {
            if !found[p] {
                found[p] =
                    at.iter().any(|(&start, here)| {
                        here.contains(&phrase[0])
                            && phrase.iter().enumerate().skip(1).all(|(k, want)| {
                                at.get(&(start + k)).is_some_and(|v| v.contains(want))
                            })
                    });
            }
        }
    }
    if !q.phrases.is_empty() {
        stats.phrases_ok = found.iter().all(|&f| f);
    }
    stats
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Search the notes that pass `filter` (hubs only with `include_hubs`); best `limit` hits.
#[must_use]
#[allow(clippy::cast_precision_loss)] // counts far below 2^52
pub fn search(
    vault: &Vault,
    query: &str,
    filter: &Filter,
    include_hubs: bool,
    limit: usize,
) -> Vec<Hit> {
    let cfg = &vault.config;
    let mut an = Analyzer::new(&cfg.stemmer);
    let q = parse_query(query, &mut an);
    if q.terms.is_empty() {
        return Vec::new();
    }
    // Statistics over every searchable note; the filter only decides which ones are shown.
    let corpus: Vec<usize> = vault
        .notes
        .iter()
        .enumerate()
        .filter(|(_, n)| include_hubs || !cfg.is_hub(&n.id))
        .map(|(i, _)| i)
        .collect();
    let docs: Vec<Stats> = corpus
        .par_iter()
        .map_init(
            || Analyzer::new(&cfg.stemmer),
            |an, &i| build_stats(&vault.notes[i], an, &q, &cfg.generic_names),
        )
        .collect();
    let n_docs = docs.len() as f64;
    let mut avg = [0.0f64; 5];
    for d in &docs {
        for (f, a) in avg.iter_mut().enumerate() {
            *a += d.lens[f] as f64;
        }
    }
    for a in &mut avg {
        *a = (*a / n_docs.max(1.0)).max(1.0);
    }
    let mut df = vec![0usize; q.terms.len()];
    for d in &docs {
        for (qi, c) in d.counts.iter().enumerate() {
            if c.iter().any(|&x| x > 0) {
                df[qi] += 1;
            }
        }
    }
    let inbound = vault.inbound_counts();
    let query_norm = normalize(query.trim_matches('"'));
    let mut hits: Vec<(f64, usize, Vec<&'static str>)> = Vec::new();
    for (pos, d) in docs.iter().enumerate() {
        let i = corpus[pos];
        let note = &vault.notes[i];
        if !d.phrases_ok || !filter.matches(vault, note) {
            continue;
        }
        let mut score = 0.0;
        let mut matched: Vec<(f64, &'static str)> = Vec::new();
        for (qi, &dft) in df.iter().enumerate() {
            if dft == 0 {
                continue;
            }
            let idf = (1.0 + (n_docs - dft as f64 + 0.5) / (dft as f64 + 0.5)).ln();
            let mut tf = 0.0;
            for (f, field) in FIELDS.iter().enumerate() {
                let count = f64::from(d.counts[qi][f]);
                if count == 0.0 {
                    continue;
                }
                let len = d.lens[f] as f64;
                let part = field.weight() * count / (1.0 - field.b() + field.b() * len / avg[f]);
                tf += part;
                match matched.iter_mut().find(|(_, n)| *n == field.name()) {
                    Some(m) => m.0 += part,
                    None => matched.push((part, field.name())),
                }
            }
            score += idf * tf / (K1 + tf);
        }
        if score <= 0.0 {
            continue;
        }
        let exact = normalize(note.name(&cfg.generic_names)) == query_norm
            || note
                .parsed
                .frontmatter
                .aliases()
                .iter()
                .any(|a| normalize(a) == query_norm)
            || catalog::title(note, &cfg.generic_names)
                .is_some_and(|t| normalize(&t) == query_norm);
        if exact && !query_norm.is_empty() {
            score += 10.0;
        }
        score *= 1.0 + 0.05 * (1.0 + inbound[i] as f64).ln();
        matched.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.push((score, i, matched.into_iter().map(|(_, n)| n).collect()));
    }
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.truncate(limit);
    hits.into_iter()
        .map(|(score, i, matched)| {
            let note = &vault.notes[i];
            let (line, snippet) = best_line(note, &q, &mut an)
                .unwrap_or_else(|| (0, catalog::summary(note, &cfg.generic_names)));
            Hit {
                id: note.id.clone(),
                score: (score * 100.0).round() / 100.0,
                kind: note
                    .parsed
                    .frontmatter
                    .str_field("type")
                    .map(str::to_string),
                matched,
                line,
                snippet,
            }
        })
        .collect()
}

/// The body line with the most distinct query terms (first on ties), as a window around the
/// first matching word.
fn best_line(note: &Note, q: &Query, an: &mut Analyzer) -> Option<(usize, String)> {
    let start_line = note.line_of(note.parsed.body_start);
    let body = &note.text[note.parsed.body_start..];
    // (distinct hits, file line, line text, focus byte)
    let mut best: Option<(usize, usize, &str, usize)> = None;
    for (k, line) in body.lines().enumerate() {
        if line.trim().is_empty() || line.trim_start().starts_with("# ") {
            continue;
        }
        let terms = an.terms_at(line);
        let mut seen = BTreeSet::new();
        let mut focus = None;
        for (at, t) in &terms {
            if let Some(&qi) = q.index.get(t) {
                seen.insert(qi);
                focus.get_or_insert(*at);
            }
        }
        if let Some(focus) = focus
            && best.is_none_or(|(h, _, _, _)| seen.len() > h)
        {
            best = Some((seen.len(), start_line + k, line, focus));
        }
    }
    best.map(|(_, line, text, focus)| (line, text::window(text, focus, SNIPPET_LEN)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzer_stems_splits_identifiers_and_cjk() {
        let mut an = Analyzer::new("english");
        assert_eq!(
            an.terms("Deploys running Cloudflare-tunnel"),
            ["deploy", "run", "cloudflar", "tunnel"]
        );
        assert_eq!(
            an.terms("snapshot.sh id_ed25519"),
            [
                "snapshot.sh",
                "snapshot",
                "sh",
                "id_ed25519",
                "id",
                "ed25519"
            ]
        );
        assert_eq!(
            an.terms("東京タワー"),
            ["東", "東京", "京", "京タ", "タ", "タワ", "ワ", "ワー", "ー"]
        );
        let mut none = Analyzer::new("none");
        assert_eq!(none.terms("Deploys"), ["deploys"]);
    }

    #[test]
    fn queries_keep_identifiers_whole() {
        let mut an = Analyzer::new("english");
        assert_eq!(an.query_terms("id_ed25519 keys"), ["id_ed25519", "key"]);
        // Documents also index leading sub-identifiers, so that query finds this one.
        assert!(
            an.terms("id_ed25519_vectorpoint")
                .contains(&"id_ed25519".to_string())
        );
    }

    #[test]
    fn phrases_are_sequences() {
        let mut an = Analyzer::new("english");
        let q = parse_query("\"ssh key\" github", &mut an);
        assert_eq!(q.terms, ["ssh", "key", "github"]);
        assert_eq!(q.phrases, vec![vec![0, 1]]);
    }
}
