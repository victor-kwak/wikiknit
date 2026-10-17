//! Deterministic link rules.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;

use crate::config::{LinkStyle, Severity, When};
use crate::parse::{LinkKind, Location};
use crate::vault::Note;
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
    ("missing-required-inbound", Severity::Error),
    ("orphan", Severity::Warn),
    ("stale", Severity::Warn),
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
    let today = today();
    // Notes linking each note (hubs count: a note listed in an index is reachable).
    let mut inbound: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); vault.notes.len()];
    for (i, outs) in vault.outbound_all().iter().enumerate() {
        for &j in outs {
            inbound[j].insert(i);
        }
    }

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
        // `[[x]]` in code that names no note is code (TOML `[[table]]`, a regex), not a
        // link someone meant.
        for l in p
            .code_links
            .iter()
            .filter(|l| matches!(vault.target_of(note, l), Target::Note(_) | Target::File(_)))
        {
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

        if let Some(me) = me
            && inbound[me].is_empty()
            && !cfg.is_hub(&note.id)
            && !cfg.is_orphan_exempt(&note.id)
        {
            // The notes it links are the natural places to link it from.
            let back: Vec<String> = outbound
                .iter()
                .filter(|&&t| !cfg.is_hub(&vault.notes[t].id))
                .take(3)
                .map(|&t| format!("[[{}]]", vault.notes[t].id))
                .collect();
            let fix = if back.is_empty() {
                "link it from a related note (`wikiknit search` finds one), or add its folder to `orphan_exempt`"
                    .to_string()
            } else {
                format!(
                    "link it from {} ({}); `wikiknit suggest {}` gives the edits",
                    back.join(" or "),
                    if back.len() == 1 {
                        "it links that note"
                    } else {
                        "it links them"
                    },
                    note.id
                )
            };
            sink.push_fix("orphan", &note.id, 1, "no note links here".into(), fix);
        }

        if let Some(rule) = cfg.stale_rule(&note.id)
            && rule.required
            && p.frontmatter
                .value
                .as_ref()
                .and_then(|v| v.get(&rule.field))
                .is_none()
        {
            sink.push_fix(
                "stale",
                &note.id,
                1,
                format!("has no `{}` date", rule.field),
                rule.fix.clone().unwrap_or_else(|| {
                    format!(
                        "check the note against its source, then add `{}: {}` to the frontmatter",
                        rule.field,
                        format_date(today)
                    )
                }),
            );
        }
        if let Some(rule) = cfg.stale_rule(&note.id)
            && let Some(value) = p
                .frontmatter
                .value
                .as_ref()
                .and_then(|v| v.get(&rule.field))
        {
            let line = frontmatter_line(note, &rule.field);
            let field = &rule.field;
            match value.as_str().and_then(parse_date) {
                Some(day) if today - day > i64::from(rule.days) => sink.push_fix(
                    "stale",
                    &note.id,
                    line,
                    format!(
                        "`{field}` is {} days old (limit {})",
                        today - day,
                        rule.days
                    ),
                    rule.fix.clone().unwrap_or_else(|| {
                        format!(
                            "check the note against its source, then set `{field}: {}`",
                            format_date(today)
                        )
                    }),
                ),
                Some(_) => {}
                None => sink.push_fix(
                    "stale",
                    &note.id,
                    line,
                    format!("`{field}` is not a date"),
                    format!("write it as YYYY-MM-DD: `{field}: {}`", format_date(today)),
                ),
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

    required_inbound(vault, &is_selected, &mut sink);
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

/// `require_inbound`: a matching note must be linked from its `from` note. Checked when either
/// note is selected, since editing either one can break the link.
fn required_inbound(vault: &Vault, is_selected: &dyn Fn(&str) -> bool, sink: &mut Sink<'_>) {
    let cfg = &vault.config;
    if cfg.require_inbound.is_empty() {
        return;
    }
    let out = vault.outbound_all();
    // `from` by id, exactly or ignoring case: Ok(note), Ok(None) for no note, Err(ids) when
    // several ids differ only by case. Cached: many notes share one `from`.
    let mut lookup: HashMap<String, Result<Option<usize>, Vec<String>>> = HashMap::new();
    let mut resolve = |from: &str| {
        lookup
            .entry(from.to_string())
            .or_insert_with(|| {
                if let Some(i) = vault.index_of(from) {
                    return Ok(Some(i));
                }
                match vault.ids_ignoring_case(from) {
                    [] => Ok(None),
                    [i] => Ok(Some(*i)),
                    many => Err(many.iter().map(|&i| vault.notes[i].id.clone()).collect()),
                }
            })
            .clone()
    };
    for (i, note) in vault.notes.iter().enumerate() {
        for req in &cfg.require_inbound {
            let caps = match &req.when {
                When::Field { field, pattern } => note
                    .parsed
                    .frontmatter
                    .str_field(field)
                    .and_then(|v| pattern.captures(v).map(|c| captures(&c))),
                When::Path { pattern } => pattern.captures(&note.id).map(|c| captures(&c)),
            };
            let Some(caps) = caps else { continue };
            let from = expand(&req.from, &caps);
            if from.eq_ignore_ascii_case(&note.id) {
                continue;
            }
            // Cheap selection test first: most notes are not selected in a hook run.
            let target_selected = is_selected(&note.id);
            if !target_selected && !is_selected(&from) && vault.index_of(&from).is_some() {
                continue;
            }
            let from_note = match resolve(&from) {
                Ok(f) => f,
                Err(ids) => {
                    if target_selected {
                        sink.push_fix(
                            "missing-required-inbound",
                            &note.id,
                            1,
                            format!("must be linked from [[{from}]], which names several notes"),
                            format!("fix the `require_inbound` rule: one of {}", ids.join(", ")),
                        );
                    }
                    continue;
                }
            };
            let from = from_note.map_or(from, |f| vault.notes[f].id.clone());
            if from == note.id {
                continue;
            }
            let from_selected = is_selected(&from);
            if !target_selected && !from_selected {
                continue;
            }
            match from_note {
                Some(f) if out[f].contains(&i) => {}
                Some(f) => {
                    let editor = &vault.notes[f];
                    let (line, edit) = sibling_edit(vault, editor, &req.when, note)
                        .unwrap_or_else(|| crate::suggest::add_link_edit(vault, editor, note));
                    if target_selected {
                        sink.push_fix(
                            "missing-required-inbound",
                            &note.id,
                            1,
                            format!("must be linked from [[{from}]]"),
                            format!("in {from}: {edit}"),
                        );
                    } else {
                        // Only the `from` note is being checked: the finding is about it.
                        sink.push_fix(
                            "missing-required-inbound",
                            &from,
                            line.max(1),
                            format!("must link [[{}]] (require_inbound)", note.id),
                            edit,
                        );
                    }
                }
                None if !target_selected => {}
                None => sink.push_fix(
                    "missing-required-inbound",
                    &note.id,
                    1,
                    format!("must be linked from [[{from}]] (that note does not exist yet)"),
                    format!("create [[{from}]] with a link to [[{}]]", note.id),
                ),
            }
        }
    }
}

/// Where `editor` already links other notes matching `when` (an index section listing
/// projects), the edit puts the new link after the last of them, as the same kind of line.
fn sibling_edit(
    vault: &Vault,
    editor: &Note,
    when: &When,
    target: &Note,
) -> Option<(usize, String)> {
    let matches = |n: &Note| match when {
        When::Field { field, pattern } => n
            .parsed
            .frontmatter
            .str_field(field)
            .is_some_and(|v| pattern.is_match(v)),
        When::Path { pattern } => pattern.is_match(&n.id),
    };
    let last = vault
        .resolve_links(editor)
        .into_iter()
        .filter(|r| r.raw.location == Location::Body)
        .filter(|r| matches!(r.target, Target::Note(i) if matches(&vault.notes[i])))
        .map(|r| r.raw.span.start)
        .max()?;
    let (line, text, _) = editor.line_at(last);
    let indent: String = text.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = text.trim_start();
    let link = crate::suggest::link_text(vault, target);
    if rest.starts_with('|') {
        return Some((
            line,
            format!("insert after line {line}: a table row for {link}, like line {line}"),
        ));
    }
    let marker = ["- ", "* ", "+ "]
        .into_iter()
        .find(|m| rest.starts_with(m))
        .unwrap_or("- ");
    Some((
        line,
        format!("insert after line {line}: {indent}{marker}{link}"),
    ))
}

/// Line of frontmatter `key:` (line 2, the first YAML line, if not found).
fn frontmatter_line(note: &Note, key: &str) -> usize {
    note.parsed
        .frontmatter
        .yaml
        .as_ref()
        .and_then(|r| {
            let src = &note.text[r.clone()];
            src.find(&format!("\n{key}:"))
                .map(|i| i + 1)
                .or_else(|| src.starts_with(&format!("{key}:")).then_some(0))
                .map(|i| note.line_of(r.start + i))
        })
        .unwrap_or(2)
}

/// Days since 1970-01-01 (UTC) for a civil date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD`, optionally followed by a time (`2026-10-17T09:00`, `2026-10-17 09:00`).
fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let date = s.get(..10)?;
    if s.len() > 10 && !s[10..].starts_with(['T', 't', ' ']) {
        return None;
    }
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days_in_month)
        .contains(&d)
        .then(|| days_from_civil(y, m, d))
}

fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today in UTC, as days since 1970-01-01.
fn today() -> i64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    i64::try_from(secs / 86_400).unwrap_or(0)
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
    fn dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-10-17"), Some(20_743));
        assert_eq!(parse_date("2026-10-17T09:30:00Z"), Some(20_743));
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
        assert_eq!(parse_date("2023-02-29"), None);
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-10-170"), None);
        assert_eq!(parse_date("17/10/2026"), None);
        assert_eq!(format_date(20_743), "2026-10-17");
        assert_eq!(format_date(19_782), "2024-02-29");
        for d in [-1000, 0, 59, 60, 10_000, 20_743] {
            assert_eq!(parse_date(&format_date(d)), Some(d));
        }
    }

    #[test]
    fn expand_captures() {
        let caps = vec!["project/x".to_string(), "x".to_string()];
        assert_eq!(expand("Projects/$1/overview", &caps), "Projects/x/overview");
    }
}
