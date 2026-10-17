//! `links <note>`: the notes around a note, each with the line where the link is written,
//! so an agent sees how two notes relate without opening either.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::catalog;
use crate::filter::Filter;
use crate::text;
use crate::vault::{Target, Vault};

const CONTEXT_LEN: usize = 160;

/// Which links to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    /// `out` (`via` links to it), `in` (it links to `via`), or `both`.
    pub dir: &'static str,
    /// Hops from the start note.
    pub depth: usize,
    pub id: String,
    pub hub: bool,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The note one hop closer to the start (the start note itself at depth 1).
    pub via: String,
    /// The note the link is written in (`via` for out/both, `id` for in), and where.
    pub source: String,
    pub line: usize,
    /// The text around the link on that line.
    pub context: String,
    /// For `both`: where `id` links back to `via`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back_context: Option<String>,
    /// How many times the source links to the target.
    pub count: usize,
    pub summary: String,
}

/// A link occurrence: source note, line, line text.
#[derive(Debug, Clone)]
struct Occurrence {
    line: usize,
    context: String,
    count: usize,
}

/// Every note-to-note link in the vault, by (source, target).
struct Index {
    out: Vec<BTreeMap<usize, Occurrence>>,
    inbound: Vec<BTreeMap<usize, Occurrence>>,
}

fn build_index(vault: &Vault) -> Index {
    let n = vault.notes.len();
    let mut out: Vec<BTreeMap<usize, Occurrence>> = vec![BTreeMap::new(); n];
    for (i, note) in vault.notes.iter().enumerate() {
        for r in vault.resolve_links(note) {
            let Target::Note(j) = r.target else { continue };
            if j == i {
                continue;
            }
            let (line_no, line, col) = note.line_at(r.raw.span.start);
            out[i]
                .entry(j)
                .and_modify(|o| o.count += 1)
                .or_insert_with(|| Occurrence {
                    line: line_no,
                    context: text::window(line, col, CONTEXT_LEN),
                    count: 1,
                });
        }
    }
    let mut inbound: Vec<BTreeMap<usize, Occurrence>> = vec![BTreeMap::new(); n];
    for (i, targets) in out.iter().enumerate() {
        for (&j, occ) in targets {
            inbound[j].insert(i, occ.clone());
        }
    }
    Index { out, inbound }
}

/// Notes within `depth` hops of `start`. Hub notes are listed but never expanded through
/// (they link everything). Each note appears once, at its smallest depth.
#[must_use]
pub fn neighbors(
    vault: &Vault,
    start: usize,
    depth: usize,
    dir: Direction,
    filter: &Filter,
) -> Vec<Edge> {
    let index = build_index(vault);
    let generic = &vault.config.generic_names;
    let mut seen: BTreeSet<usize> = BTreeSet::from([start]);
    let mut frontier = vec![start];
    let mut edges = Vec::new();
    for d in 1..=depth.max(1) {
        // target -> (dir, via, source, occurrence, back occurrence)
        type Found = (&'static str, usize, usize, Occurrence, Option<Occurrence>);
        let mut found: BTreeMap<usize, Found> = BTreeMap::new();
        for &u in &frontier {
            if u != start && vault.config.is_hub(&vault.notes[u].id) {
                continue;
            }
            if dir != Direction::In {
                for (&v, occ) in &index.out[u] {
                    if !seen.contains(&v) {
                        found.entry(v).or_insert(("out", u, u, occ.clone(), None));
                    }
                }
            }
            if dir != Direction::Out {
                for (&k, occ) in &index.inbound[u] {
                    if seen.contains(&k) {
                        continue;
                    }
                    match found.get_mut(&k) {
                        Some(e) if e.0 == "out" && e.1 == u => {
                            e.0 = "both";
                            e.4 = Some(occ.clone());
                        }
                        Some(_) => {}
                        None => {
                            found.insert(k, ("in", u, k, occ.clone(), None));
                        }
                    }
                }
            }
        }
        let mut next = Vec::new();
        for (v, (dir_name, via, source, occ, back)) in found {
            seen.insert(v);
            next.push(v);
            let note = &vault.notes[v];
            if !filter.matches(vault, note) {
                continue;
            }
            edges.push(Edge {
                dir: dir_name,
                depth: d,
                id: note.id.clone(),
                kind: note
                    .parsed
                    .frontmatter
                    .str_field("type")
                    .map(str::to_string),
                hub: vault.config.is_hub(&note.id),
                via: vault.notes[via].id.clone(),
                source: vault.notes[source].id.clone(),
                line: occ.line,
                context: occ.context,
                back_line: back.as_ref().map(|b| b.line),
                back_context: back.map(|b| b.context),
                count: occ.count,
                summary: catalog::summary(note, generic),
            });
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    edges
}
