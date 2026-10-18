//! End-to-end tests for `suggest` and check fix hints (v0.4).

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
        // Keep tests offline: no model in this cache, so suggest uses no semantic signal.
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

fn sample() -> TempDir {
    vault(
        "hubs = [\"INDEX\"]\nlink_style = \"full-path\"\n",
        &[
            (
                "INDEX.md",
                "# Index\n[[Projects/alpha/overview]] [[Reference/acme]] [[Notes/infra]] [[Notes/other]]\n",
            ),
            (
                "Projects/alpha/overview.md",
                "---\ntype: project\nstack: [rust, cloudflare]\n---\n# alpha\nBuilt for ACME Corp.\n\n## Related\n\n- [[Notes/other]]\n",
            ),
            (
                "Reference/acme.md",
                "---\naliases: [ACME Corp]\n---\n# ACME\nClient. Works with [[Projects/alpha/overview]].\n",
            ),
            (
                "Notes/infra.md",
                "---\nstack: [cloudflare]\n---\n# Infra\nTunnels. See [[Notes/other]] and [[Reference/acme]].\n",
            ),
            ("Notes/other.md", "# Other\nNothing here. [[Notes/infra]]\n"),
        ],
    )
}

#[test]
fn suggest_ranks_mentions_backlinks_and_shared_values_with_edits() {
    let v = sample();
    let s = items(v.path(), &["suggest", "alpha"]);
    let find = |target: &str| s.iter().find(|x| x["target"] == target).cloned();

    // The note names ACME Corp, and acme links here: an inline mention edit, ranked first.
    let acme = find("Reference/acme").expect("acme suggested");
    assert_eq!(s[0]["target"], "Reference/acme", "{s:?}");
    assert_eq!(acme["action"], "mention");
    assert_eq!(acme["note"], "Projects/alpha/overview");
    assert_eq!(acme["line"], 6);
    assert_eq!(
        acme["edit"],
        "line 6: replace \"ACME Corp\" with [[Reference/acme|ACME Corp]]"
    );
    let reasons: Vec<&str> = acme["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"links here"), "{reasons:?}");

    // Shared rare frontmatter value and a shared neighbor: a link under ## Related.
    let infra = find("Notes/infra").expect("infra suggested");
    assert_eq!(infra["action"], "link");
    assert_eq!(infra["edit"], "insert after line 10: - [[Notes/infra]]");
    let why = infra["reasons"].to_string();
    assert!(
        why.contains("same stack: cloudflare") && why.contains("shared link"),
        "{why}"
    );

    // alpha links Notes/other, which does not link back: a back-link edit in Notes/other.
    let back = s
        .iter()
        .find(|x| x["action"] == "backlink")
        .expect("backlink");
    assert_eq!(back["note"], "Notes/other");
    assert_eq!(back["target"], "Projects/alpha/overview");
    assert_eq!(
        back["edit"],
        "append after line 2: ## Related\\n\\n- [[Projects/alpha/overview]]"
    );

    // Hubs are never suggested.
    assert!(find("INDEX").is_none());
}

#[test]
fn suggest_follows_a_frontmatter_link_section() {
    let v = vault(
        "link_section = \"frontmatter:related\"\n",
        &[
            ("a.md", "---\nrelated: []\n---\n# a\nMentions Bee here.\n"),
            ("b.md", "---\naliases: [Bee]\n---\n# b\n[[a]]\n"),
        ],
    );
    let s = items(v.path(), &["suggest", "a"]);
    assert_eq!(s[0]["target"], "b");
    let back = items(v.path(), &["suggest", "b"]);
    // b links a; a does not link b: back-link in a's frontmatter `related`.
    let edit = back.iter().find(|x| x["action"] == "backlink").unwrap();
    assert_eq!(edit["note"], "a");
    assert_eq!(edit["edit"], "add \"[[b]]\" to frontmatter `related`");
    assert_eq!(edit["line"], 2);
}

#[test]
fn check_findings_carry_fixes() {
    let v = vault(
        "link_style = \"full-path\"\n[[require_link]]\nwhen = { field = \"scope\", matches = \"^project/(.+)$\" }\ntarget = \"Projects/$1/overview\"\n",
        &[
            (
                "Projects/alpha/overview.md",
                "# alpha\n[[Notes/sess]]\n\n## Related\n\n- [[Notes/sess]]\n",
            ),
            (
                "Notes/sess.md",
                "---\nscope: project/alpha\nrelated:\n  - [[Projects/alpha/overview]]\n---\n# s\nSee [[sess]] and [[Projects/alfa/overview]] and [[Notes/ses]].\n",
            ),
        ],
    );
    let f = items(v.path(), &["check"]);
    let fix = |rule: &str| {
        f.iter()
            .filter(|x| x["rule"] == rule)
            .map(|x| x["fix"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        fix("frontmatter-unquoted-link"),
        ["related: [\"[[Projects/alpha/overview]]\"]"]
    );
    assert_eq!(
        fix("link-style"),
        ["replace with [[Notes/sess]] (keep any |alias or #heading)"]
    );
    let unresolved = fix("unresolved-link");
    assert!(
        unresolved
            .iter()
            .any(|x| x == "did you mean [[Projects/alpha/overview]]"),
        "{unresolved:?}"
    );
    assert!(
        unresolved
            .iter()
            .any(|x| x == "did you mean [[Notes/sess]]"),
        "{unresolved:?}"
    );
    assert_eq!(
        fix("missing-required-link"),
        ["append after line 7: ## Related\\n\\n- [[Projects/alpha/overview]]"]
    );
}

#[test]
fn suggest_uses_the_semantic_index_without_the_model() {
    use wikiknit::vectors::{Index, NoteVectors, path};
    let v = vault(
        "",
        &[
            ("a.md", "# a\nfirst\n"),
            ("b.md", "# b\nsecond\n"),
            ("c.md", "# c\nthird\n"),
        ],
    );
    let root = v.path().canonicalize().unwrap();
    let store = wikiknit::state::Store::open_at(&v.path().join(".state/wikiknit"), &root);
    let mut idx = Index::empty("BGESmallENV15Q", 3);
    for (id, vec) in [
        ("a", [1.0, 0.0, 0.0]),
        ("b", [0.99, 0.141, 0.0]),
        ("c", [0.0, 1.0, 0.0]),
    ] {
        idx.notes.insert(
            id.into(),
            NoteVectors {
                hash: "old".into(),
                lines: vec![2],
                vectors: vec![vec.to_vec()],
            },
        );
    }
    idx.save(&path(store.dir(), "BGESmallENV15Q")).unwrap();
    let s = items(v.path(), &["suggest", "a"]);
    let b = s.iter().find(|x| x["target"] == "b").expect("b by meaning");
    assert!(
        b["reasons"].to_string().contains("similar meaning (0.99)"),
        "{b}"
    );
    assert!(
        s.iter().all(|x| x["target"] != "c"),
        "c is not similar: {s:?}"
    );
}
