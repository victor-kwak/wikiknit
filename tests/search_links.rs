//! End-to-end tests for `search`, `links`, filters, and the output format (v0.3 review).

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn vault(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("wikiknit.toml"),
        "hubs = [\"INDEX\"]\nlink_style = \"shortest\"\n",
    )
    .unwrap();
    for (path, body) in files {
        let p = dir.path().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }
    dir
}

fn run(dir: &Path, args: &[&str]) -> Value {
    let state = dir.join(".state");
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
        .env("XDG_STATE_HOME", &state)
        .arg("--format")
        .arg("json")
        .args(args)
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "bad JSON ({e}): {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn ids(items: &Value) -> Vec<String> {
    items["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

fn sample() -> TempDir {
    vault(&[
        (
            "INDEX.md",
            "# Index\n[[Projects/foo/overview]] [[Reference/keys]] [[Notes/tower]] [[Notes/long]]\n",
        ),
        (
            "Projects/foo/overview.md",
            "---\ntype: project\nstack: [rust, cloudflare]\n---\n# foo\nRuns `snapshot.sh` nightly; keys in id_ed25519.\nSee [[Reference/keys]].\n",
        ),
        (
            "Reference/keys.md",
            "---\ntype: reference\naliases: [SSH keys]\n---\n# Keys\nUsed by [[Projects/foo/overview|foo]].\nfoo bar\nbaz qux\n",
        ),
        (
            "Notes/tower.md",
            "# Tower\n私は東京タワーに行った。\n[[Reference/keys]]\n",
        ),
        (
            "Notes/long.md",
            &format!(
                "# Long\n{} cloudflare tunnel here {}\n[[Reference/keys]]\n",
                "filler ".repeat(40),
                "tail ".repeat(40)
            ),
        ),
    ])
}

#[test]
fn identifiers_are_split_into_parts() {
    let v = sample();
    for q in ["snapshot", "snapshot.sh", "ed25519", "id_ed25519"] {
        assert_eq!(
            ids(&run(v.path(), &["search", q])),
            ["Projects/foo/overview"],
            "{q}"
        );
    }
    // A phrase across an identifier still matches: its parts do not shift positions.
    assert_eq!(
        ids(&run(v.path(), &["search", "\"runs snapshot.sh nightly\""])),
        ["Projects/foo/overview"]
    );
}

#[test]
fn snippets_show_the_match() {
    let v = sample();
    let r = run(v.path(), &["search", "tunnel"]);
    let hit = &r["items"][0];
    assert_eq!(hit["id"], "Notes/long");
    let snippet = hit["snippet"].as_str().unwrap();
    assert!(snippet.contains("tunnel"), "{snippet}");
    assert!(
        snippet.starts_with("...") && snippet.ends_with("..."),
        "{snippet}"
    );
}

#[test]
fn filters_do_not_change_scores() {
    let v = sample();
    let all = run(v.path(), &["search", "cloudflare"]);
    let typed = run(v.path(), &["search", "cloudflare", "--type", "project"]);
    let score = |r: &Value, id: &str| {
        r["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .map(|h| h["score"].as_f64().unwrap())
    };
    assert_eq!(
        score(&all, "Projects/foo/overview"),
        score(&typed, "Projects/foo/overview")
    );
    assert_eq!(ids(&typed), ["Projects/foo/overview"]);
}

#[test]
fn cjk_single_characters_and_phrases_within_lines() {
    let v = sample();
    assert_eq!(ids(&run(v.path(), &["search", "京"])), ["Notes/tower"]);
    assert_eq!(
        ids(&run(v.path(), &["search", "東京タワー"])),
        ["Notes/tower"]
    );
    // "bar" and "baz" are on different lines: not a phrase.
    assert!(ids(&run(v.path(), &["search", "\"bar baz\""])).is_empty());
    assert_eq!(
        ids(&run(v.path(), &["search", "\"foo bar\""])),
        ["Reference/keys"]
    );
}

#[test]
fn link_targets_are_not_indexed_as_words() {
    let v = sample();
    // INDEX links into Notes/ and every overview link ends in `overview`; those are paths, not prose.
    assert!(ids(&run(v.path(), &["search", "notes"])).is_empty());
    assert!(ids(&run(v.path(), &["search", "overview"])).is_empty());
}

#[test]
fn links_report_via_and_back_lines() {
    let v = sample();
    let r = run(v.path(), &["links", "Projects/foo", "--depth", "2"]);
    assert_eq!(
        r["note"], "Projects/foo/overview",
        "a folder resolves to its overview"
    );
    let items = r["items"].as_array().unwrap();
    let keys = items.iter().find(|e| e["id"] == "Reference/keys").unwrap();
    assert_eq!(keys["dir"], "both");
    assert_eq!(keys["via"], "Projects/foo/overview");
    assert_eq!(keys["line"], 7);
    assert_eq!(keys["back_line"], 6);
    assert!(keys["back_context"].as_str().unwrap().contains("foo"));
    // Depth 2 through keys: notes linking to keys, with keys as `via`.
    let tower = items.iter().find(|e| e["id"] == "Notes/tower").unwrap();
    assert_eq!(tower["dir"], "in");
    assert_eq!(tower["depth"], 2);
    assert_eq!(tower["via"], "Reference/keys");
    assert_eq!(tower["source"], "Notes/tower");
    // Hubs are a flag, not part of the id.
    let index = items.iter().find(|e| e["hub"] == true).unwrap();
    assert_eq!(index["id"], "INDEX");
}

#[test]
fn json_keys_follow_the_text_header_order() {
    let v = sample();
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(v.path())
        .args(["--format", "json", "search", "cloudflare"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(r#"{"schema_version":1,"command":"search","items":[{"id":"#),
        "{text}"
    );
    let header = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(v.path())
        .args(["search", "cloudflare"])
        .output()
        .unwrap();
    let first = String::from_utf8(header.stdout).unwrap();
    assert!(
        first.starts_with("# id\tscore\ttype\tmatched\tline\tsnippet\n"),
        "{first}"
    );
}
