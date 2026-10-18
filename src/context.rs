//! `context`: a context pack for a note or a question, sized to a token budget.
//!
//! Seeds (the note asked for, or the top search hits) are ranked first; then the notes they
//! link and the notes that link them, weighted by link direction and distance. The budget is
//! a cap, filled in rank order:
//!
//! - Seeds go first, best first. Each seed may take what is left after every later seed is
//!   kept a minimum, so the best match is read whole when it fits and weaker ones are cut.
//! - A seed too large to fit gets an excerpt: its lead section and the sections that match
//!   the query, in note order.
//! - A linked note is included whole only when it is closely linked (both ways, or by a strong
//!   seed) and fits; otherwise it gets its one-line summary.
//! - When the best match is weak (see [`Pack::weak`]), seeds show only their matching
//!   sections and linked notes only their summaries.
//!
//! Token use is counted on the output as rendered (tags, reasons, and the list of notes left
//! out included), so `used` is what the reader gets. Every entry says why it is in the pack.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::catalog;
use crate::filter::Filter;
use crate::links::{self, Direction};
use crate::vault::{Note, Vault};

/// Share of the budget kept for summaries of the linked notes.
const SUMMARY_SHARE: f64 = 0.15;
/// Weight of a linked log note (`no_backlink_from`) next to other linked notes.
const LOG_WEIGHT: f64 = 0.6;
/// An excerpt shorter than this is not worth it: a summary says as much.
const MIN_EXCERPT_TOKENS: usize = 120;
/// What each later seed is kept, at least (its whole text when shorter).
const SEED_MINIMUM: usize = 300;
/// A linked note scoring at least this (closely linked to a strong seed) may go in whole.
const LINKED_FULL: f64 = 0.7;
/// Notes named in `<omitted>`; the rest are counted.
const MAX_OMITTED: usize = 20;
/// Characters of link context in a linked note's reason.
const WHY_CONTEXT: usize = 80;

/// A starting note and why.
#[derive(Debug, Clone)]
pub struct Seed {
    pub note: usize,
    /// 0..=1, relative to the best seed.
    pub score: f64,
    pub why: String,
    /// Lines that match the query, with their scores (empty: the note as a whole).
    pub focus: Vec<(usize, f64)>,
    /// The matching text (a search snippet), for a focus line too long to include.
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    Full,
    Excerpt,
    Summary,
}

impl Content {
    fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Excerpt => "excerpt",
            Self::Summary => "summary",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    /// `seed` or `linked`.
    pub role: &'static str,
    pub score: f64,
    pub content: Content,
    /// File lines included (`1-40`, `1-12,30-41`); empty for a summary.
    pub lines: String,
    /// Tokens of this entry as rendered.
    pub tokens: usize,
    pub why: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pack {
    pub budget: usize,
    pub used: usize,
    /// The best seed matched only part of the query by its words and nothing by meaning: the
    /// vault may not cover the question.
    pub weak: bool,
    pub entries: Vec<Entry>,
    /// Ranked notes left out for lack of budget, best first (at most [`MAX_OMITTED`]).
    pub omitted: Vec<String>,
    /// Further notes left out, not named.
    pub omitted_more: usize,
}

/// Rough token count: about four characters per token for ASCII text, one per character for
/// scripts written without spaces (CJK, Thai), two per token for other text (accented Latin,
/// Cyrillic, Greek, emoji). Errs high, so a pack stays inside the real budget.
#[must_use]
pub fn tokens(s: &str) -> usize {
    let mut c = Count::default();
    c.add(s);
    c.total()
}

/// Character counts by class, so the estimate for text built line by line equals the
/// estimate for the whole.
#[derive(Debug, Clone, Copy, Default)]
struct Count {
    ascii: usize,
    unspaced: usize,
    other: usize,
}

impl Count {
    fn add(&mut self, s: &str) {
        for c in s.chars() {
            if c.is_ascii() {
                self.ascii += 1;
            } else if crate::mentions::is_unspaced(c) {
                self.unspaced += 1;
            } else {
                self.other += 1;
            }
        }
    }

    /// `s` and a line break.
    fn add_line(&mut self, s: &str) {
        self.add(s);
        self.ascii += 1;
    }

    fn total(self) -> usize {
        self.ascii.div_ceil(4) + self.unspaced + self.other.div_ceil(2)
    }

    /// The total with `s` and a line break added.
    fn with_line(self, s: &str) -> usize {
        let mut c = self;
        c.add_line(s);
        c.total()
    }
}

/// Weight of a link from a seed: what the seed links matters most, links both ways more.
fn edge_weight(dir: &str, depth: usize, count: usize) -> f64 {
    let base = match dir {
        "both" => 0.75,
        "out" => 0.6,
        _ => 0.5,
    };
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
    let decay = 0.5f64.powi(depth.saturating_sub(1) as i32);
    #[allow(clippy::cast_precision_loss)]
    let repeat = 0.05 * (count.saturating_sub(1).min(3)) as f64;
    (base + repeat) * decay
}

struct Ranked {
    note: usize,
    score: f64,
    seed: bool,
    /// For a linked note: its strongest link to a seed (then how many more seeds link it).
    why: Vec<String>,
    more_seeds: usize,
    focus: Vec<(usize, f64)>,
    snippet: Option<String>,
}

/// Attribute value: `&` and `"` escaped.
fn attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Note text inside `<note>`: a literal `</note>` would end the entry.
fn body(s: &str) -> String {
    s.replace("</note>", "&lt;/note>")
}

/// One entry as printed in text form.
#[must_use]
pub fn render_entry(e: &Entry) -> String {
    let lines = if e.lines.is_empty() {
        String::new()
    } else {
        format!(" lines=\"{}\"", e.lines)
    };
    let why = e.why.join("; ").replace(['\t', '\n', '\r'], " ");
    format!(
        "<note id=\"{}\" role=\"{}\" content=\"{}\"{lines} score=\"{:.2}\" tokens=\"{}\">\n<why>{}</why>\n{}\n</note>\n",
        attr(&e.id),
        e.role,
        e.content.name(),
        e.score,
        e.tokens,
        body(&why),
        body(e.text.trim_end()),
    )
}

/// The header line of the text form.
#[must_use]
pub fn render_header(p: &Pack) -> String {
    format!(
        "# context budget={} used={} entries={} omitted={}{}\n",
        p.budget,
        p.used,
        p.entries.len(),
        p.omitted.len() + p.omitted_more,
        if p.weak { " weak=true" } else { "" }
    )
}

/// The `<omitted>` line, or nothing.
#[must_use]
pub fn render_omitted(p: &Pack) -> String {
    if p.omitted.is_empty() {
        return String::new();
    }
    let more = if p.omitted_more > 0 {
        format!("; and {} more", p.omitted_more)
    } else {
        String::new()
    };
    format!("<omitted>{}{more}</omitted>\n", p.omitted.join("; "))
}

/// The whole pack in text form (what `used` counts).
#[must_use]
pub fn render(p: &Pack) -> String {
    let mut s = render_header(p);
    for e in &p.entries {
        s.push_str(&render_entry(e));
    }
    s.push_str(&render_omitted(p));
    s
}

/// An entry with its `tokens` set to its own rendered size.
fn sized(mut e: Entry) -> Entry {
    // The count is part of the text it counts: settle it in a few rounds (digits only grow).
    for _ in 0..4 {
        let t = tokens(&render_entry(&e));
        if t <= e.tokens {
            break;
        }
        e.tokens = t;
    }
    e
}

/// `s` cut to `max` characters, with `...` where cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(3)).collect::<String>() + "..."
    }
}

/// The part of `line` around the snippet's text (or its start) that still fits.
fn cut_line(line: &str, snippet: Option<&str>, have: Count, max: usize) -> Option<String> {
    let room = max.saturating_sub(have.total());
    let at = snippet
        .map(|s| s.trim_matches('.').trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| line.find(s).map(|i| i + s.len() / 2))
        .unwrap_or(0);
    let mut chars = room * 4;
    let mut part = crate::text::window(line, at, chars);
    while chars > 16 && have.with_line(&part) + 1 > max {
        chars = chars * 3 / 4;
        part = crate::text::window(line, at, chars);
    }
    (!part.is_empty() && have.with_line(&part) < max).then_some(part)
}

/// Build the pack. `weak`: the seeds matched the question poorly (see [`Pack::weak`]).
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
pub fn build(vault: &Vault, seeds: &[Seed], depth: usize, budget: usize, weak: bool) -> Pack {
    let cfg = &vault.config;
    let mut ranked: BTreeMap<usize, Ranked> = BTreeMap::new();
    for s in seeds {
        ranked.entry(s.note).or_insert_with(|| Ranked {
            note: s.note,
            score: 1.0 + s.score,
            seed: true,
            why: vec![s.why.clone()],
            more_seeds: 0,
            focus: s.focus.clone(),
            snippet: s.snippet.clone(),
        });
    }
    let none = Filter::default();
    let index = links::build_index(vault);
    for s in seeds {
        let seed_id = &vault.notes[s.note].id;
        // A hub links everything: it is a fine seed but says nothing about its neighbors.
        if cfg.is_hub(seed_id) && seeds.len() > 1 {
            continue;
        }
        for e in links::neighbors_in(vault, &index, s.note, depth, Direction::Both, &none) {
            if e.hub {
                continue;
            }
            let Some(i) = vault.index_of(&e.id) else {
                continue;
            };
            // Logs (`no_backlink_from`, like session notes) are history: less to read first.
            let log = if cfg.is_no_backlink_from(&e.id) {
                LOG_WEIGHT
            } else {
                1.0
            };
            let score = s.score * edge_weight(e.dir, e.depth, e.count) * log;
            let context = clip(&e.context, WHY_CONTEXT);
            let why = match (e.dir, e.depth) {
                ("in", 1) => format!("links to [[{seed_id}]] (line {}: {context})", e.line),
                (_, 1) => format!("linked from [[{seed_id}]] (line {}: {context})", e.line),
                _ => format!("{} hops from [[{seed_id}]], via [[{}]]", e.depth, e.via),
            };
            let r = ranked.entry(i).or_insert_with(|| Ranked {
                note: i,
                score: 0.0,
                seed: false,
                why: Vec::new(),
                more_seeds: 0,
                focus: Vec::new(),
                snippet: None,
            });
            if r.seed {
                continue;
            }
            // Linked to several seeds: more relevant than linked to one. The reason shown is
            // the strongest link.
            if r.why.is_empty() {
                r.why = vec![why];
            } else {
                r.more_seeds += 1;
                if score > r.score {
                    r.why = vec![why];
                }
            }
            r.score = r.score.max(score) + 0.1 * r.score.min(score);
        }
    }
    let mut order: Vec<Ranked> = ranked.into_values().collect();
    for r in &mut order {
        if r.more_seeds > 0 {
            r.why
                .push(format!("also linked to {} more seed(s)", r.more_seeds));
        }
    }
    order.sort_by(|a, b| {
        b.seed
            .cmp(&a.seed)
            .then(b.score.total_cmp(&a.score))
            .then_with(|| vault.notes[a.note].id.cmp(&vault.notes[b.note].id))
    });

    let generic = &cfg.generic_names;
    let summary_of = |note: &Note| {
        let title = catalog::title(note, generic);
        let summary = catalog::summary(note, generic);
        match title {
            Some(t) if !summary.is_empty() => format!("{t}: {summary}"),
            Some(t) => t,
            None => summary,
        }
    };
    let entry = |r: &Ranked, content: Content, lines: String, text: String| {
        sized(Entry {
            id: vault.notes[r.note].id.clone(),
            role: if r.seed { "seed" } else { "linked" },
            score: (r.score.min(2.0) * 100.0).round() / 100.0,
            content,
            lines,
            tokens: 0,
            why: r.why.clone(),
            text,
        })
    };
    let summary_entry = |r: &Ranked| {
        entry(
            r,
            Content::Summary,
            String::new(),
            summary_of(&vault.notes[r.note]),
        )
    };
    let full_entry = |r: &Ranked| {
        let note = &vault.notes[r.note];
        let last = note.text.lines().count().max(1);
        entry(r, Content::Full, format!("1-{last}"), note.text.clone())
    };

    // The header, with its numbers at their widest, is part of the budget.
    let header = tokens(&render_header(&Pack {
        budget,
        used: budget,
        weak,
        entries: Vec::new(),
        omitted: Vec::new(),
        omitted_more: order.len(),
    })) + 1;
    let linked_summaries: usize = order
        .iter()
        .filter(|r| !r.seed)
        .map(|r| summary_entry(r).tokens)
        .sum();
    let reserve = linked_summaries.min(((budget as f64) * SUMMARY_SHARE) as usize);
    // What each seed is kept at least (its whole text when shorter), so the first one cannot
    // take everything.
    let seed_min: Vec<usize> = order
        .iter()
        .filter(|r| r.seed)
        .map(|r| full_entry(r).tokens.min(SEED_MINIMUM))
        .collect();

    let mut used = header;
    let mut entries: Vec<Entry> = Vec::new();
    let mut left_out: Vec<String> = Vec::new();
    let mut seed_k = 0usize;
    for r in &order {
        let left = budget.saturating_sub(used);
        let made = if r.seed {
            let later: usize = seed_min[seed_k + 1..].iter().sum();
            seed_k += 1;
            let room = left.saturating_sub(reserve + later);
            let full = full_entry(r);
            // A weak match is shown only where it matches: the rest is likely off topic.
            if full.tokens <= room && (!weak || r.focus.is_empty()) {
                Some(full)
            } else {
                // The excerpt's text may use the room minus the entry's frame.
                let frame = entry(r, Content::Excerpt, "1-99999".into(), String::new()).tokens;
                (room >= MIN_EXCERPT_TOKENS + frame)
                    .then(|| {
                        excerpt(
                            &vault.notes[r.note],
                            &r.focus,
                            r.snippet.as_deref(),
                            room - frame,
                        )
                    })
                    .flatten()
                    .map(|(lines, text)| entry(r, Content::Excerpt, lines, text))
                    .filter(|e| e.tokens <= room)
                    .or_else(|| Some(summary_entry(r)).filter(|e| e.tokens <= left))
            }
        } else {
            (!weak && r.score >= LINKED_FULL)
                .then(|| full_entry(r))
                .filter(|e| e.tokens + reserve <= left)
                .or_else(|| Some(summary_entry(r)).filter(|e| e.tokens <= left))
        };
        match made {
            Some(e) => {
                used += e.tokens;
                entries.push(e);
            }
            None => left_out.push(vault.notes[r.note].id.clone()),
        }
    }
    // Name as many omitted notes as fit (at most MAX_OMITTED); count the rest.
    let mut pack = Pack {
        budget,
        used,
        weak,
        entries,
        omitted: Vec::new(),
        omitted_more: 0,
    };
    let mut named: Vec<String> = left_out.iter().take(MAX_OMITTED).cloned().collect();
    loop {
        pack.omitted.clone_from(&named);
        pack.omitted_more = left_out.len() - named.len();
        let line = tokens(&render_omitted(&pack));
        if used + line <= budget {
            pack.used = used + line;
            break;
        }
        if named.is_empty() {
            // Not even a count fits: say nothing about them.
            pack.omitted_more = 0;
            break;
        }
        named.pop();
    }
    pack
}

/// Sections of a note's body: (first line, last line), 0-based and inclusive, split at
/// headings outside code. The first section runs from the body start (title included) to
/// the first heading after the title.
fn sections(lines: &[&str], body: usize, in_code: &[bool]) -> Vec<(usize, usize)> {
    let is_heading = |k: usize| {
        !in_code[k] && {
            let t = lines[k].trim_start();
            let lv = t.chars().take_while(|c| *c == '#').count();
            lv > 0 && t[lv..].starts_with(' ')
        }
    };
    let title =
        (body..lines.len()).find(|&k| is_heading(k) && lines[k].trim_start().starts_with("# "));
    let mut starts: Vec<usize> = vec![body];
    for k in body + 1..lines.len() {
        if is_heading(k) && Some(k) != title {
            starts.push(k);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| (s, starts.get(n + 1).map_or(lines.len() - 1, |&e| e - 1)))
        .collect()
}

/// The part of a note that fits in `max` tokens, in note order: small frontmatter, the lead
/// section (title and text before the first other heading), then the sections holding the
/// focus lines, best first. A section too long to fit is cut to its heading and the lines
/// around its best focus line, and a focus line too long to fit to the part around the
/// match. Returns the file lines included (`1-12,15,20-48`) and the text, or `None` when
/// nothing of use fits.
#[allow(clippy::too_many_lines)]
fn excerpt(
    note: &Note,
    focus: &[(usize, f64)],
    snippet: Option<&str>,
    max: usize,
) -> Option<(String, String)> {
    let lines: Vec<&str> = note.text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let body = (note.line_of(note.parsed.body_start).max(1) - 1).min(lines.len() - 1);
    let mut fence = crate::text::Fence::default();
    let in_code: Vec<bool> = lines.iter().map(|l| fence.line(l)).collect();
    let secs = sections(&lines, body, &in_code);
    // Section scores: the sum of their focus lines' scores, and the best focus line of each.
    let mut score = vec![0.0f64; secs.len()];
    let mut best_focus: Vec<Option<(usize, f64)>> = vec![None; secs.len()];
    for &(line, s) in focus {
        let k = line.saturating_sub(1);
        if let Some(n) = secs.iter().position(|&(a, b)| a <= k && k <= b) {
            score[n] += s;
            if best_focus[n].is_none_or(|(_, bs)| s > bs) {
                best_focus[n] = Some((k, s));
            }
        }
    }
    // The running count holds a closing `...`; each piece adds a `...` before it.
    let mut have = Count::default();
    have.add_line("...");
    let front = note.text[..note.parsed.body_start].trim_end();
    let front = (!front.trim().is_empty() && tokens(front) * 4 <= max).then_some(front);
    if let Some(f) = front {
        have.add_line(f);
    }
    let mut chosen: Vec<(usize, usize)> = Vec::new();
    let mut cut: Option<(usize, String)> = None;
    // Lines a..=b while they fit (with a `...` before); the last line taken.
    let take = |a: usize, b: usize, cap: usize, have: &mut Count| -> Option<usize> {
        let mut c = *have;
        c.add_line("...");
        let mut end = None;
        for (k, line) in lines.iter().enumerate().take(b + 1).skip(a) {
            if c.with_line(line) > cap {
                break;
            }
            c.add_line(line);
            end = Some(k);
        }
        if end.is_some() {
            *have = c;
        }
        end
    };
    // 1. The lead, at most a third of the room when focus lines are elsewhere. A first
    // section that is only the title takes the next section with it.
    let only_title = (secs[0].0..=secs[0].1).all(|k| {
        let l = lines[k].trim();
        l.is_empty() || l.starts_with("# ")
    });
    let lead_n = if only_title && secs.len() > 1 { 1 } else { 0 };
    let secs: Vec<(usize, usize)> = if lead_n == 1 {
        let mut s = secs.clone();
        s[1].0 = s[0].0;
        s.remove(0);
        score[1] += score[0];
        score.remove(0);
        if best_focus[1].is_none() {
            best_focus[1] = best_focus[0];
        }
        best_focus.remove(0);
        s
    } else {
        secs
    };
    let focus_elsewhere = score.iter().skip(1).any(|&s| s > 0.0);
    let cap = if focus_elsewhere {
        have.total() + max / 3
    } else {
        max
    };
    if let Some(e) = take(secs[0].0, secs[0].1, cap.min(max), &mut have) {
        chosen.push((secs[0].0, e));
    }
    // 2. Sections with focus lines, best first.
    let mut by_score: Vec<usize> = (1..secs.len()).filter(|&n| score[n] > 0.0).collect();
    by_score.sort_by(|&a, &b| score[b].total_cmp(&score[a]).then(a.cmp(&b)));
    for n in by_score {
        let (a, b) = secs[n];
        let mut whole = have;
        whole.add_line("...");
        for l in &lines[a..=b] {
            whole.add_line(l);
        }
        if whole.total() <= max {
            have = whole;
            chosen.push((a, b));
            continue;
        }
        // Too long: the heading, then its matching lines, best first, each whole while it
        // fits; the best one cut to the part around the match when it does not.
        if best_focus[n].is_none() || max.saturating_sub(have.total()) < 40 {
            continue;
        }
        if let Some(e) = take(a, a, max, &mut have) {
            chosen.push((a, e));
        }
        let mut here: Vec<(usize, f64)> = focus
            .iter()
            .map(|&(l, s)| (l.saturating_sub(1), s))
            .filter(|&(k, _)| a < k && k <= b)
            .collect();
        here.sort_by(|x, y| y.1.total_cmp(&x.1).then(x.0.cmp(&y.0)));
        for (k, _) in here {
            if chosen.iter().any(|&(x, y)| x <= k && k <= y) {
                continue;
            }
            if let Some(e) = take(k, k, max, &mut have) {
                chosen.push((k, e));
            } else if cut.is_none() {
                let mut c = have;
                c.add_line("...");
                if let Some(part) = cut_line(lines[k], snippet, c, max) {
                    c.add_line(&part);
                    have = c;
                    cut = Some((k, part));
                }
            }
        }
    }
    // The best focus line, when nothing chosen holds it (one long paragraph): cut it.
    let holds = |l: usize| chosen.iter().any(|&(a, b)| a < l && l <= b + 1);
    if cut.is_none()
        && let Some(&(l, _)) = focus.iter().max_by(|a, b| a.1.total_cmp(&b.1))
        && !holds(l)
    {
        let f = l.saturating_sub(1).min(lines.len() - 1);
        let mut c = have;
        c.add_line("...");
        if let Some(part) = cut_line(lines[f], snippet, c, max) {
            cut = Some((f, part));
        }
    }
    // A title alone is not worth an excerpt.
    let content_lines: usize = chosen
        .iter()
        .map(|&(a, b)| {
            (a..=b)
                .filter(|&k| {
                    !lines[k].trim().is_empty() && !lines[k].trim_start().starts_with("# ")
                })
                .count()
        })
        .sum();
    if content_lines == 0 && cut.is_none() {
        return None;
    }
    // Assemble in note order.
    let mut pieces: Vec<(usize, usize, Option<String>)> =
        chosen.iter().map(|&(a, b)| (a, b, None)).collect();
    if let Some((f, part)) = cut {
        pieces.push((f, f, Some(part)));
    }
    pieces.sort_by_key(|p| (p.0, p.1));
    let mut merged: Vec<(usize, usize, Option<String>)> = Vec::new();
    for p in pieces {
        match merged.last_mut() {
            Some(last) if last.2.is_none() && p.2.is_none() && p.0 <= last.1 + 1 => {
                last.1 = last.1.max(p.1);
            }
            _ => merged.push(p),
        }
    }
    let mut out = String::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let front_lines = front.map_or(0, |f| f.lines().count());
    if let Some(f) = front {
        out.push_str(f);
        out.push('\n');
        ranges.push((1, front_lines));
    }
    // The next line not yet shown (0-based).
    let mut next = front_lines;
    for (a, b, part) in &merged {
        if *a > next {
            out.push_str("...\n");
        }
        match part {
            Some(p) => {
                out.push_str(p);
                out.push('\n');
            }
            None => {
                for l in &lines[*a..=*b] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
        ranges.push((a + 1, b + 1));
        next = next.max(b + 1);
    }
    if next < lines.len() {
        out.push_str("...\n");
    }
    ranges.sort_unstable();
    let mut joined: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        match joined.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => joined.push((a, b)),
        }
    }
    let mut label = String::new();
    for (n, &(a, b)) in joined.iter().enumerate() {
        if n > 0 {
            label.push(',');
        }
        if a == b {
            let _ = write!(label, "{a}");
        } else {
            let _ = write!(label, "{a}-{b}");
        }
    }
    Some((label, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate() {
        assert_eq!(tokens("abcd"), 1);
        assert_eq!(tokens("abcde"), 2);
        assert_eq!(tokens("東京"), 2);
        assert_eq!(tokens("привет"), 3);
        assert_eq!(tokens(""), 0);
    }

    #[test]
    fn weights_fall_with_distance() {
        assert!(edge_weight("both", 1, 1) > edge_weight("out", 1, 1));
        assert!(edge_weight("out", 1, 1) > edge_weight("in", 1, 1));
        assert!(edge_weight("out", 2, 1) < edge_weight("in", 1, 1));
        assert!(edge_weight("out", 1, 3) > edge_weight("out", 1, 1));
    }

    #[test]
    fn sections_split_at_headings_outside_code() {
        let lines = [
            "# T", "intro", "## A", "a", "```", "## not", "```", "## B", "b",
        ];
        let mut fence = crate::text::Fence::default();
        let code: Vec<bool> = lines.iter().map(|l| fence.line(l)).collect();
        assert_eq!(sections(&lines, 0, &code), [(0, 1), (2, 6), (7, 8)]);
    }
}
