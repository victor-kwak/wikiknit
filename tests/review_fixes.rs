//! End-to-end regressions for the v0.5 review findings (suggest, aliases, mentions, rules,
//! parse, config, vault). Each test builds a small vault in a temp dir and runs the real binary.

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
    cmd.current_dir(dir)
        .env("XDG_STATE_HOME", dir.join(".state"))
        // Hermetic: no embedding model from the real cache.
        .env("XDG_CACHE_HOME", dir.join(".cache"))
        .env_remove("WIKIKNIT_SESSION")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_ENV_FILE");
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
    for cmd in [
        &["catalog"][..],
        &["mentions"],
        &["check"],
        &["aliases"],
        &["suggest", "big"],
    ] {
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
fn check_mentions_and_mark_accept_names_and_aliases() {
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
    // First use snapshots the vault; then a change to `other` is pending until marked.
    items(v.path(), &["changed", "--all"]);
    fs::write(v.path().join("other.md"), "# Other\n[[gate]]\n").unwrap();
    let marked = items(v.path(), &["mark", "Other"]);
    assert_eq!(marked, [Value::from("other")], "{marked:?}");
}

#[test]
fn fix_hints_quote_ids_with_spaces() {
    let v = vault(
        "",
        &[
            ("my note.md", "# my note\n"),
            ("b.md", "# b\n[[my note]]\n"),
        ],
    );
    let f = items(v.path(), &["check"]);
    let hint = f
        .iter()
        .find(|x| x["note"] == "my note" && x["rule"] == "no-outbound-links")
        .unwrap();
    assert_eq!(
        hint["fix"], "`wikiknit suggest 'my note'` lists link candidates",
        "{hint}"
    );
}

#[test]
fn alias_edits_use_the_parsed_yaml_value() {
    let v = vault(
        "",
        &[
            ("m.md", "---\naliases: [one,\n  two]\ntype: x\n---\n# m\n"),
            ("t.md", "---\naliases: ~\n---\n# t\n"),
            ("n.md", "---\naliases: null # none yet\n---\n# n\n"),
            (
                "a.md",
                "# a\n[[m|Emm Note]] [[t|Tee Note]] [[n|Enn Note]]\n",
            ),
            (
                "b.md",
                "# b\n[[m|Emm Note]] [[t|Tee Note]] [[n|Enn Note]]\n",
            ),
        ],
    );
    let found = items(v.path(), &["aliases"]);
    let edit = |t: &str| {
        found
            .iter()
            .find(|x| x["target"] == t)
            .unwrap_or_else(|| panic!("{t}: {found:?}"))["edit"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        edit("m"),
        "lines 2-3: replace with: aliases: [\"one\", \"two\", \"Emm Note\"]"
    );
    assert_eq!(edit("t"), "line 2: replace with: aliases: [\"Tee Note\"]");
    assert_eq!(
        edit("n"),
        "line 2: replace with: aliases: [\"Enn Note\"] # none yet"
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
    // The suggested edit replaces the whole code span, backticks included.
    let s = items(v.path(), &["suggest", "b"]);
    let cogno = s.iter().find(|x| x["target"] == "Reference/cogno").unwrap();
    assert_eq!(
        cogno["edit"], "line 2: replace \"`cognopraxis.com`\" with [[cogno|cognopraxis.com]]",
        "{cogno}"
    );
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

/// `s` plus three notes linking it (each a `link` suggestion edited in `s`).
fn linked_from_three(config: &str, s: &str) -> TempDir {
    vault(
        config,
        &[
            ("s.md", s),
            ("a.md", "# a\n[[s]]\n"),
            ("b.md", "# b\n[[s]]\n"),
            ("c.md", "# c\n[[s]]\n"),
        ],
    )
}

fn edits_in(s: &[Value], note: &str) -> Vec<(u64, String)> {
    s.iter()
        .filter(|x| x["note"] == note && x["action"] != "mention")
        .map(|x| {
            (
                x["line"].as_u64().unwrap(),
                x["edit"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn several_links_into_one_note_create_the_section_once() {
    let v = linked_from_three("", "# s\nText.\n");
    let s = items(v.path(), &["suggest", "s"]);
    assert_eq!(
        edits_in(&s, "s"),
        [
            (
                2,
                "append after line 2: ## Related\\n\\n- [[a]]".to_string()
            ),
            (5, "insert after line 5: - [[b]]".to_string()),
            (6, "insert after line 6: - [[c]]".to_string()),
        ],
        "{s:?}"
    );
    // An existing section: each edit after the previous one.
    let v = linked_from_three("", "# s\n\n## Related\n\n- [[x]]\n");
    let s = items(v.path(), &["suggest", "s"]);
    let e = edits_in(&s, "s");
    assert_eq!(
        e[0],
        (5, "insert after line 5: - [[a]]".to_string()),
        "{e:?}"
    );
    assert_eq!(e[2], (7, "insert after line 7: - [[c]]".to_string()));
}

#[test]
fn several_links_into_one_frontmatter_key_add_it_once() {
    let cfg = "link_section = \"frontmatter:related\"\n";
    let v = linked_from_three(cfg, "---\ntype: x\n---\n# s\n");
    let s = items(v.path(), &["suggest", "s"]);
    let e = edits_in(&s, "s");
    assert_eq!(
        e,
        [
            (2, "insert after line 2: related: [\"[[a]]\"]".to_string()),
            (
                3,
                "line 3: replace with: related: [\"[[a]]\", \"[[b]]\"]".to_string()
            ),
            (
                3,
                "line 3: replace with: related: [\"[[a]]\", \"[[b]]\", \"[[c]]\"]".to_string()
            ),
        ],
        "{e:?}"
    );
    // No frontmatter: one `---` block, then additions to it.
    let v = linked_from_three(cfg, "# s\n");
    let s = items(v.path(), &["suggest", "s"]);
    let e = edits_in(&s, "s");
    assert_eq!(
        e.iter().filter(|(_, x)| x.contains("---")).count(),
        1,
        "{e:?}"
    );
    assert_eq!(e[0].0, 1);
    assert!(e[1..].iter().all(|(l, _)| *l == 2), "{e:?}");
}

#[test]
fn heading_section_prefers_an_existing_related_frontmatter_list() {
    let v = linked_from_three("", "---\nrelated: [\"[[x]]\"]\n---\n# s\n");
    let s = items(v.path(), &["suggest", "s"]);
    let e = edits_in(&s, "s");
    assert_eq!(e.len(), 3, "{s:?}");
    assert_eq!(
        e[2],
        (
            2,
            "line 2: replace with: related: [\"[[x]]\", \"[[a]]\", \"[[b]]\", \"[[c]]\"]"
                .to_string()
        ),
        "{e:?}"
    );
}

#[test]
fn mention_edit_gives_a_column_when_the_term_occurs_earlier_on_the_line() {
    let v = vault(
        "",
        &[
            ("s.md", "# s\nRun `ACME Corp` then call ACME Corp today.\n"),
            ("acme.md", "---\naliases: [ACME Corp]\n---\n# acme\n[[s]]\n"),
        ],
    );
    let s = items(v.path(), &["suggest", "s"]);
    let m = s.iter().find(|x| x["action"] == "mention").unwrap();
    assert_eq!(
        m["edit"], "line 2, column 27: replace \"ACME Corp\" with [[acme|ACME Corp]]",
        "{m}"
    );
}

#[test]
fn one_weak_signal_is_not_a_suggestion_and_every_one_has_a_reason() {
    // 70 notes (the dogfood vault's size); `stack: bun` in 6 of them. The start note's title
    // matches 10 notes well and the other bun notes weakly (search ranks past 5, which gave no
    // reason). A moderately common value plus that faint text match is not a link.
    let fruits = ["apple", "kiwi", "mango", "pear", "plum"];
    let mut files: Vec<(String, String)> = vec![(
        "bun0.md".into(),
        "---\nstack: [bun]\n---\n# Deploy pipeline\n[[f0]]\n".into(),
    )];
    for (i, fruit) in fruits.iter().enumerate() {
        files.push((
            format!("bun{}.md", i + 1),
            format!("---\nstack: [bun]\n---\n# Deploy {fruit}\n[[f{i}]]\n"),
        ));
    }
    for i in 0..10 {
        files.push((
            format!("step{i}.md"),
            format!("---\nstack: [go]\n---\n# Deploy pipeline step{i}\n[[f{i}]]\n"),
        ));
    }
    for i in 0..54 {
        files.push((
            format!("f{i}.md"),
            format!("# filler{i}\n[[f{}]]\n", (i + 1) % 54),
        ));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let v = vault("", &refs);
    let s = items(v.path(), &["suggest", "bun0"]);
    assert!(
        s.iter()
            .filter(|x| x["note"] == "bun0")
            .all(|x| !x["target"].as_str().unwrap().starts_with("bun")),
        "a shared value and a faint text match are not enough: {s:?}"
    );
    let all = items(
        v.path(),
        &["suggest", "bun0", "--min-score", "0", "--limit", "100"],
    );
    assert!(
        !all.is_empty()
            && all
                .iter()
                .all(|x| !x["reasons"].as_array().unwrap().is_empty()),
        "{all:?}"
    );
}

#[test]
fn an_unknown_session_warns_instead_of_reading_as_nothing_changed() {
    let v = vault("", &[("a.md", "# a\n[[b]]\n"), ("b.md", "# b\n[[a]]\n")]);
    for args in [
        &["changed"][..],
        &["check", "--changed"],
        &["suggest", "--changed"],
    ] {
        let out = wk(v.path())
            .env("WIKIKNIT_SESSION", format!("typo-{}", args[0]))
            .args(args)
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(
            err.contains("had no record") && err.contains("0 note(s) pending"),
            "{args:?}: {err}"
        );
    }
    // Once a session has a record, no warning; pending notes are still listed.
    fs::write(v.path().join("a.md"), "# a\n[[b]] edited\n").unwrap();
    let out = wk(v.path())
        .env("WIKIKNIT_SESSION", "typo-changed")
        .arg("changed")
        .output()
        .unwrap();
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "# id\na\n");
    let out = wk(v.path())
        .env("WIKIKNIT_SESSION", "fresh")
        .arg("changed")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("1 note(s) pending"),
        "{out:?}"
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "# id\na\n");
}
