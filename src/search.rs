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

use std::collections::{BTreeMap, BTreeSet, HashMap};

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
/// Reciprocal rank fusion constant (the usual 60).
const RRF_K: f64 = 60.0;
/// How many semantic results join the fusion.
const SEMANTIC_DEPTH: usize = 100;
/// Semantic results this high are reported as a `semantic` match.
const SEMANTIC_MATCHED: usize = 10;

const SNIPPET_LEN: usize = 160;
/// Characters that join parts of an identifier inside one Unicode word.
const JOINERS: &[char] = &['.', '_', ':', '@'];
/// Longer identifiers are indexed whole only.
const MAX_IDENT_LEN: usize = 128;
/// At most this many parts, and this many prefixes, per identifier.
const MAX_PARTS: usize = 8;

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

/// How to search.
#[derive(Debug, Clone, Copy)]
pub struct Options<'a> {
    /// Also search hub notes.
    pub include_hubs: bool,
    pub limit: usize,
    /// Notes ranked by meaning, to fuse with the lexical ranking (hybrid search).
    pub semantic: Option<&'a [Meaning]>,
    /// Use the lexical (BM25) ranking; off = semantic only.
    pub lexical: bool,
}

/// A note ranked by meaning (from the semantic index): best chunk similarity and its line.
#[derive(Debug, Clone, Copy)]
pub struct Meaning {
    pub note: usize,
    pub similarity: f32,
    pub line: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: String,
    pub score: f64,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Cosine similarity of the best chunk, when the note matched by meaning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<f64>,
    /// Fields the query matched in, best first.
    pub matched: Vec<&'static str>,
    /// Share of the query's weight (by term rarity) that the note's words match, 0-1: a hit
    /// on one common word of a long query covers little. 0 for a match by meaning only.
    pub coverage: f64,
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
                // Bounded: a long word full of joiners (`a.a.a.a...`) must not cost quadratic
                // time and memory, so long identifiers are indexed whole only, and at most
                // MAX_PARTS parts and prefixes are made per word.
                if split_parts && lower.len() <= MAX_IDENT_LEN {
                    for part in lower
                        .split(JOINERS)
                        .filter(|p| !p.is_empty())
                        .take(MAX_PARTS)
                    {
                        let term = self.stem(part.to_string());
                        out.push(Token::new(start, term, false));
                    }
                    // Leading sub-identifiers, so a query for `id_ed25519` also finds
                    // `id_ed25519_vectorpoint`: prefixes that end before a joiner.
                    let ends = lower
                        .char_indices()
                        .filter(|&(i, c)| i > 0 && JOINERS.contains(&c))
                        .map(|(i, _)| i)
                        .skip(1) // the first prefix has no joiner in it: it is a part
                        .take(MAX_PARTS);
                    for i in ends {
                        out.push(Token::new(start, lower[..i].to_string(), false));
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

/// Every note analyzed once: field lengths and each term's count per field. For running many
/// queries over one vault (`suggest`), where analyzing the vault per query would dominate.
pub struct Corpus {
    docs: Vec<DocTerms>,
}

struct DocTerms {
    lens: [usize; 5],
    counts: HashMap<String, [u32; 5]>,
}

impl Corpus {
    #[must_use]
    pub fn new(vault: &Vault) -> Self {
        let cfg = &vault.config;
        let docs = vault
            .notes
            .par_iter()
            .map_init(
                || Analyzer::new(&cfg.stemmer),
                |an, note| {
                    let mut d = DocTerms {
                        lens: [0; 5],
                        counts: HashMap::new(),
                    };
                    for (f, field_text) in field_texts(note, &cfg.generic_names).iter().enumerate()
                    {
                        for line in field_text.lines() {
                            for tok in an.tokens(line, true) {
                                d.lens[f] += 1;
                                d.counts.entry(tok.term).or_default()[f] += 1;
                            }
                        }
                    }
                    d
                },
            )
            .collect();
        Self { docs }
    }

    /// The same [`Stats`] as [`build_stats`] gives, for a query without phrases.
    fn stats(&self, i: usize, q: &Query) -> Stats {
        let d = &self.docs[i];
        Stats {
            lens: d.lens,
            counts: q
                .terms
                .iter()
                .map(|t| d.counts.get(t).copied().unwrap_or_default())
                .collect(),
            phrases_ok: true,
        }
    }
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
pub fn search(vault: &Vault, query: &str, filter: &Filter, opts: &Options<'_>) -> Vec<Hit> {
    search_impl(vault, None, query, filter, opts)
}

/// [`search`] over a [`Corpus`] built once for the vault (same results, no re-analysis).
#[must_use]
pub fn search_in(
    vault: &Vault,
    corpus: &Corpus,
    query: &str,
    filter: &Filter,
    opts: &Options<'_>,
) -> Vec<Hit> {
    search_impl(vault, Some(corpus), query, filter, opts)
}

#[allow(clippy::cast_precision_loss, clippy::too_many_lines)] // counts far below 2^52
fn search_impl(
    vault: &Vault,
    prepared: Option<&Corpus>,
    query: &str,
    filter: &Filter,
    opts: &Options<'_>,
) -> Vec<Hit> {
    let Options {
        include_hubs,
        limit,
        semantic,
        lexical,
    } = *opts;
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
    let docs: Vec<Stats> = match prepared {
        // Phrases need word positions, which the corpus does not keep.
        Some(c) if q.phrases.is_empty() => corpus.iter().map(|&i| c.stats(i, &q)).collect(),
        _ => corpus
            .par_iter()
            .map_init(
                || Analyzer::new(&cfg.stemmer),
                |an, &i| build_stats(&vault.notes[i], an, &q, &cfg.generic_names),
            )
            .collect(),
    };
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
    // Term weights: rare terms weigh more; a term no note has weighs most (it is unmatched).
    let idf_q: Vec<f64> = df
        .iter()
        .map(|&d| (1.0 + (n_docs - d as f64 + 0.5) / (d as f64 + 0.5)).ln())
        .collect();
    let idf_total: f64 = idf_q.iter().sum::<f64>().max(1e-9);
    let mut coverage: BTreeMap<usize, f64> = BTreeMap::new();
    let inbound = vault.inbound_counts();
    let query_norm = normalize(query.trim_matches('"'));
    let mut hits: Vec<(f64, usize, Vec<&'static str>)> = Vec::new();
    // Notes allowed in results: pass the filter and contain every quoted phrase.
    let mut allowed = vec![false; vault.notes.len()];
    let mut exact_notes = vec![false; vault.notes.len()];
    for (pos, d) in docs.iter().enumerate() {
        let i = corpus[pos];
        let note = &vault.notes[i];
        if !d.phrases_ok || !filter.matches(vault, note) {
            continue;
        }
        allowed[i] = true;
        exact_notes[i] = !query_norm.is_empty()
            && (normalize(note.name(&cfg.generic_names)) == query_norm
                || note
                    .parsed
                    .frontmatter
                    .aliases()
                    .iter()
                    .any(|a| normalize(a) == query_norm)
                || catalog::title(note, &cfg.generic_names)
                    .is_some_and(|t| normalize(&t) == query_norm));
        let mut score = 0.0;
        let mut covered = 0.0;
        let mut matched: Vec<(f64, &'static str)> = Vec::new();
        for (qi, &dft) in df.iter().enumerate() {
            if dft == 0 {
                continue;
            }
            if d.counts[qi].iter().any(|&x| x > 0) {
                covered += idf_q[qi];
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
        coverage.insert(i, covered / idf_total);
        if exact_notes[i] {
            score += 10.0;
        }
        score *= 1.0 + 0.05 * (1.0 + inbound[i] as f64).ln();
        matched.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.push((score, i, matched.into_iter().map(|(_, n)| n).collect()));
    }
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    if !lexical {
        hits.clear();
    }

    // Hybrid: fuse the lexical and semantic rankings (reciprocal rank fusion).
    let mut semantic_line: BTreeMap<usize, usize> = BTreeMap::new();
    let mut semantic_sim: BTreeMap<usize, f32> = BTreeMap::new();
    if let Some(meaning) = semantic {
        let mut fused: BTreeMap<usize, (f64, Vec<&'static str>)> = BTreeMap::new();
        for (rank, (_, i, matched)) in hits.iter().enumerate() {
            let e = fused.entry(*i).or_insert((0.0, matched.clone()));
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
        // A semantic match must clear `min_similarity` and stand `semantic_margin` above the
        // median note for this query: similarity levels differ by vault and query, but a real
        // match stands out from the rest. (The median means little below 10 notes.)
        let mut sims: Vec<f32> = meaning.iter().map(|m| m.similarity).collect();
        sims.sort_by(f32::total_cmp);
        let floor = if sims.len() >= 10 {
            cfg.min_similarity
                .max(sims[sims.len() / 2] + cfg.semantic_margin)
        } else {
            cfg.min_similarity
        };
        // A query none of whose words occur in the vault (`zzzqqq`, a typo) has nothing to
        // ground its embedding, which then lands near some note by chance: it must clear one
        // more margin. (A cross-language match with a multilingual model scores well above.)
        let grounded = df.iter().any(|&d| d > 0);
        let floor = if grounded {
            floor
        } else {
            floor + cfg.semantic_margin
        };
        // The floor decides which notes may enter by meaning alone. Below it, meaning still
        // reorders the notes the words already found: weak evidence ranks, it does not add.
        let ranked = meaning
            .iter()
            .filter(|m| allowed[m.note])
            .take(SEMANTIC_DEPTH);
        for (rank, m) in ranked.enumerate() {
            let above = m.similarity >= floor;
            if !above && !fused.contains_key(&m.note) {
                continue;
            }
            let e = fused.entry(m.note).or_insert((0.0, Vec::new()));
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            if !above {
                continue;
            }
            if rank < SEMANTIC_MATCHED {
                e.1.push("semantic");
            }
            semantic_line.insert(m.note, m.line);
            semantic_sim.insert(m.note, m.similarity);
        }
        hits = fused
            .into_iter()
            .map(|(i, (s, matched))| {
                // Exact name/alias/title matches stay first; scores read as percent-ish.
                let s = s * 100.0 + if exact_notes[i] { 100.0 } else { 0.0 };
                (s, i, matched)
            })
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    hits.truncate(limit);
    hits.into_iter()
        .map(|(score, i, matched)| {
            let note = &vault.notes[i];
            let (line, snippet) = best_line(note, &q, &mut an, &idf_q)
                .or_else(|| {
                    semantic_line.get(&i).filter(|&&l| l > 0).map(|&l| {
                        let (_, text, _) = note.line_at(note.line_start(l));
                        (l, text::window(text, 0, SNIPPET_LEN))
                    })
                })
                .unwrap_or_else(|| (0, catalog::summary(note, &cfg.generic_names)));
            Hit {
                id: note.id.clone(),
                score: (score * 100.0).round() / 100.0,
                kind: note
                    .parsed
                    .frontmatter
                    .str_field("type")
                    .map(str::to_string),
                similarity: semantic_sim
                    .get(&i)
                    .map(|s| (f64::from(*s) * 1000.0).round() / 1000.0),
                coverage: coverage
                    .get(&i)
                    .map_or(0.0, |c| (c * 100.0).round() / 100.0),
                matched,
                line,
                snippet,
            }
        })
        .collect()
}

/// Body lines that match the query, scored: the summed weight of the distinct query terms
/// on the line, more when one is a label at the start of the line (`- **Deploy**: ...`), less
/// inside code blocks (logs, command output, snapshots). Returns (score, file line, text,
/// byte of the first matching word).
fn scored_lines<'a>(
    note: &'a Note,
    q: &Query,
    an: &mut Analyzer,
    weight: &[f64],
) -> Vec<(f64, usize, &'a str, usize)> {
    let start_line = note.line_of(note.parsed.body_start);
    let body = &note.text[note.parsed.body_start..];
    let mut fence = text::Fence::default();
    let mut out = Vec::new();
    for (k, line) in body.lines().enumerate() {
        let in_code = fence.line(line);
        if line.trim().is_empty() || line.trim_start().starts_with("# ") {
            continue;
        }
        let terms = an.terms_at(line);
        let mut seen = BTreeSet::new();
        let mut focus = None;
        let mut label = false;
        // The first few words of the line (after list and emphasis marks).
        let lead = line
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .len();
        let lead_start = line.len() - lead;
        let lead_end = line[lead_start..]
            .char_indices()
            .filter(|(_, c)| c.is_whitespace())
            .nth(2)
            .map_or(line.len(), |(b, _)| lead_start + b);
        for (at, t) in &terms {
            if let Some(&qi) = q.index.get(t) {
                seen.insert(qi);
                focus.get_or_insert(*at);
                label |= *at < lead_end;
            }
        }
        let Some(focus) = focus else { continue };
        let mut score: f64 = seen
            .iter()
            .map(|&qi| weight.get(qi).copied().unwrap_or(1.0))
            .sum();
        if label {
            score *= 1.25;
        }
        if in_code {
            score *= 0.5;
        }
        out.push((score, start_line + k, line, focus));
    }
    out
}

/// The best body line for the query (first on ties), as a window around the first matching
/// word.
fn best_line(note: &Note, q: &Query, an: &mut Analyzer, weight: &[f64]) -> Option<(usize, String)> {
    let mut best: Option<(f64, usize, &str, usize)> = None;
    for l in scored_lines(note, q, an, weight) {
        if best.is_none_or(|b| l.0 > b.0) {
            best = Some(l);
        }
    }
    best.map(|(_, line, text, focus)| (line, text::window(text, focus, SNIPPET_LEN)))
}

/// File lines of `note` that match `query`, with their scores (equal term weights).
#[must_use]
pub fn matching_lines(vault: &Vault, note: &Note, query: &str) -> Vec<(usize, f64)> {
    let mut an = Analyzer::new(&vault.config.stemmer);
    let q = parse_query(query, &mut an);
    scored_lines(note, &q, &mut an, &[])
        .into_iter()
        .map(|(s, l, _, _)| (l, s))
        .collect()
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

    fn vault_of(files: &[(&str, &str)]) -> Vault {
        let dir = tempfile::TempDir::new().unwrap();
        for (p, body) in files {
            std::fs::write(dir.path().join(p), body).unwrap();
        }
        let v = Vault::load(crate::Config::for_vault(dir.path().canonicalize().unwrap())).unwrap();
        std::mem::forget(dir);
        v
    }

    #[test]
    fn a_query_with_no_vault_words_needs_a_clearer_meaning_match() {
        let v = vault_of(&[
            ("x.md", "# x\nalpha\n"),
            ("y.md", "# y\ngamma\n"),
            ("z.md", "# z\ndelta\n"),
        ]);
        let idx = |id: &str| v.index_of(id).unwrap();
        // 0.58 clears min_similarity (0.5) but not min_similarity + semantic_margin (0.62).
        let meaning = [
            Meaning {
                note: idx("y"),
                similarity: 0.58,
                line: 2,
            },
            Meaning {
                note: idx("z"),
                similarity: 0.70,
                line: 2,
            },
        ];
        let opts = Options {
            include_hubs: false,
            limit: 10,
            semantic: Some(&meaning),
            lexical: true,
        };
        let ids = |q: &str| -> Vec<String> {
            search(&v, q, &Filter::default(), &opts)
                .into_iter()
                .map(|h| h.id)
                .collect()
        };
        // `zzzqqq` is in no note: only the clear match counts.
        assert_eq!(ids("zzzqqq"), ["z"]);
        // `alpha` is a vault word: both meaning matches count.
        let grounded = ids("alpha");
        assert!(grounded.contains(&"y".to_string()), "{grounded:?}");
        assert!(grounded.contains(&"z".to_string()), "{grounded:?}");
    }

    #[test]
    fn a_prepared_corpus_gives_the_same_results() {
        let v = vault_of(&[
            (
                "a.md",
                "---\ntags: [net]\n---\n# Alpha net\ncloudflare tunnel snapshot.sh\n## Deploy\nrun it\n",
            ),
            ("b.md", "# b\ncloudflare dns, tunnel later\n"),
            ("c.md", "# Gamma\nid_ed25519 key for deploys\n"),
        ]);
        let corpus = Corpus::new(&v);
        let opts = Options {
            include_hubs: false,
            limit: 10,
            semantic: None,
            lexical: true,
        };
        for q in [
            "cloudflare tunnel",
            "deploy",
            "snapshot.sh net",
            "id_ed25519",
            "\"cloudflare tunnel\"",
            "Gamma",
        ] {
            let a = search(&v, q, &Filter::default(), &opts);
            let b = search_in(&v, &corpus, q, &Filter::default(), &opts);
            let key = |h: &Hit| {
                (
                    h.id.clone(),
                    h.score.to_bits(),
                    h.coverage.to_bits(),
                    h.line,
                )
            };
            assert_eq!(
                a.iter().map(key).collect::<Vec<_>>(),
                b.iter().map(key).collect::<Vec<_>>(),
                "{q}"
            );
        }
    }

    #[test]
    fn coverage_weighs_rare_terms() {
        let v = vault_of(&[
            ("a.md", "# a\ncommon rare\n"),
            ("b.md", "# b\ncommon\n"),
            ("c.md", "# c\ncommon\n"),
        ]);
        let opts = Options {
            include_hubs: false,
            limit: 10,
            semantic: None,
            lexical: true,
        };
        let hits = search(&v, "common rare", &Filter::default(), &opts);
        let cov = |id: &str| hits.iter().find(|h| h.id == id).unwrap().coverage;
        assert!((cov("a") - 1.0).abs() < 1e-9);
        // `rare` is in one note, `common` in all: b covers far less than half.
        assert!(cov("b") < 0.35, "{}", cov("b"));
    }

    #[test]
    fn hybrid_fuses_and_respects_the_floor() {
        let v = vault_of(&[
            ("x.md", "# x\nalpha beta\n"),
            ("y.md", "# y\ngamma\n"),
            ("z.md", "# z\ndelta\n"),
        ]);
        let idx = |id: &str| v.index_of(id).unwrap();
        let meaning = [
            Meaning {
                note: idx("y"),
                similarity: 0.9,
                line: 2,
            },
            Meaning {
                note: idx("z"),
                similarity: 0.4,
                line: 2,
            },
        ];
        let opts = |lexical| Options {
            include_hubs: false,
            limit: 10,
            semantic: Some(&meaning),
            lexical,
        };
        let hybrid = search(&v, "alpha", &Filter::default(), &opts(true));
        let ids: Vec<&str> = hybrid.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["x", "y"], "z is below min_similarity");
        assert!(hybrid[1].matched.contains(&"semantic"));
        assert_eq!(hybrid[1].similarity, Some(0.9));
        assert_eq!(hybrid[1].line, 2);
        assert_eq!(hybrid[0].similarity, None);
        // Semantic only.
        let sem = search(&v, "alpha", &Filter::default(), &opts(false));
        assert_eq!(sem.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), ["y"]);
        // An exact name match stays first even when meaning ranks it lower.
        let exact = search(&v, "y", &Filter::default(), &opts(true));
        assert_eq!(exact[0].id, "y");
    }

    #[test]
    fn meaning_below_the_floor_reorders_but_does_not_add() {
        // a and b match the word equally; c does not match it.
        let v = vault_of(&[
            ("a.md", "# a\nalpha\n"),
            ("b.md", "# b\nalpha\n"),
            ("c.md", "# c\ngamma\n"),
        ]);
        let idx = |id: &str| v.index_of(id).unwrap();
        // All below min_similarity (0.5); b is the closest note, a the farthest.
        let meaning = [
            Meaning {
                note: idx("b"),
                similarity: 0.45,
                line: 2,
            },
            Meaning {
                note: idx("c"),
                similarity: 0.40,
                line: 2,
            },
            Meaning {
                note: idx("a"),
                similarity: 0.30,
                line: 2,
            },
        ];
        let opts = Options {
            include_hubs: false,
            limit: 10,
            semantic: Some(&meaning),
            lexical: true,
        };
        let hits = search(&v, "alpha", &Filter::default(), &opts);
        let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(
            ids,
            ["b", "a"],
            "c is below the floor and has no word match"
        );
        assert!(hits.iter().all(|h| h.similarity.is_none()));
        assert!(hits.iter().all(|h| !h.matched.contains(&"semantic")));
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
    fn identifier_splitting_is_bounded() {
        let mut an = Analyzer::new("none");
        // A huge joiner-filled word: indexed whole only.
        let huge = "a.".repeat(5_000);
        assert_eq!(an.terms(&huge).len(), 1);
        // A long but allowed one: at most MAX_PARTS parts and MAX_PARTS prefixes.
        let many = "ab.".repeat(30);
        let many = many.trim_end_matches('.');
        assert!(many.len() <= MAX_IDENT_LEN);
        assert!(an.terms(many).len() <= 1 + 2 * MAX_PARTS);
    }

    #[test]
    fn phrases_are_sequences() {
        let mut an = Analyzer::new("english");
        let q = parse_query("\"ssh key\" github", &mut an);
        assert_eq!(q.terms, ["ssh", "key", "github"]);
        assert_eq!(q.phrases, vec![vec![0, 1]]);
    }
}
