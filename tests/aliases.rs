//! `aliases`: terms the vault uses for a note that do not name it yet.

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

fn pairs(found: &[Value]) -> Vec<(String, String)> {
    found
        .iter()
        .flat_map(|a| {
            a["terms"].as_array().unwrap().iter().map(|x| {
                (
                    a["target"].as_str().unwrap().to_string(),
                    x["term"].as_str().unwrap().to_string(),
                )
            })
        })
        .collect()
}

fn term<'a>(found: &'a [Value], name: &str) -> &'a Value {
    found
        .iter()
        .flat_map(|a| a["terms"].as_array().unwrap())
        .find(|x| x["term"] == name)
        .unwrap_or_else(|| panic!("no term {name}: {found:?}"))
}

fn edit_for(found: &[Value], target: &str) -> String {
    found
        .iter()
        .find(|a| a["target"] == target)
        .unwrap_or_else(|| panic!("no target {target}: {found:?}"))["edit"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn display_text_titles_and_co_links() {
    let v = vault(
        "hubs = [\"INDEX\"]",
        &[
            (
                "INDEX.md",
                "# Index\nProton VPN [[vpn]] and Kill Switch [[vpn]]\n",
            ),
            // Title differs from the name and another note writes it the same way.
            (
                "vpn.md",
                "---\naliases: [protonvpn]\n---\n# Proton VPN\nThe client and its kill switch.\n",
            ),
            // Display text names the note, in two notes.
            (
                "a.md",
                "# a\nSee [[vpn|the tunnel]] for routing, and [[b]].\n",
            ),
            ("a2.md", "# a2\nAgain [[vpn|the tunnel]].\n"),
            ("b.md", "# b\nWe run Proton VPN everywhere. Also [[a]].\n"),
            // KWin: three notes use it, and every use links `kwin-notes` on the same line.
            ("kwin-notes.md", "# kwin-notes\nCompositor details.\n"),
            ("c.md", "# c\nThe KWin crash: [[kwin-notes]]\n"),
            ("d.md", "# d\nRestart KWin, see [[kwin-notes]]\n"),
            ("e.md", "# e\nA KWin script, per [[kwin-notes]]\n"),
            // Run: capitalized only at the start of bullets; an ordinary word.
            (
                "f.md",
                "# f\n- Run it [[kwin-notes]]\n- we run things, run often\n",
            ),
            ("g.md", "# g\n- Run again [[kwin-notes]]\nthen run more\n"),
            ("h.md", "# h\n- Run now [[kwin-notes]]\nrun\n"),
        ],
    );
    let found = items(v.path(), &["aliases"]);
    let p = pairs(&found);
    assert!(p.contains(&("vpn".into(), "Proton VPN".into())), "{p:?}");
    assert!(p.contains(&("vpn".into(), "the tunnel".into())), "{p:?}");
    assert!(p.contains(&("kwin-notes".into(), "KWin".into())), "{p:?}");
    assert!(p.iter().all(|(_, t)| t != "Run"), "{p:?}");
    // Aliases the note already has are never suggested.
    assert!(
        p.iter().all(|(_, t)| t.to_lowercase() != "protonvpn"),
        "{p:?}"
    );

    let reasons = term(&found, "Proton VPN")["reasons"].to_string();
    assert!(reasons.contains("its title"), "{reasons}");
    // b uses it without linking vpn (the hub INDEX does not count).
    assert!(
        reasons.contains("1 note(s) use it without linking [[vpn]]"),
        "{reasons}"
    );
    // One edit for both terms of vpn, best first.
    assert_eq!(
        edit_for(&found, "vpn"),
        "line 2: replace with: aliases: [protonvpn, \"the tunnel\", \"Proton VPN\"]"
    );
    let kwin = term(&found, "KWin");
    assert!(
        kwin["reasons"]
            .to_string()
            .contains("3 of 3 note(s) using it link [[kwin-notes]]")
    );
    assert_eq!(
        edit_for(&found, "kwin-notes"),
        "insert at line 1: ---\\naliases: [\"KWin\"]\\n---"
    );
    // One note.
    let only = pairs(&items(v.path(), &["aliases", "kwin-notes"]));
    assert_eq!(only, [("kwin-notes".to_string(), "KWin".to_string())]);
}

#[test]
fn related_terms_are_not_aliases() {
    // `Frisco ISD` appears with links to different notes: related, not a name for one.
    let v = vault(
        "",
        &[
            ("school.md", "# school\nx\n"),
            ("canvas.md", "# canvas\nx\n"),
            ("a.md", "# a\nFrisco ISD uses it [[school]]\n"),
            ("b.md", "# b\nFrisco ISD progress [[canvas]]\n"),
            ("c.md", "# c\nFrisco ISD again [[school]]\n"),
            ("d.md", "# d\nFrisco ISD, no link\n"),
        ],
    );
    assert!(items(v.path(), &["aliases"]).is_empty());
}

#[test]
fn weak_link_text_and_common_titles_are_skipped() {
    let v = vault(
        "",
        &[
            // A section link, a path as link text, and text used once.
            ("install.md", "# Installation\nSteps.\n"),
            ("setup.md", "# setup\nx\n"),
            ("one.md", "# one\nx\n"),
            (
                "a.md",
                "# a\n[[install#Linux|Linux]] [docs/setup.md](setup.md) [[one|this note]]\n",
            ),
            (
                "b.md",
                "# b\n[[install#Linux|Linux]] the installation is slow\n",
            ),
            ("c.md", "# c\nan installation guide\n"),
            // Link text written on links to several notes names none of them.
            ("p1.md", "# p1\nx\n"),
            ("p2.md", "# p2\nx\n"),
            ("p3.md", "# p3\nx\n"),
            ("d.md", "# d\n[[p1|Acme Corp]] [[p2|Acme Corp]]\n"),
            ("e.md", "# e\n[[p3|Acme Corp]] [[p1|Acme Corp]]\n"),
            ("f.md", "# f\n[[p2|Acme Corp]] [[p3|Acme Corp]]\n"),
        ],
    );
    let p = pairs(&items(v.path(), &["aliases"]));
    assert!(p.is_empty(), "{p:?}");
}

#[test]
fn edits_keep_the_yaml_valid() {
    let v = vault(
        "",
        &[
            (
                "t.md",
                "---\ntype: x\naliases:\n  - one\n  - two\n---\n# t\n",
            ),
            ("u.md", "---\ntype: x\n---\n# u\n"),
            ("e.md", "---\n---\n# e\n"),
            ("c.md", "---\naliases: [kube] # main alias\n---\n# c\n"),
            ("s.md", "---\naliases: foo, bar\n---\n# s\n"),
            (
                "a.md",
                "# a\n[[t|Tee Note]] [[u|You Note]] [[e|Empty Thing]] [[c|Mullvad]] [[s|Ess Note]] [[t|Teapot]]\n",
            ),
            (
                "b.md",
                "# b\n[[t|Tee Note]] [[u|You Note]] [[e|Empty Thing]] [[c|Mullvad]] [[s|Ess Note]] [[t|Teapot]]\n",
            ),
        ],
    );
    let found = items(v.path(), &["aliases"]);
    assert_eq!(
        edit_for(&found, "t"),
        "insert after line 5:   - \"Teapot\"\\n  - \"Tee Note\""
    );
    assert_eq!(
        edit_for(&found, "u"),
        "insert after line 2: aliases: [\"You Note\"]"
    );
    // Empty frontmatter: after the opening `---`, not after the closing one.
    assert_eq!(
        edit_for(&found, "e"),
        "insert after line 1: aliases: [\"Empty Thing\"]"
    );
    assert_eq!(
        edit_for(&found, "c"),
        "line 2: replace with: aliases: [kube, \"Mullvad\"] # main alias"
    );
    assert_eq!(
        edit_for(&found, "s"),
        "line 2: replace with: aliases: [\"foo, bar\", \"Ess Note\"]"
    );
}

#[test]
fn longer_phrases_do_not_hide_terms_and_sentence_starts_do_not_count() {
    let v = vault(
        "",
        &[
            ("kb.md", "# Kafka Broker\nx\n"),
            ("a.md", "# a\nThe Kafka Broker died.\n"),
            ("b.md", "# b\nThe Kafka Broker died again.\n"),
            // `Overview` only at sentence starts elsewhere: an ordinary word.
            ("ov.md", "# Overview\nx\n"),
            ("c.md", "# c\nOverview of the thing.\n"),
            ("d.md", "# d\nsee below. Overview here.\n"),
            // A block scalar alias: its lines are replaced whole.
            ("bs.md", "---\naliases: |\n  foo\n---\n# bs\n"),
            ("e.md", "# e\n[[bs|Block Name]] [[bs|Block Name]]\n"),
            ("f.md", "# f\n[[bs|Block Name]]\n"),
        ],
    );
    let found = items(v.path(), &["aliases"]);
    let p = pairs(&found);
    assert!(p.contains(&("kb".into(), "Kafka Broker".into())), "{p:?}");
    assert!(p.iter().all(|(t, _)| t != "ov"), "{p:?}");
    // The parsed value makes a whole replacement of the key and its indented lines safe.
    assert_eq!(
        edit_for(&found, "bs"),
        "lines 2-3: replace with: aliases: [\"foo\", \"Block Name\"]"
    );
}
