//! End-to-end tests: run the real binary against fixture vaults.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Copy a fixture vault into a temp dir so tests can edit it.
fn copy_fixture(name: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    copy_dir(&fixture(name), tmp.path());
    tmp
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            fs::create_dir_all(&dest).unwrap();
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

fn wikiknit(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("wikiknit").unwrap();
    cmd.current_dir(dir);
    cmd
}

/// The JSON envelope of a `--format json` run.
fn envelope(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!(v["schema_version"], 1);
    v
}

/// The `items` of a `--format json` run.
fn json_out(cmd: &mut Command) -> Value {
    envelope(cmd)["items"].clone()
}

fn rules_for(findings: &Value, note: &str) -> Vec<String> {
    let mut r: Vec<String> = findings
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["note"] == note)
        .map(|f| f["rule"].as_str().unwrap().to_string())
        .collect();
    r.sort();
    r
}

#[test]
fn check_reports_each_rule() {
    let v = fixture("basic");
    let findings = json_out(wikiknit(&v).args(["--format", "json", "check"]));

    assert_eq!(
        rules_for(&findings, "Sessions/s1"),
        [
            "link-in-code",
            "link-style",
            "missing-required-link",
            "unresolved-link"
        ]
    );
    assert_eq!(
        rules_for(&findings, "Issues/i1"),
        ["frontmatter-unquoted-link", "no-outbound-links"]
    );
    assert_eq!(rules_for(&findings, "Notes/dup"), ["duplicate-alias"]);
    assert_eq!(rules_for(&findings, "Reference/acme"), ["duplicate-alias"]);
    assert_eq!(rules_for(&findings, "Notes/lonely"), ["no-outbound-links"]);
    for clean in ["INDEX", "Projects/alpha/overview", "Systems/display"] {
        assert!(
            rules_for(&findings, clean).is_empty(),
            "{clean} should be clean"
        );
    }
    // Ignored folder is never reported.
    assert!(rules_for(&findings, "Templates/t").is_empty());

    let style = findings
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule"] == "link-style")
        .unwrap();
    assert!(
        style["message"]
            .as_str()
            .unwrap()
            .contains("[[Reference/acme]]")
    );
    assert_eq!(style["line"], 8);
}

#[test]
fn check_exit_codes() {
    let v = fixture("basic");
    wikiknit(&v).arg("check").assert().code(1);
    wikiknit(&v)
        .args(["check", "Projects/alpha/overview", "Systems/display.md"])
        .assert()
        .success();
}

#[test]
fn shortest_style_accepts_short_links() {
    let tmp = copy_fixture("basic");
    let cfg = tmp.path().join("wikiknit.toml");
    let text = fs::read_to_string(&cfg)
        .unwrap()
        .replace("full-path", "shortest");
    fs::write(&cfg, text).unwrap();
    let findings = json_out(wikiknit(tmp.path()).args(["--format", "json", "check"]));
    assert!(!rules_for(&findings, "Sessions/s1").contains(&"link-style".to_string()));
}

#[test]
fn mentions_use_names_and_aliases() {
    let v = fixture("basic");
    let found = json_out(wikiknit(&v).args(["--format", "json", "mentions", "Sessions/s1"]));
    let pairs: Vec<(String, String)> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["term"].as_str().unwrap().into(),
                m["target"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(pairs.contains(&("Alpha App".into(), "Projects/alpha/overview".into())));
    // Alias-only note: matched by its alias, not by its common-word name.
    assert!(pairs.contains(&("DP-1".into(), "Systems/display".into())));
    assert!(!pairs.iter().any(|(t, _)| t == "display"));
    // Code spans are not prose.
    assert!(!pairs.iter().any(|(t, _)| t.contains("Reference")));

    // Names inside URLs are not mentions.
    let in_urls = json_out(wikiknit(&v).args(["--format", "json", "mentions", "Notes/lonely"]));
    assert_eq!(in_urls, json!([]));
}

#[test]
fn catalog_and_graph() {
    let v = fixture("basic");
    let cat = json_out(wikiknit(&v).args(["--format", "json", "catalog"]));
    let alpha = cat
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "Projects/alpha/overview")
        .unwrap();
    assert_eq!(alpha["type"], "project");
    assert_eq!(alpha["aliases"], json!(["Alpha App"]));
    assert_eq!(alpha["summary"], "The alpha app, built for ACME.");
    assert!(
        cat.as_array()
            .unwrap()
            .iter()
            .all(|e| e["id"] != "Templates/t")
    );

    let g = json_out(wikiknit(&v).args(["--format", "json", "graph"]));
    let isolated: Vec<&str> = g
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == "isolated")
        .map(|r| r["ids"][0].as_str().unwrap())
        .collect();
    assert_eq!(isolated, ["Issues/i1", "Notes/lonely"]);
}

#[test]
fn text_output_is_a_header_and_tab_separated_rows() {
    let v = fixture("basic");
    let out = wikiknit(&v).arg("check").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("# note\tline\tseverity\trule\tmessage\tfix")
    );
    for line in lines {
        assert_eq!(line.split('\t').count(), 6, "{line}");
    }
    assert!(out.stderr.is_empty(), "no chatter on stderr");
    // Clean output is empty.
    let clean = wikiknit(&v)
        .args(["check", "Systems/display"])
        .output()
        .unwrap();
    assert!(clean.stdout.is_empty());
}

#[test]
fn links_lists_neighbors_with_the_linking_line() {
    let v = fixture("basic");
    let env = envelope(wikiknit(&v).args(["--format", "json", "links", "alpha"]));
    assert_eq!(env["note"], "Projects/alpha/overview");
    let edges = env["items"].as_array().unwrap().clone();
    let find = |id: &str| edges.iter().find(|e| e["id"] == id).cloned();
    let acme = find("Reference/acme").expect("acme");
    assert_eq!(acme["dir"], "both");
    assert_eq!(acme["source"], "Projects/alpha/overview");
    assert!(
        acme["context"]
            .as_str()
            .unwrap()
            .contains("[[Reference/acme|ACME]]")
    );
    let display = find("Systems/display").expect("display");
    assert_eq!(display["dir"], "in");
    assert_eq!(display["source"], "Systems/display");
    assert_eq!(find("INDEX").expect("hub")["hub"], true);
    // --out only, and depth 2 does not expand through the hub.
    let out = json_out(wikiknit(&v).args(["--format", "json", "links", "alpha", "--out"]));
    assert!(out.as_array().unwrap().iter().all(|e| e["dir"] == "out"));
    let deep = json_out(wikiknit(&v).args([
        "--format",
        "json",
        "links",
        "Systems/display",
        "--depth",
        "2",
    ]));
    let ids: Vec<&str> = deep
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(
        ids.contains(&"Reference/acme"),
        "reached through alpha: {ids:?}"
    );
    assert!(
        !ids.contains(&"Notes/lonely"),
        "not through the hub: {ids:?}"
    );
    // Unknown and ambiguous references are errors that say why.
    let bad = wikiknit(&v).args(["links", "nope"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no note matches"));
}

#[test]
fn catalog_filters() {
    let v = fixture("basic");
    let ids = |args: &[&str]| -> Vec<String> {
        let mut full = vec!["--format", "json", "catalog"];
        full.extend_from_slice(args);
        json_out(wikiknit(&v).args(&full))
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&["--type", "reference"]), ["Reference/acme"]);
    assert_eq!(ids(&["--path", "Projects"]), ["Projects/alpha/overview"]);
    assert_eq!(ids(&["--where", "scope=^project/"]), ["Sessions/s1"]);
    assert_eq!(
        ids(&["--linked-to", "Reference/acme"]),
        [
            "INDEX",
            "Notes/dup",
            "Projects/alpha/overview",
            "Sessions/s1"
        ]
    );
    assert_eq!(ids(&["--tag", "infra"]), ["Systems/display"]);
    assert_eq!(
        ids(&["--type", "reference", "--type", "system"]),
        ["Reference/acme", "Systems/display"]
    );
}
