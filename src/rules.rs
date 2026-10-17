//! Deterministic link rules.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::config::{LinkStyle, Severity, When};
use crate::parse::{LinkKind, Location};
use crate::vault::{Form, Target, Vault};

/// Every rule and its default severity.
pub const RULES: &[(&str, Severity)] = &[
    ("unresolved-link", Severity::Error),
    ("link-style", Severity::Error),
    ("ambiguous-link", Severity::Warn),
    ("frontmatter-invalid", Severity::Error),
    ("frontmatter-unquoted-link", Severity::Error),
    ("link-in-code", Severity::Warn),
    ("link-in-html", Severity::Warn),
    ("no-outbound-links", Severity::Error),
    ("missing-required-link", Severity::Error),
    ("duplicate-alias", Severity::Error),
    ("file-not-utf8", Severity::Warn),
];

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub note: String,
    pub line: usize,
    pub severity: &'static str,
    pub rule: &'static str,
    pub message: String,
    /// How to fix it (empty when there is no specific fix).
    pub fix: String,
}

impl Finding {
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == "error"
    }
}

struct Sink<'a> {
    vault: &'a Vault,
    out: Vec<Finding>,
}

impl Sink<'_> {
    fn push_fix(
        &mut self,
        rule: &'static str,
        note: &str,
        line: usize,
        message: String,
        fix: String,
    ) {
        let severity = match self.vault.config.severity(rule) {
            Severity::Off => return,
            Severity::Warn => "warn",
            Severity::Error => "error",
        };
        self.out.push(Finding {
            note: note.to_string(),
            line,
            severity,
            rule,
            message,
            fix,
        });
    }
}

/// Edit distance between two strings, or `None` once it must exceed `max` (cheap early exit).
fn distance_within(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        if cur.iter().min().copied().unwrap_or(0) > max {
            return None;
        }
        prev = cur;
    }
    let d = prev[b.len()];
    (d <= max).then_some(d)
}

/// Distinct broken-link targets that get "did you mean" hints in one `check`.
const MAX_HINT_TARGETS: usize = 1000;

/// "Did you mean" for broken links: notes compared once, results memoized per target.
struct Closest<'a> {
    vault: &'a Vault,
    /// (id lowercase, stem lowercase, name lowercase, aliases lowercase)
    keys: Vec<(String, String, String, Vec<String>)>,
    /// target -> up to 10 (distance, note) candidates, best first
    memo: std::collections::HashMap<String, Vec<(usize, usize)>>,
    /// `prefetch` ran: a target missing from `memo` is past the hint cap.
    prefetched: bool,
}

impl<'a> Closest<'a> {
    fn new(vault: &'a Vault) -> Self {
        let generic = &vault.config.generic_names;
        let keys = vault
            .notes
            .iter()
            .map(|n| {
                (
                    n.id.to_lowercase(),
                    n.stem().to_lowercase(),
                    n.name(generic).to_lowercase(),
                    n.parsed
                        .frontmatter
                        .aliases()
                        .iter()
                        .map(|a| a.to_lowercase())
                        .collect(),
                )
            })
            .collect();
        Self {
            vault,
            keys,
            memo: std::collections::HashMap::new(),
            prefetched: false,
        }
    }

    /// Up to three notes a broken link probably meant. A target with a folder is compared
    /// as a whole path (`Projects/c/overview` does not match every `overview`); a bare name
    /// is compared with file names, note names, and aliases. Same-folder matches come first.
    /// Look up hints for many targets at once, in parallel. Past `MAX_HINT_TARGETS` distinct
    /// targets the rest get no hint (`get` returns `None`), so a vault full of broken links
    /// still checks quickly.
    fn prefetch(&mut self, targets: BTreeSet<String>) {
        use rayon::prelude::*;
        let todo: Vec<String> = targets
            .into_iter()
            .filter(|t| !self.memo.contains_key(t))
            .take(MAX_HINT_TARGETS)
            .collect();
        let keys = &self.keys;
        let found: Vec<(String, Vec<(usize, usize)>)> = todo
            .into_par_iter()
            .map(|t| {
                let c = Self::scan(keys, &t);
                (t, c)
            })
            .collect();
        self.memo.extend(found);
        self.prefetched = true;
    }

    fn get(&mut self, target: &str, from: &str) -> Option<Vec<String>> {
        let want = target.trim().trim_end_matches(".md").to_lowercase();
        let candidates = match self.memo.get(&want) {
            Some(c) => c.clone(),
            None if self.prefetched => return None,
            None => {
                let c = Self::scan(&self.keys, &want);
                self.memo.insert(want, c.clone());
                c
            }
        };
        let from_dir = from.rsplit_once('/').map_or("", |(d, _)| d);
        let mut ranked: Vec<(usize, bool, usize)> = candidates
            .into_iter()
            .map(|(d, i)| {
                let id = &self.vault.notes[i].id;
                (
                    d,
                    id.rsplit_once('/').map_or("", |(dd, _)| dd) != from_dir,
                    i,
                )
            })
            .collect();
        ranked.sort();
        Some(
            ranked
                .into_iter()
                .take(3)
                .map(|(_, _, i)| format!("[[{}]]", self.vault.notes[i].id))
                .collect(),
        )
    }

    fn scan(keys: &[(String, String, String, Vec<String>)], want: &str) -> Vec<(usize, usize)> {
        let has_dir = want.contains('/');
        let max = (want.chars().count() / 4).clamp(1, 3);
        let mut scored: Vec<(usize, usize)> = Vec::new();
        if want.chars().count() < 3 {
            return scored;
        }
        for (i, (id, stem, name, aliases)) in keys.iter().enumerate() {
            let d = if has_dir {
                distance_within(want, id, max)
            } else if aliases.iter().any(|a| a == want) {
                Some(0)
            } else {
                [stem, name]
                    .iter()
                    .filter_map(|s| distance_within(want, s, max))
                    .min()
                    .or_else(|| {
                        (want.len() >= 5 && (stem.contains(want) || name.contains(want)))
                            .then_some(max)
                    })
            };
            if let Some(d) = d {
                scored.push((d, i));
            }
        }
        scored.sort_unstable();
        scored.truncate(10);
        scored
    }
}

/// Run all rules over the selected notes (`None` = every note).
#[must_use]
pub fn check(vault: &Vault, selected: Option<&BTreeSet<String>>) -> Vec<Finding> {
    let cfg = &vault.config;
    let mut sink = Sink {
        vault,
        out: Vec::new(),
    };
    let mut closest = Closest::new(vault);
    // Every broken target in the selected notes, hinted in one parallel pass.
    let broken: BTreeSet<String> = vault
        .notes
        .iter()
        .filter(|n| selected.is_none_or(|s| s.contains(&n.id)))
        .flat_map(|n| vault.resolve_links(n))
        .filter(|r| r.target == Target::Unresolved)
        .map(|r| r.raw.target.trim().trim_end_matches(".md").to_lowercase())
        .collect();
    closest.prefetch(broken);
    let is_selected = |id: &str| selected.is_none_or(|s| s.contains(id));

    for note in &vault.notes {
        if !is_selected(&note.id) {
            continue;
        }
        let p = &note.parsed;
        if note.not_utf8 {
            sink.push_fix(
                "file-not-utf8",
                &note.id,
                1,
                "file is not valid UTF-8; read with replacement characters".into(),
                "re-save the file as UTF-8".into(),
            );
        }
        if let Some(err) = &p.frontmatter.error {
            sink.push_fix(
                "frontmatter-invalid",
                &note.id,
                2,
                format!("frontmatter is not valid YAML: {err}"),
                "fix the YAML between the `---` lines; quote values that contain `: `, `#`, or `[[`"
                    .into(),
            );
        }
        for key in &p.unquoted_frontmatter_links {
            let line = p
                .frontmatter
                .yaml
                .as_ref()
                .and_then(|r| {
                    note.text[r.clone()]
                        .find(&format!("{key}:"))
                        .map(|i| note.line_of(r.start + i))
                })
                .unwrap_or(2);
            let targets = unquoted_targets(p.frontmatter.value.as_ref().and_then(|v| v.get(key)));
            let fix = if targets.is_empty() {
                format!("{key}: [\"[[target]]\"]")
            } else {
                format!(
                    "{key}: [{}]",
                    targets
                        .iter()
                        .map(|t| format!("\"[[{t}]]\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            sink.push_fix(
                "frontmatter-unquoted-link",
                &note.id,
                line,
                format!("`{key}` holds an unquoted [[link]]; quote each one: \"[[target]]\""),
                fix,
            );
        }
        for l in &p.code_links {
            sink.push_fix(
                "link-in-code",
                &note.id,
                note.line_of(l.span.start),
                format!("[[{}]] is inside code, so it is not a link", l.target),
                "move it out of the code span if it should be a link".into(),
            );
        }
        for l in &p.html_links {
            sink.push_fix(
                "link-in-html",
                &note.id,
                note.line_of(l.span.start),
                format!(
                    "[[{}]] is inside raw HTML, so it is not rendered as a link",
                    l.target
                ),
                "move it outside the HTML if it should be a link".into(),
            );
        }
        let resolved = vault.resolve_links(note);
        let me = vault.index_of(&note.id);
        let outbound: BTreeSet<usize> = resolved
            .iter()
            .filter_map(|r| match r.target {
                Target::Note(i) if Some(i) != me => Some(i),
                _ => None,
            })
            .collect();
        for r in &resolved {
            let line = note.line_of(r.raw.span.start);
            let shown = match &r.raw.location {
                Location::Frontmatter { key } => {
                    format!("[[{}]] (frontmatter `{key}`)", r.raw.target)
                }
                Location::Body if r.raw.kind == LinkKind::Markdown => format!("({})", r.raw.target),
                Location::Body => format!("[[{}]]", r.raw.target),
            };
            match (&r.target, r.form) {
                (Target::Unresolved, _) => {
                    let fix = match closest.get(&r.raw.target, &note.id) {
                        None => format!(
                            "too many broken links for hints; `wikiknit check {}` gives them",
                            note.id
                        ),
                        Some(near) if near.is_empty() => {
                            "create the note, or remove the link".to_string()
                        }
                        Some(near) => format!("did you mean {}", near.join(" or ")),
                    };
                    sink.push_fix(
                        "unresolved-link",
                        &note.id,
                        line,
                        format!("{shown} points to no note or file"),
                        fix,
                    );
                }
                (Target::SelfLink, _) => {}
                (target, form) => {
                    if form == Form::Ambiguous {
                        let options: Vec<String> = vault
                            .notes
                            .iter()
                            .filter(|n| {
                                let want = r.raw.target.trim_end_matches(".md").to_lowercase();
                                let id = n.id.to_lowercase();
                                id == want || id.ends_with(&format!("/{want}"))
                            })
                            .map(|n| format!("[[{}]]", n.id))
                            .take(4)
                            .collect();
                        let fix = if options.is_empty() {
                            "write the full vault path of the note you mean".to_string()
                        } else {
                            format!("write one of {}", options.join(", "))
                        };
                        sink.push_fix(
                            "ambiguous-link",
                            &note.id,
                            line,
                            format!("{shown} matches several notes; write the full path"),
                            fix,
                        );
                    }
                    // An ambiguous link gets one finding: choosing is the fix, not a replacement.
                    if cfg.link_style == LinkStyle::FullPath
                        && r.raw.kind != LinkKind::Markdown
                        && form != Form::FullPath
                        && form != Form::Ambiguous
                    {
                        let full = match target {
                            Target::Note(i) => vault.notes[*i].id.clone(),
                            Target::File(f) => f.clone(),
                            _ => String::new(),
                        };
                        sink.push_fix(
                            "link-style",
                            &note.id,
                            line,
                            format!("{shown} must use the full vault path: [[{full}]]"),
                            format!("replace with [[{full}]] (keep any |alias or #heading)"),
                        );
                    }
                }
            }
        }

        if !cfg.is_hub(&note.id) && outbound.is_empty() {
            sink.push_fix(
                "no-outbound-links",
                &note.id,
                1,
                "note links to no other note".into(),
                format!("`wikiknit suggest {}` lists link candidates", note.id),
            );
        }

        for req in &cfg.require_link {
            let caps = match &req.when {
                When::Field { field, pattern } => p
                    .frontmatter
                    .str_field(field)
                    .and_then(|v| pattern.captures(v).map(|c| captures(&c))),
                When::Path { pattern } => pattern.captures(&note.id).map(|c| captures(&c)),
            };
            let Some(caps) = caps else { continue };
            let target = expand(&req.target, &caps);
            if target == note.id {
                continue;
            }
            let linked = vault
                .index_of(&target)
                .is_some_and(|t| outbound.contains(&t));
            if !linked {
                let exists = if vault.get(&target).is_some() {
                    ""
                } else {
                    " (that note does not exist yet)"
                };
                let fix = match vault.get(&target) {
                    Some(t) => crate::suggest::add_link_edit(vault, note, t).1,
                    None => format!("create [[{target}]] first, then link it"),
                };
                sink.push_fix(
                    "missing-required-link",
                    &note.id,
                    1,
                    format!("must link [[{target}]]{exists}"),
                    fix,
                );
            }
        }
    }

    duplicate_aliases(vault, &is_selected, &mut sink);
    sink.out
        .sort_by(|a, b| (&a.note, a.line, a.rule).cmp(&(&b.note, b.line, b.rule)));
    sink.out
}

/// Link targets written unquoted under a frontmatter key (`[[a]]` reads as `[["a"]]`).
fn unquoted_targets(v: Option<&serde_json::Value>) -> Vec<String> {
    fn walk(v: &serde_json::Value, depth: usize, out: &mut Vec<String>) {
        match v {
            serde_json::Value::Array(items) => items.iter().for_each(|i| walk(i, depth + 1, out)),
            serde_json::Value::String(s) if depth >= 2 => out.push(s.clone()),
            serde_json::Value::String(s) if s.contains("[[") => {
                out.extend(
                    s.split("[[")
                        .skip(1)
                        .filter_map(|p| p.split("]]").next())
                        .map(str::to_string),
                );
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(v) = v {
        walk(v, 0, &mut out);
    }
    out
}

fn captures(c: &regex::Captures<'_>) -> Vec<String> {
    c.iter()
        .map(|m| m.map_or(String::new(), |m| m.as_str().to_string()))
        .collect()
}

/// Replace `$1`..`$9` with capture groups.
fn expand(template: &str, caps: &[String]) -> String {
    let mut out = template.to_string();
    for (i, c) in caps.iter().enumerate().skip(1).rev() {
        out = out.replace(&format!("${i}"), c);
    }
    out
}

fn duplicate_aliases(vault: &Vault, is_selected: &dyn Fn(&str) -> bool, sink: &mut Sink<'_>) {
    let generic = &vault.config.generic_names;
    // term (lowercase) -> notes using it as their name or an alias
    let mut owners: HashMap<String, BTreeSet<usize>> = HashMap::new();
    for (i, n) in vault.notes.iter().enumerate() {
        owners
            .entry(n.name(generic).to_lowercase())
            .or_default()
            .insert(i);
        for alias in n.parsed.frontmatter.aliases() {
            owners.entry(alias.to_lowercase()).or_default().insert(i);
        }
    }
    for (i, n) in vault.notes.iter().enumerate() {
        if !is_selected(&n.id) {
            continue;
        }
        let mut seen = BTreeSet::new();
        for alias in n.parsed.frontmatter.aliases() {
            let key = alias.to_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            let others: Vec<&str> = owners[&key]
                .iter()
                .filter(|&&j| j != i)
                .map(|&j| vault.notes[j].id.as_str())
                .collect();
            if !others.is_empty() {
                sink.push_fix(
                    "duplicate-alias",
                    &n.id,
                    1,
                    format!("alias `{alias}` also names {}", others.join(", ")),
                    format!("keep `{alias}` on one note; remove it from the others or make it more specific"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_distance() {
        assert_eq!(distance_within("kitten", "sitting", 3), Some(3));
        assert_eq!(distance_within("kitten", "sitting", 2), None);
        assert_eq!(distance_within("a", "abcdef", 2), None);
    }

    #[test]
    fn expand_captures() {
        let caps = vec!["project/x".to_string(), "x".to_string()];
        assert_eq!(expand("Projects/$1/overview", &caps), "Projects/x/overview");
    }
}
