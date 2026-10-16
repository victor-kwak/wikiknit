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
    ("no-outbound-links", Severity::Error),
    ("missing-required-link", Severity::Error),
    ("duplicate-alias", Severity::Error),
];

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: &'static str,
    pub note: String,
    pub line: usize,
    pub message: String,
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
    fn push(&mut self, rule: &'static str, note: &str, line: usize, message: String) {
        let severity = match self.vault.config.severity(rule) {
            Severity::Off => return,
            Severity::Warn => "warn",
            Severity::Error => "error",
        };
        self.out.push(Finding {
            rule,
            severity,
            note: note.to_string(),
            line,
            message,
        });
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
    let is_selected = |id: &str| selected.is_none_or(|s| s.contains(id));

    for note in &vault.notes {
        if !is_selected(&note.id) {
            continue;
        }
        let p = &note.parsed;
        if let Some(err) = &p.frontmatter.error {
            sink.push(
                "frontmatter-invalid",
                &note.id,
                2,
                format!("frontmatter is not valid YAML: {err}"),
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
            sink.push(
                "frontmatter-unquoted-link",
                &note.id,
                line,
                format!("`{key}` holds an unquoted [[link]]; quote each one: \"[[target]]\""),
            );
        }
        for l in &p.code_links {
            sink.push(
                "link-in-code",
                &note.id,
                note.line_of(l.span.start),
                format!("[[{}]] is inside code, so it is not a link", l.target),
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
                    sink.push(
                        "unresolved-link",
                        &note.id,
                        line,
                        format!("{shown} points to no note or file"),
                    );
                }
                (Target::SelfLink, _) => {}
                (target, form) => {
                    if form == Form::Ambiguous {
                        sink.push(
                            "ambiguous-link",
                            &note.id,
                            line,
                            format!("{shown} matches several notes; write the full path"),
                        );
                    }
                    if cfg.link_style == LinkStyle::FullPath
                        && r.raw.kind != LinkKind::Markdown
                        && form != Form::FullPath
                    {
                        let full = match target {
                            Target::Note(i) => vault.notes[*i].id.clone(),
                            Target::File(f) => f.clone(),
                            _ => String::new(),
                        };
                        sink.push(
                            "link-style",
                            &note.id,
                            line,
                            format!("{shown} must use the full vault path: [[{full}]]"),
                        );
                    }
                }
            }
        }

        if !cfg.is_hub(&note.id) && outbound.is_empty() {
            sink.push(
                "no-outbound-links",
                &note.id,
                1,
                "note links to no other note".into(),
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
                sink.push(
                    "missing-required-link",
                    &note.id,
                    1,
                    format!("must link [[{target}]]{exists}"),
                );
            }
        }
    }

    duplicate_aliases(vault, &is_selected, &mut sink);
    sink.out
        .sort_by(|a, b| (&a.note, a.line, a.rule).cmp(&(&b.note, b.line, b.rule)));
    sink.out
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
                sink.push(
                    "duplicate-alias",
                    &n.id,
                    1,
                    format!("alias `{alias}` also names {}", others.join(", ")),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_captures() {
        let caps = vec!["project/x".to_string(), "x".to_string()];
        assert_eq!(expand("Projects/$1/overview", &caps), "Projects/x/overview");
    }
}
