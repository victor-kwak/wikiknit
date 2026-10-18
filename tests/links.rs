//! End-to-end tests for `links` and the output format (v0.3 review).

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
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
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
        .args(["--format", "json", "links", "Reference/keys"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(
            r#"{"schema_version":1,"command":"links","note":"Reference/keys","items":[{"dir":"in","#
        ),
        "{text}"
    );
    // An edge with every field set lists them in header order (empty fields are left out).
    assert!(
        text.contains(
            r#"{"dir":"both","depth":1,"id":"Projects/foo/overview","hub":false,"type":"project","via":"Reference/keys","source":"Reference/keys","line":6,"context":"Used by [[Projects/foo/overview|foo]].","back_line":7,"back_context":"See [[Reference/keys]].","count":1,"summary":"#
        ),
        "{text}"
    );
    let header = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(v.path())
        .args(["links", "Reference/keys"])
        .output()
        .unwrap();
    let first = String::from_utf8(header.stdout).unwrap();
    assert!(
        first.starts_with("# dir\tdepth\tid\thub\ttype\tvia\tsource\tline\tcontext\tback_line\tback_context\tcount\tsummary\n"),
        "{first}"
    );
}
