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

fn wikiknit(dir: &Path, state: &Path) -> Command {
    let mut cmd = Command::cargo_bin("wikiknit").unwrap();
    cmd.current_dir(dir)
        .env("XDG_STATE_HOME", state)
        .env_remove("WIKIKNIT_SESSION")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("CLAUDE_ENV_FILE");
    cmd
}

fn json_out(cmd: &mut Command) -> Value {
    let out = cmd.output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("bad JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
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
    let state = TempDir::new().unwrap();
    let v = fixture("basic");
    let findings = json_out(wikiknit(&v, state.path()).args(["--format", "json", "check"]));

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
    let state = TempDir::new().unwrap();
    let v = fixture("basic");
    wikiknit(&v, state.path()).arg("check").assert().code(1);
    wikiknit(&v, state.path())
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
    let state = TempDir::new().unwrap();
    let findings = json_out(wikiknit(tmp.path(), state.path()).args(["--format", "json", "check"]));
    assert!(!rules_for(&findings, "Sessions/s1").contains(&"link-style".to_string()));
}

#[test]
fn mentions_use_names_and_aliases() {
    let state = TempDir::new().unwrap();
    let v = fixture("basic");
    let found =
        json_out(wikiknit(&v, state.path()).args(["--format", "json", "mentions", "Sessions/s1"]));
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
}

#[test]
fn catalog_and_graph() {
    let state = TempDir::new().unwrap();
    let v = fixture("basic");
    let cat = json_out(wikiknit(&v, state.path()).args(["--format", "json", "catalog"]));
    let alpha = cat
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "Projects/alpha/overview")
        .unwrap();
    assert_eq!(alpha["kind"], "project");
    assert_eq!(alpha["aliases"], json!(["Alpha App"]));
    assert_eq!(alpha["summary"], "The alpha app, built for ACME.");
    assert!(
        cat.as_array()
            .unwrap()
            .iter()
            .all(|e| e["id"] != "Templates/t")
    );

    let g = json_out(wikiknit(&v, state.path()).args(["--format", "json", "graph"]));
    let isolated: Vec<&str> = g["isolated"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(isolated, ["Issues/i1", "Notes/lonely"]);
}

fn hook(dir: &Path, state: &Path, event: &str, input: &Value) -> assert_cmd::assert::Assert {
    wikiknit(dir, state)
        .args(["hook", event])
        .write_stdin(input.to_string())
        .assert()
}

#[test]
fn commit_is_held_until_marked() {
    let vault = copy_fixture("basic");
    let state = TempDir::new().unwrap();
    let env_file = state.path().join("env.sh");
    let v = vault.path();
    let start = json!({"session_id": "s-1", "cwd": v, "hook_event_name": "SessionStart"});
    wikiknit(v, state.path())
        .env("CLAUDE_ENV_FILE", &env_file)
        .args(["hook", "session-start"])
        .write_stdin(start.to_string())
        .assert()
        .success();
    assert!(
        fs::read_to_string(&env_file)
            .unwrap()
            .contains("export WIKIKNIT_SESSION='s-1'")
    );

    let commit = json!({"session_id": "s-1", "cwd": v, "tool_name": "Bash",
                        "tool_input": {"command": "git add -A && git commit -m notes"}});
    hook(v, state.path(), "pre-commit", &commit).success();

    fs::write(
        v.join("Systems/display.md"),
        "# Display\n\nEdited. [[Projects/alpha/overview]]\n",
    )
    .unwrap();
    let held = hook(v, state.path(), "pre-commit", &commit).code(2);
    let msg = String::from_utf8_lossy(&held.get_output().stderr).into_owned();
    assert!(msg.contains("Systems/display"), "{msg}");
    assert!(msg.contains("wikiknit mark"), "{msg}");

    // Other Bash commands pass.
    let ls = json!({"session_id": "s-1", "cwd": v, "tool_input": {"command": "ls"}});
    hook(v, state.path(), "pre-commit", &ls).success();

    let changed = wikiknit(v, state.path())
        .env("WIKIKNIT_SESSION", "s-1")
        .arg("changed")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&changed.stdout).trim(),
        "Systems/display"
    );

    wikiknit(v, state.path())
        .args(["mark", "--session", "s-1"])
        .assert()
        .success();
    hook(v, state.path(), "pre-commit", &commit).success();
}

#[test]
fn stop_nudges_once_when_settled() {
    let vault = copy_fixture("basic");
    let state = TempDir::new().unwrap();
    let v = vault.path();
    let input = |active: bool| json!({"session_id": "s-2", "cwd": v, "stop_hook_active": active});
    hook(v, state.path(), "session-start", &input(false)).success();
    hook(v, state.path(), "stop", &input(false)).success();

    fs::write(v.join("Notes/lonely.md"), "# Lonely\n\nNow edited.\n").unwrap();
    // Turn that made the edit: still in progress, no nudge.
    hook(v, state.path(), "stop", &input(false)).success();
    // Next turn with no further vault edits: nudge.
    hook(v, state.path(), "stop", &input(false)).code(2);
    // Continuation caused by the nudge, and later turns: no repeat.
    hook(v, state.path(), "stop", &input(true)).success();
    hook(v, state.path(), "stop", &input(false)).success();

    // A new edit starts the cycle again.
    fs::write(v.join("Notes/lonely.md"), "# Lonely\n\nEdited twice.\n").unwrap();
    hook(v, state.path(), "stop", &input(false)).success();
    hook(v, state.path(), "stop", &input(false)).code(2);
}

#[test]
fn hooks_ignore_projects_without_config() {
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let input = json!({"session_id": "s-3", "cwd": dir.path(), "tool_input": {"command": "git commit -m x"}});
    for event in ["session-start", "pre-commit", "stop"] {
        hook(dir.path(), state.path(), event, &input).success();
    }
}

#[test]
fn bad_hook_input_does_not_block() {
    let state = TempDir::new().unwrap();
    wikiknit(&fixture("basic"), state.path())
        .args(["hook", "stop"])
        .write_stdin("not json")
        .assert()
        .code(1);
}

#[cfg(unix)]
#[test]
fn state_dir_is_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let state = TempDir::new().unwrap();
    wikiknit(&fixture("basic"), state.path())
        .args(["baseline", "--session", "p-1"])
        .assert()
        .success();
    let root = state.path().join("wikiknit");
    let vault_dir = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    for dir in [&root, &vault_dir] {
        let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    assert!(vault_dir.join("p-1.json").is_file());
}

#[test]
fn no_state_dir_without_home() {
    let state = TempDir::new().unwrap();
    let mut cmd = wikiknit(&fixture("basic"), state.path());
    cmd.env_remove("XDG_STATE_HOME").env_remove("HOME");
    cmd.args(["baseline", "--session", "x"]).assert().code(2);
}
