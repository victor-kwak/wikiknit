//! End-to-end regressions for the v0.5 review findings (mentions, rules, parse, config,
//! vault). Each test builds a small vault in a temp dir and runs the real binary.

use std::fs;
use std::path::Path;
use std::time::Duration;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

/// A temp vault with `wikiknit.toml` and the given files.
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

fn wk(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("wikiknit").unwrap();
    cmd.current_dir(dir);
    cmd
}

/// The `items` of a `--format json` run (panics with stderr on bad output).
fn items(dir: &Path, args: &[&str]) -> Vec<Value> {
    let out = wk(dir)
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

#[test]
fn many_tags_and_links_in_one_note_stay_fast() {
    // 60k lines, each with a tag, a link, and a mention: range checks were quadratic.
    let mut big = String::from("# Big\n");
    for i in 0..60_000 {
        big.push_str(&format!("#t{i} [[other]] see Other `c`\n"));
    }
    let v = vault("", &[("big.md", &big), ("other.md", "# Other\n")]);
    for cmd in [&["catalog"][..], &["mentions"], &["check"], &["graph"]] {
        let out = wk(v.path())
            .args(cmd)
            .timeout(Duration::from_secs(60))
            .output()
            .expect("finishes in time");
        assert!(out.status.code().is_some_and(|c| c < 2), "{cmd:?}: {out:?}");
    }
}

#[test]
fn bare_config_file_name_is_the_current_directory() {
    let v = vault("", &[("a.md", "# a\n")]);
    let out = wk(v.path())
        .args(["--config", "wikiknit.toml", "catalog"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("\na\t"));
}

#[test]
fn vault_flag_reports_a_broken_vault_config() {
    let v = vault("hubs = [\n", &[("a.md", "# a\n")]);
    let out = wk(v.path())
        .args(["--vault", ".", "catalog"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("wikiknit.toml"),
        "{out:?}"
    );
}

#[test]
fn title_skips_comments_in_code_fences() {
    let v = vault(
        "",
        &[(
            "setup.md",
            "Intro.\n\n```sh\n# install the tool\nmake\n```\n\n# Real Title\n",
        )],
    );
    let c = items(v.path(), &["catalog"]);
    assert_eq!(c[0]["title"], "Real Title", "{c:?}");
}

#[test]
fn frontmatter_lines_are_where_the_key_is() {
    let v = vault(
        "",
        &[
            (
                "a.md",
                "---\nunrelated: x\nrelated:\n  - [[b]]\n---\n# a\n[[b]]\n",
            ),
            (
                "c.md",
                "---\n# old: [[gone]]\nup: [\"[[gone]]\"]\n---\n# c\n[[b]]\n",
            ),
            ("b.md", "# b\n[[a]] [[c]]\n"),
        ],
    );
    let f = items(v.path(), &["check"]);
    let line = |note: &str, rule: &str| {
        f.iter()
            .find(|x| x["note"] == note && x["rule"] == rule)
            .unwrap_or_else(|| panic!("{note} {rule}: {f:?}"))["line"]
            .clone()
    };
    assert_eq!(line("a", "frontmatter-unquoted-link"), 3);
    assert_eq!(line("c", "unresolved-link"), 3);
}

#[test]
fn note_ids_that_differ_only_by_extension_case_are_reported() {
    let v = vault(
        "",
        &[
            ("a.MD", "# upper\n[[b]]\n"),
            ("a.md", "# lower\n[[b]]\n"),
            ("b.md", "# b\n[[a]]\n"),
        ],
    );
    let c = items(v.path(), &["catalog"]);
    let ids: Vec<&str> = c.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a", "b"]);
    assert_eq!(
        c[0]["title"], "lower",
        "a.md (exact extension) is the note: {c:?}"
    );
    let f = items(v.path(), &["check"]);
    let dup: Vec<&Value> = f
        .iter()
        .filter(|x| x["rule"] == "duplicate-note-id")
        .collect();
    assert_eq!(dup.len(), 1, "{f:?}");
    assert_eq!(dup[0]["note"], "a");
    assert!(
        dup[0]["message"].as_str().unwrap().contains("a.MD"),
        "{dup:?}"
    );
}

#[test]
fn hubs_are_normalized_like_other_note_references() {
    let v = vault(
        "hubs = [\"./INDEX.md\"]\n",
        &[
            ("INDEX.md", "# Index\n[[a]]\n"),
            ("a.md", "# a\n[[INDEX]]\n"),
        ],
    );
    let c = items(v.path(), &["catalog"]);
    let index = c.iter().find(|e| e["id"] == "INDEX").unwrap();
    assert_eq!(index["hub"], true, "{c:?}");
}

#[test]
fn check_and_mentions_accept_names_and_aliases() {
    let v = vault(
        "",
        &[
            (
                "Projects/gate/overview.md",
                "---\naliases: [Gatehouse]\n---\n# gate\nSee Other here.\n",
            ),
            ("other.md", "# Other\n[[Projects/gate/overview]]\n"),
        ],
    );
    let f = items(v.path(), &["check", "gatehouse"]);
    assert!(
        !f.is_empty() && f.iter().all(|x| x["note"] == "Projects/gate/overview"),
        "{f:?}"
    );
    let m = items(v.path(), &["mentions", "gate"]);
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(m[0]["target"], "other");
    // File paths still work.
    let f = items(v.path(), &["check", "other.md"]);
    assert!(f.iter().all(|x| x["note"] == "other"), "{f:?}");
    let out = wk(v.path()).args(["check", "nosuch"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no note matches"));
}

#[test]
fn fix_hints_quote_ids_with_spaces() {
    // Past 1000 distinct broken targets, the rest get a command to rerun instead of a hint.
    let mut body = String::from("# my note\n[[b]]\n");
    for i in 0..1001 {
        body.push_str(&format!("[[gone{i:04}]]\n"));
    }
    let v = vault("", &[("my note.md", &body), ("b.md", "# b\n[[my note]]\n")]);
    let f = items(v.path(), &["check"]);
    let hint = f
        .iter()
        .find(|x| {
            x["rule"] == "unresolved-link" && x["fix"].as_str().unwrap().starts_with("too many")
        })
        .unwrap_or_else(|| panic!("no rerun hint: {} findings", f.len()));
    assert_eq!(
        hint["fix"], "too many broken links for hints; `wikiknit check 'my note'` gives them",
        "{hint}"
    );
}

#[test]
fn domain_aliases_are_found_in_inline_code() {
    let v = vault(
        "",
        &[
            (
                "Reference/cogno.md",
                "---\naliases: [cognopraxis.com]\n---\n# Cogno\n[[b]]\n",
            ),
            (
                "Reference/canvas.md",
                "---\naliases: [canvas.vectorpoint.io]\n---\n# Canvas\n[[b]]\n",
            ),
            (
                "b.md",
                "# b\nSite: `cognopraxis.com`.\nRun `dig cognopraxis.com` first.\n\n```\ncanvas.vectorpoint.io\n```\nAlso www.cognopraxis.com.au and sub.canvas.vectorpoint.io here.\n",
            ),
        ],
    );
    let m = items(v.path(), &["mentions", "b"]);
    let pairs: Vec<(String, u64, u64)> = m
        .iter()
        .map(|x| {
            (
                x["target"].as_str().unwrap().to_string(),
                x["line"].as_u64().unwrap(),
                x["count"].as_u64().unwrap(),
            )
        })
        .collect();
    // Only the code span that holds just the domain; not a command, a code block, or a
    // longer domain.
    assert_eq!(pairs, [("Reference/cogno".to_string(), 2, 1)], "{m:?}");
}

#[test]
fn links_in_math_are_not_links() {
    let v = vault(
        "",
        &[
            ("a.md", "# a\n[[b]]\n"),
            (
                "b.md",
                "# b\n[[a]] costs $5 and [[a]] $10.\n\n$$\n\\text{[[gone]]}\n$$\n\nInline $x = [[nope]]$ too.\n",
            ),
        ],
    );
    let f = items(v.path(), &["check"]);
    assert!(
        f.iter().all(|x| x["rule"] != "unresolved-link"),
        "math is not links: {f:?}"
    );
    // Dollar amounts are not math: links between them still count.
    let l = items(v.path(), &["links", "b", "--out"]);
    assert_eq!(l.len(), 1, "{l:?}");
}
