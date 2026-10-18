//! Alias suggestions: words the vault uses for a note that do not name it yet.
//!
//! Three kinds of evidence, strongest first:
//! - links written as `[[target|term]]`: the vault already calls the note `term`;
//! - the note's title (its H1), when other notes use it;
//! - a capitalized term (`Proton VPN`, `KWin`) that appears in several notes, on lines that
//!   mostly link the same note.
//!
//! Each suggestion says how many notes use the term without linking the note: those become
//! unlinked mentions (`wikiknit mentions`) once the alias is added.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::LazyLock;

use aho_corasick::{AhoCorasickBuilder, MatchKind};
use regex::Regex;
use serde::Serialize;

use crate::mentions::{URL, fold, is_unspaced, is_word_char, prose_name};
use crate::parse::{LinkKind, Location};
use crate::vault::{Note, Target, Vault};

/// `[[target|alias]]`: the alias part.
static WIKI_ALIAS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^!?\[\[[^\[\]|\n]+\|([^\[\]\n]+)\]\]$").expect("valid regex"));
/// `[text](target)`: the text part.
static MD_TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[([^\]]+)\]\(").expect("valid regex"));
/// A word: letters and digits, with `.`, `+`, `-`, `_` inside (`Node.js`, `C++`, `DP-1`).
static WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\p{L}\p{N}](?:[\p{L}\p{N}_.+-]*[\p{L}\p{N}+])?").expect("valid regex")
});

/// Longest phrase taken from prose, in words.
const MAX_WORDS: usize = 3;
const MAX_TERM_CHARS: usize = 60;
/// Longer titles are descriptions, not names.
const MAX_TITLE_WORDS: usize = 4;
/// Notes whose lines must link the same note before a prose term counts as its name.
const MIN_CO_NOTES: usize = 3;

/// One candidate alias and its evidence.
#[derive(Debug, Clone, Serialize)]
pub struct Term {
    pub term: String,
    pub score: f64,
    /// Notes other than the target that use the term.
    pub notes: usize,
    pub reasons: Vec<String>,
}

/// Alias candidates for one note, with one edit that adds all of them (several edits to the
/// same `aliases:` line would overwrite each other).
#[derive(Debug, Clone, Serialize)]
pub struct AliasSuggestion {
    /// The note that gets the aliases.
    pub target: String,
    /// Best term's score.
    pub score: f64,
    /// Best first.
    pub terms: Vec<Term>,
    /// Line of the edit in the target note.
    pub line: usize,
    pub edit: String,
}

#[derive(Default)]
struct Evidence {
    /// Spellings seen, with counts (the most common one is suggested).
    spellings: BTreeMap<String, usize>,
    /// Notes that write `[[target|term]]`.
    displayed_in: BTreeSet<usize>,
    title: bool,
}

/// Body text with code, links, HTML, and URLs blanked out (same byte offsets).
fn prose(note: &Note) -> String {
    let p = &note.parsed;
    let mut skip: Vec<Range<usize>> = p.code.clone();
    skip.push(0..p.body_start);
    skip.extend(p.links.iter().map(|l| l.span.clone()));
    skip.extend(p.code_links.iter().map(|l| l.span.clone()));
    skip.extend(p.html_links.iter().map(|l| l.span.clone()));
    skip.extend(p.non_prose.iter().cloned());
    skip.extend(URL.find_iter(&note.text).map(|m| m.range()));
    let mut bytes = note.text.as_bytes().to_vec();
    for r in skip {
        let end = r.end.min(bytes.len());
        for b in &mut bytes[r.start.min(end)..end] {
            // Keep line breaks, so line numbers stay right.
            if *b != b'\n' {
                *b = b' ';
            }
        }
    }
    // Ranges fall on char boundaries, so whole characters were replaced.
    String::from_utf8(bytes).unwrap_or_default()
}

/// The word stands out as a name: a capital letter, or letters mixed with digits.
fn is_proper(word: &str) -> bool {
    let mut chars = word.chars();
    let first = chars.next().unwrap_or(' ');
    first.is_uppercase()
        || word.chars().skip(1).any(char::is_uppercase)
        || (word.chars().any(char::is_alphabetic) && word.chars().any(|c| c.is_ascii_digit()))
}

/// Alias text as written in a link, or `None` for a plain link.
fn link_alias(note: &Note, span: &Range<usize>, kind: LinkKind) -> Option<String> {
    let raw = &note.text[span.clone()];
    let re = if kind == LinkKind::Markdown {
        &MD_TEXT
    } else {
        &WIKI_ALIAS
    };
    let caps = re.captures(raw)?;
    let a = caps.get(1)?.as_str().trim();
    // `[[install#Linux|Linux]]` names a section, not the note.
    let target_part = &raw[..caps.get(1)?.start()];
    if target_part.contains('#')
        || (kind == LinkKind::Markdown && raw[caps.get(0)?.end()..].contains('#'))
    {
        return None;
    }
    // Link text that is a path or URL is not a name.
    let pathlike = a.contains('/')
        || a.contains('\\')
        || a.contains("://")
        || a.to_lowercase().ends_with(".md");
    (!a.is_empty() && !pathlike).then(|| a.to_string())
}

/// Alias suggestions, best first. `only`: just this target note.
#[must_use]
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn suggest(
    vault: &Vault,
    only: Option<usize>,
    limit: usize,
    min_notes: usize,
) -> Vec<AliasSuggestion> {
    let cfg = &vault.config;
    let generic = &cfg.generic_names;
    let n_notes = vault.notes.len();
    let is_source = |i: usize| !cfg.is_hub(&vault.notes[i].id);
    let is_target = |i: usize| !cfg.is_hub(&vault.notes[i].id) && only.is_none_or(|o| o == i);
    use rayon::prelude::*;

    // Everything that already names a note: such a term is linkable already.
    let mut known: HashSet<String> = HashSet::new();
    for n in &vault.notes {
        known.insert(fold(&n.id).text);
        known.insert(fold(n.stem()).text);
        if let Some(name) = prose_name(n, generic) {
            known.insert(fold(name).text);
        }
        for a in n.parsed.frontmatter.aliases() {
            known.insert(fold(a.trim()).text);
        }
    }
    let usable = |t: &str| {
        let len = t.chars().count();
        len >= cfg.min_alias_len && len <= MAX_TERM_CHARS && !known.contains(&fold(t).text)
    };

    let prose_text: Vec<String> = vault.notes.par_iter().map(prose).collect();

    // Words written in lowercase across many notes are ordinary words ("Run" at the start of a
    // bullet is not a name).
    let mut lower_df: HashMap<String, usize> = HashMap::new();
    for (i, text) in prose_text.iter().enumerate() {
        if !is_source(i) {
            continue;
        }
        let words: HashSet<&str> = WORD
            .find_iter(text)
            .map(|m| m.as_str())
            .filter(|w| w.chars().all(|c| !c.is_uppercase()))
            .collect();
        for w in words {
            *lower_df.entry(w.to_string()).or_default() += 1;
        }
    }
    let common_df = (n_notes / 20).max(3);
    let is_common = |w: &str| {
        lower_df
            .get(&w.to_lowercase())
            .is_some_and(|&c| c >= common_df)
    };

    // (target, folded term) -> evidence
    let mut ev: BTreeMap<(usize, String), Evidence> = BTreeMap::new();
    // 1. link display text
    for (i, note) in vault.notes.iter().enumerate() {
        if !is_source(i) {
            continue;
        }
        for r in vault.resolve_links(note) {
            let Target::Note(t) = r.target else { continue };
            if t == i || !is_target(t) || r.raw.location != Location::Body {
                continue;
            }
            let Some(alias) = link_alias(note, &r.raw.span, r.raw.kind) else {
                continue;
            };
            if !usable(&alias) {
                continue;
            }
            let e = ev.entry((t, fold(&alias).text)).or_default();
            *e.spellings.entry(alias).or_default() += 1;
            e.displayed_in.insert(i);
        }
    }
    // Link text used for several notes (`here`, a company name written on links to each of its
    // projects) names none of them: keep it only for a target with two thirds of its uses.
    let mut shown_total: HashMap<String, usize> = HashMap::new();
    for ((_, term), e) in &ev {
        *shown_total.entry(term.clone()).or_default() += e.displayed_in.len();
    }
    for ((_, term), e) in &mut ev {
        if e.displayed_in.len() * 3 < shown_total[term] * 2 {
            e.displayed_in.clear();
            e.spellings.clear();
        }
    }
    ev.retain(|_, e| !e.displayed_in.is_empty());
    // 2. titles
    for (t, note) in vault.notes.iter().enumerate() {
        if !is_target(t) {
            continue;
        }
        if let Some(title) = crate::catalog::title(note, generic)
            && usable(&title)
            && title.split_whitespace().count() <= MAX_TITLE_WORDS
            && !WORD.find_iter(&title).all(|w| is_common(w.as_str()))
        {
            let e = ev.entry((t, fold(&title).text)).or_default();
            *e.spellings.entry(title).or_default() += 1;
            e.title = true;
        }
    }
    // 3. capitalized terms from prose. Only whole runs of capitalized words, with ordinary
    // words trimmed from the ends: `Cloud Cost Optimization` gives that phrase, not
    // `Optimization`. Headings are skipped (Title Case template headings are not names).
    #[derive(Default)]
    struct Phrase {
        spellings: BTreeMap<String, usize>,
        notes: BTreeSet<usize>,
        /// Seen after another word in a sentence, not only where any word is capitalized.
        mid_sentence: bool,
    }
    let mut phrases: HashMap<String, Phrase> = HashMap::new();
    for (i, text) in prose_text.iter().enumerate() {
        if !is_source(i) {
            continue;
        }
        for line in text.lines() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            let words: Vec<regex::Match<'_>> = WORD.find_iter(line).collect();
            let mut k = 0;
            while k < words.len() {
                // A run of proper words separated by single spaces.
                let mut end = k;
                while end < words.len()
                    && is_proper(words[end].as_str())
                    && (end == k || &line[words[end - 1].end()..words[end].start()] == " ")
                {
                    end += 1;
                }
                let (mut a, mut b) = (k, end);
                while a < b && is_common(words[a].as_str()) {
                    a += 1;
                }
                while b > a && is_common(words[b - 1].as_str()) {
                    b -= 1;
                }
                let starts_sentence = |a: usize| {
                    !line[..words[a].start()]
                        .trim_end()
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_alphanumeric() || matches!(c, ',' | ')'))
                };
                let mut spans = vec![(a, b)];
                // At a sentence start any first word is capitalized (`The KWin crash`), so
                // the run without it counts too.
                let plain = |w: &str| w.chars().skip(1).all(|c| !c.is_uppercase());
                if b > a + 1 && starts_sentence(a) && plain(words[a].as_str()) {
                    spans.push((a + 1, b));
                }
                for (a, b) in spans {
                    if a >= b || b - a > MAX_WORDS {
                        continue;
                    }
                    let term = &line[words[a].start()..words[b - 1].end()];
                    if usable(term) {
                        let e = phrases.entry(fold(term).text).or_default();
                        *e.spellings.entry(term.to_string()).or_default() += 1;
                        e.notes.insert(i);
                        e.mid_sentence |= !starts_sentence(a);
                    }
                }
                k = end.max(k + 1);
            }
        }
    }
    let max_df = ((n_notes * 3) / 10).max(min_notes);
    let phrases: HashMap<String, BTreeMap<String, usize>> = phrases
        .into_iter()
        .filter(|(term, p)| {
            // One plain capitalized word (`Optimization`) must also appear mid-sentence: at
            // the start of a sentence or bullet any word is capitalized.
            let plain_word = !term.contains(' ')
                && p.spellings.keys().all(|s| {
                    s.chars()
                        .skip(1)
                        .all(|c| !c.is_uppercase() && !c.is_ascii_digit())
                });
            (p.mid_sentence || !plain_word) && p.notes.len() >= min_notes && p.notes.len() <= max_df
        })
        .map(|(t, p)| (t, p.spellings))
        .collect();

    // Find every use of every candidate term (any case), with the links on the same line.
    let mut terms: Vec<String> = ev.keys().map(|(_, t)| t.clone()).collect();
    terms.extend(phrases.keys().cloned());
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        return Vec::new();
    }
    let ac = AhoCorasickBuilder::new()
        // Overlapping: `The Kafka Broker` must not hide the uses of `Kafka Broker`.
        .match_kind(MatchKind::Standard)
        .build(&terms)
        .expect("valid patterns");
    // term -> note -> (notes linked on the lines using it, spellings written there)
    #[derive(Default, Clone)]
    struct Use {
        links: BTreeSet<usize>,
        written: BTreeSet<String>,
        /// Spellings written after another word in a sentence.
        written_mid: BTreeSet<String>,
    }
    let per_note: Vec<Vec<(usize, usize, Use)>> = vault
        .notes
        .par_iter()
        .enumerate()
        .filter(|(i, _)| is_source(*i))
        .map(|(i, note)| {
            let text = &prose_text[i];
            let mut links_on: HashMap<usize, BTreeSet<usize>> = HashMap::new();
            for r in vault.resolve_links(note) {
                if let Target::Note(t) = r.target
                    && t != i
                {
                    links_on
                        .entry(note.line_of(r.raw.span.start))
                        .or_default()
                        .insert(t);
                }
            }
            let mut found: BTreeMap<usize, Use> = BTreeMap::new();
            let folded = fold(text);
            for m in ac.find_overlapping_iter(&folded.text) {
                let abs = folded.orig[m.start()]..folded.orig[m.end()];
                if abs.is_empty() {
                    continue;
                }
                let matched = &text[abs.clone()];
                let first = matched.chars().next().expect("non-empty");
                let last = matched.chars().next_back().expect("non-empty");
                let before = text[..abs.start].chars().next_back();
                let after = text[abs.end..].chars().next();
                if (!is_unspaced(first) && before.is_some_and(is_word_char))
                    || (!is_unspaced(last) && after.is_some_and(is_word_char))
                {
                    continue;
                }
                let line = note.line_of(abs.start);
                let u = found.entry(m.pattern().as_usize()).or_default();
                if let Some(l) = links_on.get(&line) {
                    u.links.extend(l);
                }
                u.written.insert(matched.to_string());
                let mid = text[..abs.start]
                    .trim_end_matches([' ', '\t'])
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || matches!(c, ',' | ')'));
                if mid {
                    u.written_mid.insert(matched.to_string());
                }
            }
            found.into_iter().map(|(k, u)| (k, i, u)).collect()
        })
        .collect();
    let mut uses: Vec<BTreeMap<usize, Use>> = vec![BTreeMap::new(); terms.len()];
    for (k, i, u) in per_note.into_iter().flatten() {
        uses[k].insert(i, u);
    }
    let term_index: HashMap<&str, usize> = terms
        .iter()
        .enumerate()
        .map(|(k, t)| (t.as_str(), k))
        .collect();

    // 3b. a prose term goes to the note its lines link most, when that is clear.
    for (term, spellings) in &phrases {
        let used = &uses[term_index[term.as_str()]];
        let mut votes: BTreeMap<usize, usize> = BTreeMap::new();
        for u in used.values() {
            for &t in &u.links {
                if is_target(t) {
                    *votes.entry(t).or_default() += 1;
                }
            }
        }
        let mut ranked: Vec<(usize, usize)> = votes.into_iter().map(|(t, c)| (c, t)).collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        let Some(&(best, t)) = ranked.first() else {
            continue;
        };
        // Lines that use a term and link a note show the two are related; only when nearly
        // every use links the same note is the term a name for it.
        let others = used.keys().filter(|&&n| n != t).count();
        let clear = ranked.get(1).is_none_or(|&(second, _)| second * 2 <= best);
        if best >= min_notes.max(MIN_CO_NOTES) && best * 3 >= others * 2 && clear {
            let e = ev.entry((t, term.clone())).or_default();
            for (s, c) in spellings {
                *e.spellings.entry(s.clone()).or_default() += c;
            }
        }
    }

    // target -> (term, its spelling)
    let mut found: BTreeMap<usize, Vec<Term>> = BTreeMap::new();
    for ((t, term), e) in ev {
        let target = &vault.notes[t];
        let used = &uses[term_index[term.as_str()]];
        let others: BTreeSet<usize> = used
            .keys()
            .chain(&e.displayed_in)
            .copied()
            .filter(|&n| n != t)
            .collect();
        let co = used
            .iter()
            .filter(|(n, u)| **n != t && u.links.contains(&t))
            .count();
        let unlinked = others
            .iter()
            .filter(|&&n| !vault.outbound_all()[n].contains(&t))
            .count();
        let shown = e.displayed_in.len();
        let spelling = e
            .spellings
            .iter()
            .max_by_key(|(s, c)| (**c, std::cmp::Reverse((*s).clone())))
            .map(|(s, _)| s.clone())
            .expect("evidence has a spelling");
        // Link text used once counts when other notes also write the term.
        let prose_users = used
            .keys()
            .filter(|n| **n != t && !e.displayed_in.contains(n))
            .count();
        let shown_enough = shown >= min_notes || (shown > 0 && prose_users > 0);
        // A title counts when another note writes it the same way (`Installation` in a heading
        // does not make every lowercase "installation" a name).
        // One capitalized word (`Overview`) must be written so mid-sentence: at a sentence
        // start any word is capitalized.
        let one_word = !spelling.contains(' ')
            && spelling
                .chars()
                .skip(1)
                .all(|c| !c.is_uppercase() && !c.is_ascii_digit());
        let title_used = e.title
            && used.iter().any(|(n, u)| {
                *n != t
                    && if one_word {
                        u.written_mid.contains(&spelling)
                    } else {
                        u.written.contains(&spelling)
                    }
            });
        let co_named = co >= min_notes.max(MIN_CO_NOTES) && co * 3 >= others.len() * 2;
        if !shown_enough && !co_named && !title_used {
            continue;
        }
        let mut reasons = Vec::new();
        if shown > 0 {
            reasons.push(format!(
                "linked as [[{}|{spelling}]] in {shown} note(s)",
                target.id
            ));
        }
        if e.title {
            reasons.push("its title".to_string());
        }
        if co > 0 {
            reasons.push(format!(
                "{co} of {} note(s) using it link [[{}]] on the same line",
                others.len(),
                target.id
            ));
        }
        if unlinked > 0 {
            reasons.push(format!(
                "{unlinked} note(s) use it without linking [[{}]]",
                target.id
            ));
        }
        let score = 2.0 * shown.min(3) as f64
            + if title_used { 1.0 } else { 0.0 }
            + 2.0 * (co as f64 / others.len().max(1) as f64) * (co.min(5) as f64 / 5.0).sqrt()
            + 0.5 * unlinked.min(4) as f64;
        found.entry(t).or_default().push(Term {
            term: spelling,
            score: (score * 100.0).round() / 100.0,
            notes: others.len(),
            reasons,
        });
    }
    // One note per term: the best-supported one.
    let mut all: Vec<(usize, Term)> = found
        .into_iter()
        .flat_map(|(t, terms)| terms.into_iter().map(move |x| (t, x)))
        .collect();
    all.sort_by(|a, b| {
        b.1.score
            .total_cmp(&a.1.score)
            .then_with(|| a.1.term.cmp(&b.1.term))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut taken: HashSet<String> = HashSet::new();
    all.retain(|(_, x)| taken.insert(fold(&x.term).text));
    let mut by_target: BTreeMap<usize, Vec<Term>> = BTreeMap::new();
    for (t, x) in all {
        by_target.entry(t).or_default().push(x);
    }
    let mut out: Vec<AliasSuggestion> = by_target
        .into_iter()
        .map(|(t, terms)| {
            let target = &vault.notes[t];
            let words: Vec<&str> = terms.iter().map(|x| x.term.as_str()).collect();
            let (line, edit) = alias_edit(target, &words);
            AliasSuggestion {
                target: target.id.clone(),
                score: terms[0].score,
                terms,
                line,
                edit,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.target.cmp(&b.target))
    });
    out.truncate(limit);
    out
}

/// A YAML value with a trailing `# comment` removed (a `#` inside quotes is kept).
fn strip_comment(v: &str) -> &str {
    v[..crate::parse::yaml_comment_start(v)].trim_end()
}

/// The edit that adds `terms` to the note's `aliases` (one edit for all of them).
#[must_use]
pub fn alias_edit(note: &Note, terms: &[&str]) -> (usize, String) {
    let quoted: Vec<String> = terms
        .iter()
        .map(|t| serde_json::to_string(t).expect("a string serializes"))
        .collect();
    let list = quoted.join(", ");
    let Some(yaml) = &note.parsed.frontmatter.yaml else {
        return (
            1,
            format!("insert at line 1: ---\\naliases: [{list}]\\n---"),
        );
    };
    let src = &note.text[yaml.clone()];
    let lines: Vec<&str> = src.lines().collect();
    // The first YAML line; for empty frontmatter, this is the closing `---`.
    let first_line = note.line_of(yaml.start);
    let key = lines.iter().position(|l| {
        let l = l.trim_end();
        l.starts_with("aliases:") || l.starts_with("alias:")
    });
    let Some(k) = key else {
        // After the last YAML line (the opening `---` when there is none).
        let last = first_line - 1 + lines.len();
        return (last, format!("insert after line {last}: aliases: [{list}]"));
    };
    let line_no = first_line + k;
    let (name, value) = lines[k].split_once(':').expect("has a colon");
    let comment = &value.trim()[strip_comment(value.trim()).len()..];
    let comment = if comment.trim().is_empty() {
        String::new()
    } else {
        format!(" {}", comment.trim())
    };
    let value = strip_comment(value.trim());
    // Lines after the key line that belong to its value: indented (or blank) lines, such as a
    // flow list over several lines or a block scalar. Trailing blank lines are not part of it.
    let mut extra = lines[k + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
        .count();
    while extra > 0 && lines[k + extra].trim().is_empty() {
        extra -= 1;
    }
    let block_items = lines[k + 1..k + 1 + extra]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with("- ") || l.trim() == "-");
    if value.is_empty() && extra > 0 && block_items {
        // A block list: add items after the last one.
        let last = lines[k + extra];
        let indent: String = last.chars().take_while(|c| c.is_whitespace()).collect();
        let at = line_no + extra;
        let new: Vec<String> = quoted.iter().map(|q| format!("{indent}- {q}")).collect();
        return (at, format!("insert after line {at}: {}", new.join("\\n")));
    }
    // A block list at the key's own indentation (`- a` at column 0) also belongs to the key.
    if value.is_empty() && extra == 0 {
        let items = lines[k + 1..]
            .iter()
            .take_while(|l| l.starts_with("- ") || l.trim_end() == "-")
            .count();
        if items > 0 {
            let at = line_no + items;
            let new: Vec<String> = quoted.iter().map(|q| format!("- {q}")).collect();
            return (at, format!("insert after line {at}: {}", new.join("\\n")));
        }
    }
    // Everything else is replaced whole, from the parsed value: `~`/`null`/nothing is no
    // alias; a scalar is one alias (`foo, bar` is one, not two); a list keeps its items.
    let current: Vec<String> = match note
        .parsed
        .frontmatter
        .value
        .as_ref()
        .and_then(|v| v.get(name.trim()))
    {
        Some(serde_json::Value::String(s)) => vec![s.trim_end_matches('\n').to_string()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .map(|i| match i {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(serde_json::Value::Null) | None => Vec::new(),
        Some(other) => vec![other.to_string()],
    };
    let single_flow =
        extra == 0 && value.starts_with('[') && value.ends_with(']') && !value.starts_with("[[");
    let new = if single_flow {
        // One line `[a, b]`: keep the items as written.
        let inner = value[1..value.len() - 1].trim();
        if inner.is_empty() {
            format!("[{list}]")
        } else {
            format!("[{inner}, {list}]")
        }
    } else {
        let mut all: Vec<String> = current
            .iter()
            .map(|c| serde_json::to_string(c).expect("a string serializes"))
            .collect();
        all.extend(quoted.iter().cloned());
        format!("[{}]", all.join(", "))
    };
    if extra == 0 {
        (
            line_no,
            format!("line {line_no}: replace with: {name}: {new}{comment}"),
        )
    } else {
        let end = line_no + extra;
        (
            line_no,
            format!("lines {line_no}-{end}: replace with: {name}: {new}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proper_words() {
        assert!(is_proper("Proton"));
        assert!(is_proper("KWin"));
        assert!(is_proper("iPhone"));
        assert!(is_proper("rtx3080"));
        assert!(!is_proper("proton"));
        assert!(!is_proper("2026"));
    }
}
