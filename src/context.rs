//! `context`: a context pack for a note or a question, sized to a token budget.
//!
//! Seeds (the note asked for, or the top search hits) are ranked first; then the notes they
//! link and the notes that link them, weighted by link direction and distance. The budget is
//! filled in rank order: whole notes while they fit, an excerpt (the section around the
//! matching line) for a seed too large to fit, and a one-line summary for the rest. Every
//! entry says why it is in the pack.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::catalog;
use crate::filter::Filter;
use crate::links::{self, Direction};
use crate::vault::{Note, Vault};

/// Tokens for an entry's framing (id, reasons, tags).
const ENTRY_OVERHEAD: usize = 24;
/// Share of the budget kept for summaries of the notes that do not fit whole.
const SUMMARY_SHARE: f64 = 0.15;
/// Weight of a linked log note (`no_backlink_from`) next to other linked notes.
const LOG_WEIGHT: f64 = 0.6;
/// An excerpt shorter than this is not worth it: a summary says as much.
const MIN_EXCERPT_TOKENS: usize = 120;

/// A starting note and why.
#[derive(Debug, Clone)]
pub struct Seed {
    pub note: usize,
    /// 0..=1, relative to the best seed.
    pub score: f64,
    pub why: String,
    /// The line that matched, for the excerpt of a large note.
    pub line: Option<usize>,
    /// The matching text on that line (a search snippet), for a line too long to include.
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    Full,
    Excerpt,
    Summary,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    /// `seed` or `linked`.
    pub role: &'static str,
    pub score: f64,
    pub content: Content,
    /// File lines included (`1-40`); empty for a summary.
    pub lines: String,
    pub tokens: usize,
    pub why: Vec<String>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pack {
    pub budget: usize,
    pub used: usize,
    pub entries: Vec<Entry>,
    /// Ranked notes left out for lack of budget, best first.
    pub omitted: Vec<String>,
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
    why: Vec<String>,
    line: Option<usize>,
    snippet: Option<String>,
}

/// Build the pack.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn build(vault: &Vault, seeds: &[Seed], depth: usize, budget: usize) -> Pack {
    let cfg = &vault.config;
    let mut ranked: BTreeMap<usize, Ranked> = BTreeMap::new();
    for s in seeds {
        ranked.entry(s.note).or_insert_with(|| Ranked {
            note: s.note,
            score: 1.0 + s.score,
            seed: true,
            why: vec![s.why.clone()],
            line: s.line,
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
            let why = match (e.dir, e.depth) {
                ("in", 1) => format!("links to [[{seed_id}]] (line {}: {})", e.line, e.context),
                (_, 1) => format!("linked from [[{seed_id}]] (line {}: {})", e.line, e.context),
                _ => format!("{} hops from [[{seed_id}]], via [[{}]]", e.depth, e.via),
            };
            let r = ranked.entry(i).or_insert_with(|| Ranked {
                note: i,
                score: 0.0,
                seed: false,
                why: Vec::new(),
                line: None,
                snippet: None,
            });
            if !r.seed {
                // Linked to several seeds: more relevant than linked to one.
                r.score = r.score.max(score) + 0.1 * r.score.min(score);
                r.why.push(why);
            }
        }
    }
    let mut order: Vec<Ranked> = ranked.into_values().collect();
    order.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
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
    // Kept for summaries of the notes that do not fit whole: at most SUMMARY_SHARE of the
    // budget, and no more than those summaries need.
    let summaries: usize = order
        .iter()
        .filter(|r| !r.seed)
        .map(|r| tokens(&summary_of(&vault.notes[r.note])) + ENTRY_OVERHEAD)
        .sum();
    let reserve = summaries.min(((budget as f64) * SUMMARY_SHARE) as usize);
    let mut seeds_left = order.iter().filter(|r| r.seed).count();
    let mut used = 0usize;
    let mut entries = Vec::new();
    let mut omitted = Vec::new();
    for r in order {
        let note = &vault.notes[r.note];
        let left = budget.saturating_sub(used);
        let full = tokens(&note.text) + ENTRY_OVERHEAD;
        // Whole notes may use the budget down to the summary reserve. Seeds share it evenly
        // (what one leaves unused goes to the next), so one long note cannot crowd out the
        // other hits.
        let mut room = left.saturating_sub(reserve);
        if r.seed {
            room /= seeds_left.max(1);
            seeds_left -= 1;
        }
        let part = if full <= room {
            let last = note.text.lines().count().max(1);
            Some((Content::Full, format!("1-{last}"), note.text.clone()))
        } else if r.seed && room >= MIN_EXCERPT_TOKENS + ENTRY_OVERHEAD {
            excerpt(note, r.line, r.snippet.as_deref(), room - ENTRY_OVERHEAD)
                .map(|(lines, text)| (Content::Excerpt, lines, text))
        } else {
            None
        };
        let (content, lines, text) = match part {
            Some(p) => p,
            None => {
                let text = summary_of(note);
                if tokens(&text) + ENTRY_OVERHEAD > left {
                    omitted.push(note.id.clone());
                    continue;
                }
                (Content::Summary, String::new(), text)
            }
        };
        let cost = tokens(&text) + ENTRY_OVERHEAD;
        used += cost;
        entries.push(Entry {
            id: note.id.clone(),
            role: if r.seed { "seed" } else { "linked" },
            score: ((r.score.min(2.0)) * 100.0).round() / 100.0,
            content,
            lines,
            tokens: cost,
            why: r.why,
            text,
        });
    }
    Pack {
        budget,
        used,
        entries,
        omitted,
    }
}

/// The part of a note that fits in `max` tokens: small frontmatter, the H1 line, then the
/// section holding `focus` (from its heading on, or from close above the focus line in a long
/// section), extended down while it fits. A focus line too long to fit is cut to the window
/// around the match. Returns the file lines included (`1-12,15,20-48`) and the text, or
/// `None` when nothing of the section fits.
fn excerpt(
    note: &Note,
    focus: Option<usize>,
    snippet: Option<&str>,
    max: usize,
) -> Option<(String, String)> {
    let lines: Vec<&str> = note.text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let body_line = note.line_of(note.parsed.body_start).max(1);
    let body = (body_line - 1).min(lines.len() - 1);
    // 0-based index of the focus line.
    let f = focus.unwrap_or(body_line).clamp(1, lines.len()) - 1;
    let f = f.max(body);
    let mut fence = crate::text::Fence::default();
    let in_code: Vec<bool> = lines.iter().map(|l| fence.line(l)).collect();
    let is_heading = |k: usize| {
        !in_code[k] && {
            let t = lines[k].trim_start();
            let lv = t.chars().take_while(|c| *c == '#').count();
            lv > 0 && t[lv..].starts_with(' ')
        }
    };
    // The heading that opens the focus line's section; in a long section, start close enough
    // above the focus line that at most half the excerpt comes before it.
    let section = (body..=f).rev().find(|&k| is_heading(k)).unwrap_or(body);
    let mut start = f;
    let mut before = 0usize;
    while start > section {
        before += tokens(lines[start - 1]) + 1;
        if before > max / 2 {
            break;
        }
        start -= 1;
    }
    // Frontmatter (type, status, dates) when it is small next to the excerpt.
    let front = note.text[..note.parsed.body_start].trim_end();
    let front = (!front.trim().is_empty() && tokens(front) * 4 <= max).then_some(front);
    let h1 = (body..lines.len()).find(|&k| !in_code[k] && lines[k].trim_start().starts_with("# "));
    // Running count of what the excerpt holds, closing `...` included.
    let mut have = Count::default();
    have.add_line("...");
    if let Some(front) = front {
        have.add_line(front);
    }
    if let Some(h) = h1.filter(|&h| h < start) {
        have.add_line(lines[h]);
        have.add_line("...");
    }
    // Down from the start while lines fit; a focus line too long to fit is cut to the part
    // around the match.
    let mut end: Option<usize> = None;
    let mut cut: Option<String> = None;
    for (k, line) in lines.iter().enumerate().skip(start) {
        if have.with_line(line) <= max {
            have.add_line(line);
            end = Some(k);
            continue;
        }
        if k == f {
            let room = max.saturating_sub(have.total() + 1);
            // Centre the window on the search snippet's text when it is on this line, and
            // let it grow to the room left.
            let at = snippet
                .map(|s| s.trim_matches('.').trim())
                .filter(|s| !s.is_empty())
                .and_then(|s| line.find(s).map(|i| i + s.len() / 2))
                .unwrap_or(0);
            let mut chars = room * 4;
            let mut part = crate::text::window(line, at, chars);
            while chars > 16 && have.with_line(&part) > max {
                chars = chars * 3 / 4;
                part = crate::text::window(line, at, chars);
            }
            if !part.is_empty() && have.with_line(&part) <= max {
                have.add_line(&part);
                cut = Some(part);
                end = Some(k);
            }
        }
        break;
    }
    let end = end?;
    // Budget left (the note ends soon after the match): extend upward.
    if cut.is_none() {
        while start > section && have.with_line(lines[start - 1]) <= max {
            start -= 1;
            have.add_line(lines[start]);
        }
    }
    let mut out = String::new();
    if let Some(front) = front {
        out.push_str(front);
        out.push('\n');
        ranges.push((1, front.lines().count()));
    }
    // The title, when the excerpt does not start with it.
    if let Some(h) = h1
        && h < start
    {
        out.push_str(lines[h]);
        out.push_str("\n...\n");
        ranges.push((h + 1, h + 1));
    }
    for (k, line) in lines.iter().enumerate().take(end + 1).skip(start) {
        match &cut {
            Some(part) if k == end => out.push_str(part),
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if end + 1 < lines.len() {
        out.push_str("...\n");
    }
    ranges.push((start + 1, end + 1));
    // Adjacent ranges read as one.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let label = merged
        .iter()
        .map(|&(a, b)| {
            if a == b {
                a.to_string()
            } else {
                format!("{a}-{b}")
            }
        })
        .collect::<Vec<_>>()
        .join(",");
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
}
