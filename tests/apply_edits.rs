//! End-to-end: apply every `suggest` / `check` edit in output order to a copy of a vault,
//! the way an agent would, then check that the notes still parse and hold each intended link
//! exactly once. Edits name lines in the note as the earlier edits left it.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn vault(config: &str, files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("wikiknit.toml"), config).unwrap();
    for (path, body) in files {
        let p = dir.path().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    dir
}

fn items(dir: &Path, args: &[&str]) -> Vec<Value> {
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
        .env("XDG_STATE_HOME", dir.join(".state"))
        .env("XDG_CACHE_HOME", dir.join(".cache"))
        .env_remove("WIKIKNIT_SESSION")
        .args(["--format", "json"])
        .args(args)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    v["items"].as_array().unwrap().clone()
}

/// Number after `prefix` at the start of `s`, and the rest.
fn num<'a>(s: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let rest = s.strip_prefix(prefix)?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    Some((rest[..end].parse().ok()?, &rest[end..]))
}

/// Apply one edit to a note's lines. `None` if the text is not a mechanical edit.
fn apply(lines: &mut Vec<String>, edit: &str) -> Option<()> {
    let split = |s: &str| s.split("\\n").map(str::to_string).collect::<Vec<_>>();
    for prefix in ["insert after line ", "append after line "] {
        if let Some((n, rest)) = num(edit, prefix) {
            // An optional ` (the ... line)` note, then `: text`.
            let text = rest.split_once(": ").expect("insert text").1;
            assert!(
                n <= lines.len(),
                "{edit}: past the end ({} lines)",
                lines.len()
            );
            let new = split(text);
            lines.splice(n..n, new);
            return Some(());
        }
    }
    if let Some(text) = edit.strip_prefix("insert at line 1: ") {
        lines.splice(0..0, split(text));
        return Some(());
    }
    if let Some((a, rest)) = num(edit, "lines ") {
        let (b, rest) = num(rest, "-").expect("line range");
        let text = rest.strip_prefix(": replace with: ").expect("replace");
        lines.splice(a - 1..b, split(text));
        return Some(());
    }
    if let Some((n, rest)) = num(edit, "line ") {
        if let Some(text) = rest.strip_prefix(": replace with: ") {
            lines.splice(n - 1..n, split(text));
            return Some(());
        }
        let (col, rest) = match num(rest, ", column ") {
            Some((c, r)) => (Some(c), r),
            None => (None, rest),
        };
        let rest = rest.strip_prefix(": replace \"").expect("replace");
        let (from, to) = rest.split_once("\" with ").expect("with");
        let line = &lines[n - 1];
        let start = col.map_or(0, |c| {
            line.char_indices()
                .nth(c - 1)
                .map_or(line.len(), |(i, _)| i)
        });
        let at = line[start..]
            .find(from)
            .map(|i| start + i)
            .unwrap_or_else(|| panic!("{edit}: `{from}` is not on line {n}: {line}"));
        if col.is_some() {
            assert_eq!(
                at, start,
                "{edit}: column does not point at `{from}`: {line}"
            );
        }
        lines[n - 1] = format!("{}{to}{}", &line[..at], &line[at + from.len()..]);
        return Some(());
    }
    None
}

/// Apply `(note, edit)` pairs in order to the vault's files.
fn apply_all(dir: &Path, edits: &[(String, String)]) {
    let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (note, edit) in edits {
        let lines = notes.entry(note.clone()).or_insert_with(|| {
            fs::read_to_string(dir.join(format!("{note}.md")))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        });
        apply(lines, edit).unwrap_or_else(|| panic!("not a mechanical edit: {edit}"));
    }
    for (note, lines) in notes {
        fs::write(dir.join(format!("{note}.md")), lines.join("\n") + "\n").unwrap();
    }
}

/// Times `[[target]]` / `[[target|...]]` is written in a note.
fn links_to(dir: &Path, note: &str, target: &str) -> usize {
    let text = fs::read_to_string(dir.join(format!("{note}.md"))).unwrap();
    text.matches(&format!("[[{target}]]")).count()
        + text.matches(&format!("[[{target}|")).count()
        + text.matches(&format!("[[{target}\\|")).count()
}

/// Apply every suggestion in order; each link is then written once, the YAML still parses,
/// and nothing points nowhere.
fn apply_suggestions(dir: &Path, s: &[Value]) {
    let edits: Vec<(String, String)> = s
        .iter()
        .map(|x| {
            (
                x["note"].as_str().unwrap().to_string(),
                x["edit"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    apply_all(dir, &edits);
    for x in s {
        let (note, target) = (x["note"].as_str().unwrap(), x["target"].as_str().unwrap());
        assert_eq!(links_to(dir, note, target), 1, "{note} -> {target}: {s:?}");
    }
    let f = items(dir, &["check"]);
    assert!(
        f.iter()
            .all(|x| x["rule"] != "frontmatter-invalid" && x["rule"] != "unresolved-link"),
        "{f:?}"
    );
}

#[test]
fn suggest_edits_apply_in_output_order() {
    // A link inserted under ## Related shifts the mention lines below it.
    let v = vault(
        "link_style = \"full-path\"\n",
        &[
            (
                "alpha.md",
                "---\ntype: project\nstack: [rustlang, zigzag]\n---\n# Alpha\n\nAlpha talks about things.\n\n## Related\n\n- [[gamma]]\n\n## Notes\n\nHere we mention Beta Thing and also Delta Thing in prose. Zeta Thing too.\n",
            ),
            (
                "beta.md",
                "---\ntype: project\naliases: [\"Beta Thing\"]\nstack: [rustlang, zigzag]\n---\n# Beta\n\nBeta links [[alpha]] and [[gamma]].\n",
            ),
            (
                "delta.md",
                "---\naliases: [\"Delta Thing\"]\n---\n# Delta\n\nDelta links [[alpha]].\n",
            ),
            (
                "eps.md",
                "---\ntype: project\nstack: [rustlang, zigzag]\n---\n# Eps\n\nLinks [[alpha]], [[gamma]], [[beta]].\n",
            ),
            (
                "gamma.md",
                "# Gamma\n\nGamma links [[alpha]] and [[beta]].\n",
            ),
            (
                "zeta.md",
                "---\naliases: [\"Zeta Thing\"]\n---\n# Zeta\n\nZeta.\n",
            ),
        ],
    );
    let s = items(
        v.path(),
        &["suggest", "alpha", "--min-score", "0.1", "--limit", "20"],
    );
    assert!(
        s.iter().any(|x| x["action"] == "link") && s.iter().any(|x| x["action"] == "mention"),
        "{s:?}"
    );
    apply_suggestions(v.path(), &s);
}

#[test]
fn frontmatter_list_adds_shift_the_body_for_later_edits() {
    let v = vault(
        "link_style = \"full-path\"\nlink_section = \"frontmatter:related\"\n",
        &[
            (
                "s.md",
                "---\nrelated:\n  - \"[[x]]\"\n---\n# s\nWe use Acme Corp daily.\n",
            ),
            ("x.md", "# x\n[[s]]\n"),
            ("a.md", "# a\n[[s]]\n"),
            ("b.md", "# b\n[[s]]\n"),
            ("acme.md", "---\naliases: [Acme Corp]\n---\n# acme\n[[s]]\n"),
        ],
    );
    let s = items(v.path(), &["suggest", "s", "--limit", "20"]);
    assert!(s.iter().any(|x| x["action"] == "mention"), "{s:?}");
    apply_suggestions(v.path(), &s);
}

#[test]
fn suggest_changed_has_one_row_per_note_and_target() {
    let v = vault(
        "link_style = \"full-path\"\n",
        &[
            ("a.md", "# a\nText.\n"),
            ("b.md", "# b\nSee [[a]].\n"),
            ("c.md", "# c\n[[a]] [[b]]\n"),
        ],
    );
    // First use snapshots the vault; then a and b change in the session.
    items(v.path(), &["changed", "--all"]);
    Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(v.path())
        .env("XDG_STATE_HOME", v.path().join(".state"))
        .args(["baseline", "--session", "s1"])
        .assert()
        .success();
    fs::write(v.path().join("a.md"), "# a\nText [[b]].\n").unwrap();
    fs::write(v.path().join("b.md"), "# b\nSee [[c]].\n").unwrap();
    let s = items(
        v.path(),
        &[
            "suggest",
            "--changed",
            "--session",
            "s1",
            "--min-score",
            "0",
        ],
    );
    let mut pairs: Vec<(&str, &str)> = s
        .iter()
        .map(|x| (x["note"].as_str().unwrap(), x["target"].as_str().unwrap()))
        .collect();
    let n = pairs.len();
    pairs.sort_unstable();
    pairs.dedup();
    assert_eq!(pairs.len(), n, "duplicate (note, target) rows: {s:?}");
    apply_suggestions(v.path(), &s);
}

#[test]
fn a_log_note_does_not_ask_for_back_links() {
    let v = vault(
        "link_style = \"full-path\"\nno_backlink_from = [\"Sessions/\"]\n",
        &[
            ("Sessions/s1.md", "# s1\nTouched [[p]].\n"),
            ("p.md", "# p\nA project.\n"),
        ],
    );
    let s = items(v.path(), &["suggest", "Sessions/s1", "--min-score", "0"]);
    assert!(s.iter().all(|x| x["action"] != "backlink"), "{s:?}");
}

#[test]
fn check_fixes_apply_in_output_order() {
    let v = vault(
        "link_style = \"full-path\"\nhubs = [\"INDEX\"]\nno_backlink_from = [\"Sessions/\"]\norphan_exempt = [\"Sessions/\"]\n\
         [[require_link]]\nwhen = { field = \"scope\", matches = \"^project/(.+)$\" }\ntarget = \"Projects/$1/overview\"\n\
         [[require_inbound]]\nwhen = { path = \"^Issues/open/\" }\nfrom = \"INDEX\"\n\
         [[require_inbound]]\nwhen = { path = \"^Projects/[^/]+/overview$\" }\nfrom = \"INDEX\"\n",
        &[
            (
                "INDEX.md",
                "# Index\n\n## Open issues\n\n- [[Issues/open/old]]\n\n## Projects\n\n- [[Projects/bar/overview]]\n\n## Recent\n",
            ),
            (
                "Issues/open/old.md",
                "---\nscope: project/bar\n---\n# old\n[[Projects/bar/overview]]\n",
            ),
            (
                "Issues/open/new1.md",
                "---\nscope: project/foo\n---\n# new1\nText.\n",
            ),
            (
                "Issues/open/new2.md",
                "---\nscope: project/foo\n---\n# new2\nText.\n",
            ),
            ("Projects/bar/overview.md", "# bar\n[[Issues/open/old]]\n"),
            (
                "Projects/foo/overview.md",
                "# foo\n[[Projects/bar/overview]]\n",
            ),
            (
                "Sessions/s1.md",
                "---\nscope: project/foo\n---\n# s1\nWork.\n",
            ),
        ],
    );
    let f = items(v.path(), &["check"]);
    let mut edits: Vec<(String, String)> = Vec::new();
    let mut wanted: Vec<(String, String)> = Vec::new();
    for x in &f {
        let rule = x["rule"].as_str().unwrap();
        let note = x["note"].as_str().unwrap().to_string();
        let fix = x["fix"].as_str().unwrap();
        let (editor, edit) = match fix.strip_prefix("in ").and_then(|r| r.split_once(": ")) {
            Some((from, edit)) => (from.to_string(), edit.to_string()),
            None => (note.clone(), fix.to_string()),
        };
        match rule {
            "missing-required-link" => {
                let target = x["message"].as_str().unwrap();
                let target = &target[target.find("[[").unwrap() + 2..target.find("]]").unwrap()];
                wanted.push((editor.clone(), target.to_string()));
            }
            "missing-required-inbound" => wanted.push((editor.clone(), note.clone())),
            _ => continue,
        }
        edits.push((editor, edit));
    }
    assert!(edits.len() >= 5, "{f:?}");
    apply_all(v.path(), &edits);
    for (editor, target) in &wanted {
        assert_eq!(
            links_to(v.path(), editor, target),
            1,
            "{editor} -> {target}"
        );
    }
    let index = fs::read_to_string(v.path().join("INDEX.md")).unwrap();
    // Each new link sits next to its siblings, in the right section.
    assert_eq!(
        index,
        "# Index\n\n## Open issues\n\n- [[Issues/open/old]]\n- [[Issues/open/new1]]\n- [[Issues/open/new2]]\n\n## Projects\n\n- [[Projects/bar/overview]]\n- [[Projects/foo/overview]]\n\n## Recent\n"
    );
    let f = items(v.path(), &["check"]);
    assert!(
        f.iter().all(|x| x["rule"] != "missing-required-link"
            && x["rule"] != "missing-required-inbound"
            && x["rule"] != "frontmatter-invalid"),
        "{f:?}"
    );
}
