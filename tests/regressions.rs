//! End-to-end regressions for bugs found in the pre-release review. Each test builds a small
//! vault in a temp dir and runs the real binary.

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

/// A temp vault with `wikiknit.toml` and the given files.
fn vault(config: &str, files: &[(&str, &[u8])]) -> TempDir {
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

/// The `items` of a `--format json` run.
fn json_of(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    v["items"].clone()
}

fn rules(findings: &Value) -> Vec<(String, String)> {
    findings
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["note"].as_str().unwrap().to_string(),
                f["rule"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn mention_pairs(found: &Value) -> Vec<(String, String)> {
    found
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["term"].as_str().unwrap().to_string(),
                m["target"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn non_utf8_note_is_a_finding_not_a_failure() {
    let v = vault(
        "",
        &[("a.md", b"# a\n[[b]]\n"), ("b.md", b"# caf\xe9\n[[a]]\n")],
    );
    let out = wk(v.path())
        .args(["--format", "json", "check"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let f: Value = serde_json::from_slice::<Value>(&out.stdout).unwrap()["items"].clone();
    assert_eq!(rules(&f), [("b".to_string(), "file-not-utf8".to_string())]);
}

#[test]
fn links_into_ignored_folders_resolve() {
    let v = vault(
        "ignore = [\"Archive/\"]",
        &[
            ("a.md", b"# a\n[[old]] [[Archive/old]]\n"),
            ("Archive/old.md", b"# old\n"),
        ],
    );
    let f = json_of(wk(v.path()).args(["--format", "json", "check"]));
    // a links only to an ignored note: no unresolved links, but no note link either.
    assert_eq!(
        rules(&f),
        [
            ("a".to_string(), "no-outbound-links".to_string()),
            ("a".to_string(), "orphan".to_string())
        ]
    );
    let msg = wk(v.path())
        .args(["check", "Archive/old.md"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&msg.stderr).contains("ignores"));
}

#[cfg(unix)]
#[test]
fn symlinked_notes_are_notes() {
    let v = vault(
        "",
        &[
            ("a.md", b"# a\n[[linked]]\n"),
            ("real/r.md", b"# r\n[[a]]\n"),
        ],
    );
    std::os::unix::fs::symlink(v.path().join("real/r.md"), v.path().join("linked.md")).unwrap();
    let f = json_of(wk(v.path()).args(["--format", "json", "check"]));
    assert!(rules(&f).iter().all(|(_, r)| r != "unresolved-link"), "{f}");
}

#[test]
fn bom_html_and_schemes() {
    let v = vault(
        "",
        &[
            (
                "bom.md",
                "\u{feff}---\naliases: [Bommy]\n---\n# bom\n[[a]]\n".as_bytes(),
            ),
            (
                "a.md",
                b"# a\n[[bom]] Bommy here. [call](tel:123)\n\n<div>\n[[gone]] Bommy\n</div>\n",
            ),
        ],
    );
    let f = json_of(wk(v.path()).args(["--format", "json", "check"]));
    assert_eq!(rules(&f), [("a".to_string(), "link-in-html".to_string())]);
    let cat = json_of(wk(v.path()).args(["--format", "json", "catalog"]));
    let bom = cat
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "bom")
        .unwrap();
    assert_eq!(bom["aliases"], json!(["Bommy"]));
}

#[test]
fn mention_false_positives_are_gone() {
    let v = vault(
        "",
        &[
            ("index.md", b"# Index\n[[python]]\n"),
            ("python.md", b"# python\n[[index]]\n"),
            (
                "a.md",
                b"# a\nThe search index is fast. [learn python](https://python.org) \
                  <span title=\"python\">x</span>\n",
            ),
        ],
    );
    let found = json_of(wk(v.path()).args(["--format", "json", "mentions", "a"]));
    assert_eq!(found, json!([]));
}

#[test]
fn unicode_mentions() {
    let v = vault(
        "",
        &[
            (
                "people/émile.md",
                "---\naliases: [\"Émile Zola\"]\n---\n# Émile\n[[a]]\n".as_bytes(),
            ),
            (
                "places/tower.md",
                "---\naliases: [\"東京タワー\"]\n---\n# tower\n[[a]]\n".as_bytes(),
            ),
            ("s1.md", b"---\naliases: [Shared]\n---\n# s1\n[[a]]\n"),
            ("s2.md", b"---\naliases: [Shared]\n---\n# s2\n[[a]]\n"),
            (
                "a.md",
                "# a\nI read émile zola. 私は東京タワーに行った。Shared thing.\n[[s1]]\n"
                    .as_bytes(),
            ),
        ],
    );
    let pairs = mention_pairs(&json_of(
        wk(v.path()).args(["--format", "json", "mentions", "a"]),
    ));
    assert!(
        pairs.contains(&("émile zola".into(), "people/émile".into())),
        "{pairs:?}"
    );
    assert!(
        pairs.contains(&("東京タワー".into(), "places/tower".into())),
        "{pairs:?}"
    );
    // s1 is linked already; the other owner of the shared alias is still suggested.
    assert!(pairs.contains(&("Shared".into(), "s2".into())), "{pairs:?}");
    assert!(!pairs.iter().any(|(_, t)| t == "s1"), "{pairs:?}");
}

#[test]
fn vault_flag_uses_the_config_above() {
    let root = TempDir::new().unwrap();
    fs::write(
        root.path().join("wikiknit.toml"),
        "vault = \"notes\"\nignore = [\"Templates/\"]\n",
    )
    .unwrap();
    fs::create_dir_all(root.path().join("notes/Templates")).unwrap();
    fs::write(root.path().join("notes/a.md"), "# a\n[[b]]\n").unwrap();
    fs::write(root.path().join("notes/b.md"), "# b\n[[a]]\n").unwrap();
    fs::write(root.path().join("notes/Templates/t.md"), "[[broken]]\n").unwrap();
    wk(root.path())
        .args(["--vault", "notes", "check"])
        .assert()
        .success();
}

#[test]
fn file_argument_beats_a_root_note_of_the_same_name() {
    let v = vault(
        "",
        &[
            ("dup.md", b"# dup\n[[sub/dup]]\n"),
            ("sub/dup.md", b"# dup2\nno links\n"),
        ],
    );
    wk(&v.path().join("sub"))
        .args(["--vault", "..", "check", "dup.md"])
        .assert()
        .code(1);
}

#[test]
fn vault_outside_the_config_folder_is_rejected() {
    let v = vault("vault = \"/\"", &[]);
    let out = wk(v.path()).arg("check").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside the config"));
}

#[cfg(unix)]
#[test]
fn symlinked_vault_folder_and_symlinks_inside_it() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    // The vault folder itself is a symlink: allowed.
    fs::create_dir_all(elsewhere.path().join("vault/real")).unwrap();
    fs::write(
        elsewhere.path().join("vault/a.md"),
        "---\naliases: [Ay]\n---\n# a\n[[real/r]]\n",
    )
    .unwrap();
    fs::write(
        elsewhere.path().join("vault/real/r.md"),
        "---\naliases: [Ar]\n---\n# r\n[[a]]\n",
    )
    .unwrap();
    symlink(elsewhere.path().join("vault"), root.path().join("notes")).unwrap();
    fs::write(root.path().join("wikiknit.toml"), "vault = \"notes\"\n").unwrap();
    // A folder symlink inside the vault (same notes again) and one pointing outside it.
    symlink(
        elsewhere.path().join("vault/real"),
        elsewhere.path().join("vault/again"),
    )
    .unwrap();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("o.md"), "# outside\n[[nowhere]]\n").unwrap();
    symlink(outside.path(), elsewhere.path().join("vault/out")).unwrap();
    let f = json_of(wk(root.path()).args(["--format", "json", "check"]));
    assert_eq!(f, json!([]), "duplicates or outside notes leaked in");
    let cat = json_of(wk(root.path()).args(["--format", "json", "catalog"]));
    let ids: Vec<&str> = cat
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "real/r"]);
}

#[test]
fn md_suffix_link_to_an_ignored_note_is_not_ambiguous() {
    let v = vault(
        "ignore = [\"Archive/\"]",
        &[
            ("a.md", b"# a\n[[old.md]] [[b]]\n"),
            ("b.md", b"# b\n[[a]]\n"),
            ("Archive/old.md", b"# old\n"),
        ],
    );
    let f = json_of(wk(v.path()).args(["--format", "json", "check"]));
    assert_eq!(f, json!([]));
}

#[cfg(unix)]
#[test]
fn vault_symlink_to_a_folder_above_the_config_is_rejected() {
    let root = TempDir::new().unwrap();
    let repo = root.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    std::os::unix::fs::symlink(root.path(), repo.join("notes")).unwrap();
    fs::write(repo.join("wikiknit.toml"), "vault = \"notes\"\n").unwrap();
    let out = wk(&repo).arg("check").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("folder above the config"));
}
