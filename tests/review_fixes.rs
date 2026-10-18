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
