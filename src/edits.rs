//! Edits that apply in output order. `suggest` and `check` plan what each row should do to a
//! note (add a link, turn a mention into a link); this module writes the edits for one note
//! in order, each against the note as the earlier edits leave it, so an agent applying them
//! one after another gets what was meant: one section or key created once, line numbers that
//! count the lines inserted above, links next to their siblings.

use std::path::PathBuf;

use crate::vault::{Note, Vault};

/// What a row does to its note.
#[derive(Debug, Clone)]
pub enum Planned {
    /// Add a link to note `target` (an index into `vault.notes`) in the link section.
    Link { target: usize },
    /// Add a link to `target` next to the note's links to notes matching
    /// `require_inbound[rule].when`, else in the link section.
    Sibling { target: usize, rule: usize },
    /// Replace `from` with `to` on 1-based `line` of the note as it was: the occurrence after
    /// `nth` earlier ones on that line.
    Replace {
        line: usize,
        nth: usize,
        from: String,
        to: String,
    },
}

/// A note as planned edits leave it, and where each of its original lines is now.
struct Virtual {
    note: Note,
    /// `now[k]` = current 1-based line of original line `k + 1`.
    now: Vec<usize>,
}

impl Virtual {
    fn new(note: &Note) -> Self {
        let n = note.text.lines().count().max(1);
        Self {
            note: note.clone(),
            now: (1..=n).collect(),
        }
    }

    fn lines(&self) -> Vec<String> {
        self.note.text.lines().map(str::to_string).collect()
    }

    fn set(&mut self, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        self.note = Note::from_text(&self.note.id, PathBuf::from(&self.note.path), text);
    }

    /// Lines `a..=b` (1-based, current) became `count` lines.
    fn moved(&mut self, a: usize, b: usize, count: usize) {
        for l in &mut self.now {
            if *l > b {
                *l = *l + count - (b + 1 - a);
            } else if *l >= a {
                *l = a.min(*l);
            }
        }
    }

    /// Apply one edit in the grammar this crate writes. Edits that are not mechanical
    /// (`a table row for ...`) count as one inserted line.
    fn apply(&mut self, edit: &str) {
        let mut lines = self.lines();
        let split = |s: &str| s.split("\\n").map(str::to_string).collect::<Vec<_>>();
        let num = |s: &str, prefix: &str| -> Option<(usize, String)> {
            let rest = s.strip_prefix(prefix)?;
            let end = rest.find(|c: char| !c.is_ascii_digit())?;
            Some((rest[..end].parse().ok()?, rest[end..].to_string()))
        };
        for prefix in ["insert after line ", "append after line "] {
            if let Some((n, rest)) = num(edit, prefix) {
                let text = rest.split_once(": ").map_or("", |(_, t)| t);
                let new = split(text);
                let n = n.min(lines.len());
                let count = new.len();
                lines.splice(n..n, new);
                // Lines after `n` move down.
                for l in &mut self.now {
                    if *l > n {
                        *l += count;
                    }
                }
                self.set(&lines);
                return;
            }
        }
        if let Some(text) = edit.strip_prefix("insert at line 1: ") {
            let new = split(text);
            let count = new.len();
            lines.splice(0..0, new);
            for l in &mut self.now {
                *l += count;
            }
            self.set(&lines);
            return;
        }
        let range = num(edit, "lines ").and_then(|(a, rest)| {
            let (b, rest) = num(&rest, "-")?;
            Some((a, b, rest))
        });
        let range = range.or_else(|| num(edit, "line ").map(|(a, rest)| (a, a, rest)));
        if let Some((a, b, rest)) = range
            && let Some(text) = rest.strip_prefix(": replace with: ")
            && a >= 1
            && b <= lines.len()
        {
            let new = split(text);
            let count = new.len();
            lines.splice(a - 1..b, new);
            self.moved(a, b, count);
            self.set(&lines);
        }
        // `line N[, column C]: replace "x" with y` is applied by the caller, which knows the
        // exact text.
    }
}

/// The edits for `plans` in `editor`, in order: each against the note as the edits before it
/// leave it.
#[must_use]
pub fn plan(vault: &Vault, editor: &Note, plans: &[Planned]) -> Vec<(usize, String)> {
    let mut v = Virtual::new(editor);
    let mut out = Vec::with_capacity(plans.len());
    for p in plans {
        let (line, edit) = match p {
            Planned::Link { target } => {
                crate::suggest::add_link_edit(vault, &v.note, &vault.notes[*target])
            }
            Planned::Sibling { target, rule } => {
                let target = &vault.notes[*target];
                crate::rules::sibling_edit(
                    vault,
                    &v.note,
                    &vault.config.require_inbound[*rule].when,
                    target,
                )
                .unwrap_or_else(|| crate::suggest::add_link_edit(vault, &v.note, target))
            }
            Planned::Replace {
                line,
                nth,
                from,
                to,
            } => {
                let now = v.now.get(line - 1).copied().unwrap_or(*line);
                let mut lines = v.lines();
                let Some(text) = lines.get(now - 1).cloned() else {
                    out.push((now, format!("line {now}: replace \"{from}\" with {to}")));
                    continue;
                };
                let at = text
                    .match_indices(from.as_str())
                    .nth(*nth)
                    .or_else(|| text.match_indices(from.as_str()).next())
                    .map(|(i, _)| i);
                let Some(at) = at else {
                    out.push((now, format!("line {now}: replace \"{from}\" with {to}")));
                    continue;
                };
                let column = if text[..at].contains(from.as_str()) {
                    format!(", column {}", text[..at].chars().count() + 1)
                } else {
                    String::new()
                };
                lines[now - 1] = format!("{}{to}{}", &text[..at], &text[at + from.len()..]);
                v.set(&lines);
                out.push((
                    now,
                    format!("line {now}{column}: replace \"{from}\" with {to}"),
                ));
                continue;
            }
        };
        v.apply(&edit);
        out.push((line, edit));
    }
    out
}
