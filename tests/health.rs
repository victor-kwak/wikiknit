//! Vault health rules: orphan notes, stale notes, and required inbound links.

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

fn check(dir: &Path, args: &[&str]) -> Vec<Value> {
    let state = dir.join(".state");
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
        .env("XDG_STATE_HOME", &state)
        .env("XDG_CACHE_HOME", state.join("cache"))
        .env_remove("WIKIKNIT_SESSION")
        .env_remove("CLAUDE_PROJECT_DIR")
        .args(["--format", "json", "check"])
        .args(args)
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["items"].as_array().unwrap().clone()
}

fn by_rule<'a>(f: &'a [Value], rule: &str) -> Vec<&'a Value> {
    f.iter().filter(|x| x["rule"] == rule).collect()
}

#[test]
fn orphans_skip_hubs_and_exempt_folders() {
    let v = vault(
        "hubs = [\"INDEX\"]\norphan_exempt = [\"Sessions/\"]",
        &[
            ("INDEX.md", "# Index\n[[a]]\n"),
            ("a.md", "# a\n[[b]]\n"),
            ("b.md", "# b\n[[a]]\n"),
            ("c.md", "# c\n[[a]] and [[b]]\n"),
            ("lonely.md", "# lonely\nno links\n"),
            ("Sessions/s1.md", "# s1\n[[a]]\n"),
        ],
    );
    let f = check(v.path(), &[]);
    let orphans: Vec<&str> = by_rule(&f, "orphan")
        .iter()
        .map(|x| x["note"].as_str().unwrap())
        .collect();
    // INDEX is a hub; Sessions/ is exempt; a and b link each other.
    assert_eq!(orphans, ["c", "lonely"]);
    let c = by_rule(&f, "orphan")[0];
    assert_eq!(c["severity"], "warn");
    assert!(
        c["fix"]
            .as_str()
            .unwrap()
            .contains("link it from [[a]] or [[b]]"),
        "{c}"
    );
    // Selection: only the selected note is checked.
    assert!(by_rule(&check(v.path(), &["a"]), "orphan").is_empty());
}

#[test]
fn stale_dates_by_folder() {
    let v = vault(
        "[[stale]]\nfield = \"last_verified\"\ndays = 30\npath = \"^Systems/\"\n\n\
         [[stale]]\nfield = \"updated\"\ndays = 3650\n",
        &[
            (
                "Systems/old.md",
                "---\nlast_verified: 2001-01-01\n---\n# old\n[[Systems/new]]\n",
            ),
            (
                "Systems/new.md",
                "---\nlast_verified: 2999-01-01\n---\n# new\n[[Systems/old]]\n",
            ),
            (
                "Systems/bad.md",
                "---\ntype: x\nlast_verified: soon\n---\n# bad\n[[Systems/old]]\n",
            ),
            // The Systems/ rule applies here, so `updated` is not checked.
            (
                "Systems/other.md",
                "---\nupdated: 2001-01-01\n---\n# other\n[[Systems/old]]\n",
            ),
            (
                "notes/n.md",
                "---\nupdated: 2001-01-01T10:00:00\n---\n# n\n[[Systems/old]]\n",
            ),
            ("notes/m.md", "# m\nno date at all [[notes/n]]\n"),
        ],
    );
    let f = check(v.path(), &[]);
    let stale: Vec<(&str, u64, &str)> = by_rule(&f, "stale")
        .iter()
        .map(|x| {
            (
                x["note"].as_str().unwrap(),
                x["line"].as_u64().unwrap(),
                x["message"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(stale.len(), 3, "{stale:?}");
    assert_eq!(stale[0].0, "Systems/bad");
    assert_eq!(stale[0].1, 3, "line of the field");
    assert!(stale[0].2.contains("not a date"));
    assert_eq!(stale[1].0, "Systems/old");
    assert!(stale[1].2.contains("days old (limit 30)"), "{}", stale[1].2);
    assert_eq!(stale[2].0, "notes/n");
    let fix = by_rule(&f, "stale")[1]["fix"].as_str().unwrap();
    assert!(fix.starts_with("check the note against its source, then set `last_verified: 20"));
}

#[test]
fn stale_config_errors() {
    let v = vault(
        "[[stale]]\nfield = \"x\"\ndays = 1\npath = \"(\"\n",
        &[("a.md", "# a\n")],
    );
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(v.path())
        .env("XDG_STATE_HOME", v.path().join(".state"))
        .arg("check")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("stale.path"));
}

#[test]
fn required_inbound_links() {
    let v = vault(
        "hubs = [\"INDEX\"]\n\
         [[require_inbound]]\nwhen = { path = \"^Projects/([^/]+)/overview$\" }\nfrom = \"INDEX\"\n\n\
         [[require_inbound]]\nwhen = { path = \"^Research/notes/([^/]+)/\" }\nfrom = \"Research/$1\"\n",
        &[
            (
                "INDEX.md",
                "# Index\n\n## Projects\n\n- [[Projects/a/overview]]\n\n## Other\n",
            ),
            ("Projects/a/overview.md", "# a\n[[Projects/b/overview]]\n"),
            ("Projects/b/overview.md", "# b\n[[Projects/a/overview]]\n"),
            ("Research/r.md", "# r\n[[Research/notes/r/one]]\n"),
            ("Research/notes/r/one.md", "# one\n[[Research/r]]\n"),
            ("Research/notes/r/two.md", "# two\n[[Research/r]]\n"),
            ("Research/notes/q/x.md", "# x\n[[Research/r]]\n"),
        ],
    );
    let f = check(v.path(), &[]);
    let missing: Vec<(&str, &str)> = by_rule(&f, "missing-required-inbound")
        .iter()
        .map(|x| (x["note"].as_str().unwrap(), x["message"].as_str().unwrap()))
        .collect();
    assert_eq!(
        missing,
        [
            ("Projects/b/overview", "must be linked from [[INDEX]]"),
            (
                "Research/notes/q/x",
                "must be linked from [[Research/q]] (that note does not exist yet)"
            ),
            ("Research/notes/r/two", "must be linked from [[Research/r]]"),
        ]
    );
    let b = by_rule(&f, "missing-required-inbound")[0];
    assert_eq!(b["severity"], "error");
    assert_eq!(
        b["fix"],
        "in INDEX: insert after line 5: - [[Projects/b/overview]]"
    );
    // Selecting only the `from` note still finds the missing link (editing it can drop one).
    let only_index = check(v.path(), &["INDEX"]);
    assert_eq!(by_rule(&only_index, "missing-required-inbound").len(), 1);
    // Selecting Research/r finds only the misses its own links decide.
    assert_eq!(
        by_rule(
            &check(v.path(), &["Research/r"]),
            "missing-required-inbound"
        )
        .len(),
        1
    );
}

#[test]
fn required_inbound_edge_cases() {
    let v = vault(
        "hubs = [\"INDEX\"]\n\
         [[require_inbound]]\nwhen = { path = \"^(INDEX|p/.+)$\" }\nfrom = \"./INDEX.md\"\n",
        &[
            ("INDEX.md", "# Index\n- [[p/a]]\n"),
            ("p/a.md", "# a\n[[p/b]]\n"),
            ("p/b.md", "# b\n[[p/a]]\n"),
        ],
    );
    // `./INDEX.md` names INDEX; INDEX itself is skipped (from == note).
    let f = check(v.path(), &[]);
    let m = by_rule(&f, "missing-required-inbound");
    assert_eq!(m.len(), 1, "{m:?}");
    assert_eq!(m[0]["note"], "p/b");
    assert_eq!(m[0]["fix"], "in INDEX: insert after line 2: - [[b]]");
    // Only INDEX selected: the finding is about INDEX, at the line to edit.
    let f = check(v.path(), &["INDEX"]);
    let m = by_rule(&f, "missing-required-inbound");
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["note"], "INDEX");
    assert_eq!(m[0]["line"], 2);
    assert_eq!(m[0]["message"], "must link [[p/b]] (require_inbound)");
    // A note linked only from a hub is not an orphan.
    assert!(by_rule(&f, "orphan").is_empty());
    assert!(
        by_rule(&check(v.path(), &[]), "orphan").is_empty(),
        "p/a is linked from INDEX and p/b"
    );
}

#[test]
fn stale_required_field() {
    let v = vault(
        "[[stale]]\nfield = \"last_verified\"\ndays = 30\nrequired = true\n",
        &[
            ("a.md", "# a\n[[b]]\n"),
            ("b.md", "---\nlast_verified: 2999-01-01\n---\n# b\n[[a]]\n"),
        ],
    );
    let f = check(v.path(), &[]);
    let s = by_rule(&f, "stale");
    assert_eq!(s.len(), 1);
    assert_eq!(s[0]["note"], "a");
    assert_eq!(s[0]["message"], "has no `last_verified` date");
}

#[test]
fn required_inbound_from_is_an_id_not_an_alias() {
    let v = vault(
        "[[require_inbound]]\nwhen = { path = \"^R/notes/([^/]+)/\" }\nfrom = \"$1\"\n",
        &[
            ("R/notes/q/x.md", "# x\n[[Other/thing]]\n"),
            (
                "Other/thing.md",
                "---\naliases: [q]\n---\n# thing\n[[R/notes/q/x]]\n",
            ),
        ],
    );
    let f = check(v.path(), &[]);
    let m = by_rule(&f, "missing-required-inbound");
    assert_eq!(m.len(), 1);
    // `q` is an alias of Other/thing, not a note id.
    assert_eq!(
        m[0]["message"],
        "must be linked from [[q]] (that note does not exist yet)"
    );
}

#[test]
fn code_that_names_no_note_is_not_a_link_in_code() {
    let v = vault(
        "",
        &[
            (
                "a.md",
                "# a\nConfig `[[stale]]` table, and `[[b]]` in code. [[b]]\n",
            ),
            ("b.md", "# b\n[[a]]\n"),
        ],
    );
    let f = check(v.path(), &[]);
    let m = by_rule(&f, "link-in-code");
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0]["message"].as_str().unwrap().starts_with("[[b]]"));
}

#[test]
fn several_required_links_into_one_note_create_the_section_once() {
    let v = vault(
        "[[require_link]]\nwhen = { path = \"^s$\" }\ntarget = \"a\"\n\n\
         [[require_link]]\nwhen = { path = \"^s$\" }\ntarget = \"b\"\n",
        &[
            ("s.md", "# s\nText.\n"),
            ("a.md", "# a\n[[b]] [[s]]\n"),
            ("b.md", "# b\n[[a]]\n"),
        ],
    );
    let f = check(v.path(), &["s"]);
    let fixes: Vec<&str> = by_rule(&f, "missing-required-link")
        .iter()
        .map(|x| x["fix"].as_str().unwrap())
        .collect();
    assert_eq!(fixes.len(), 2, "{f:?}");
    assert_eq!(
        fixes.iter().filter(|x| x.starts_with("append")).count(),
        1,
        "{fixes:?}"
    );
}

#[test]
fn several_required_inbound_links_into_one_note_create_the_section_once() {
    let v = vault(
        "[[require_inbound]]\nwhen = { path = \"^p/\" }\nfrom = \"hub\"\n",
        &[
            ("hub.md", "# hub\nText [[other]].\n"),
            ("other.md", "# other\n[[hub]]\n"),
            ("p/a.md", "# a\n[[p/b]]\n"),
            ("p/b.md", "# b\n[[p/c]]\n"),
            ("p/c.md", "# c\n[[p/a]]\n"),
        ],
    );
    // hub links no p/ note, so all three go in a new link section, created once.
    let f = check(v.path(), &[]);
    let fixes: Vec<&str> = by_rule(&f, "missing-required-inbound")
        .iter()
        .map(|x| x["fix"].as_str().unwrap())
        .collect();
    assert_eq!(fixes.len(), 3, "{f:?}");
    assert_eq!(
        fixes
            .iter()
            .filter(|x| x.starts_with("in hub: append"))
            .count(),
        1,
        "{fixes:?}"
    );
}
