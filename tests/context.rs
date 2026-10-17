//! `context`: a budgeted context pack for a note or a query.

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

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
        .env("XDG_STATE_HOME", dir.join(".state"))
        .env("XDG_CACHE_HOME", dir.join(".cache"))
        .args(args)
        .output()
        .unwrap()
}

fn pack(dir: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--format", "json", "context"];
    all.extend(args);
    let out = run(dir, &all);
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn sample() -> TempDir {
    let long = "Filler line about other things.\n".repeat(400);
    vault(
        "hubs = [\"INDEX\"]\nno_backlink_from = [\"Sessions/\"]",
        &[
            ("INDEX.md", "# Index\n[[app]] [[db]] [[ops]]\n"),
            (
                "app.md",
                "---\ntype: project\n---\n# app\nThe app uses [[db]] for storage.\n\n## Deploy\nSee [[ops]].\n",
            ),
            ("db.md", "# db\nPostgres notes. Used by [[app]].\n"),
            ("ops.md", "# ops\nDeploy runbook.\n"),
            ("far.md", "# far\nLinks [[db]] only.\n"),
            ("Sessions/s1.md", "# s1\nWorked on [[app]].\n"),
            (
                "big.md",
                &format!(
                    "# big\n\n## Intro\n{long}\n## Kubernetes rollout\nThe kubernetes rollout steps.\n[[app]]\n"
                ),
            ),
        ],
    )
}

#[test]
fn note_pack_ranks_links_and_fills_the_budget() {
    let v = sample();
    let p = pack(v.path(), &["app", "--budget", "2000"]);
    assert_eq!(p["command"], "context");
    let entries = p["items"].as_array().unwrap();
    let ids: Vec<&str> = entries.iter().map(|e| e["id"].as_str().unwrap()).collect();
    assert_eq!(ids[0], "app");
    assert_eq!(entries[0]["role"], "seed");
    assert_eq!(entries[0]["content"], "full");
    assert_eq!(entries[0]["why"][0], "the note asked for");
    // db links both ways: ranked above ops (linked from app only); the hub is left out;
    // the session log comes after notes linked the same way.
    assert!(!ids.contains(&"INDEX"), "{ids:?}");
    let pos = |id: &str| ids.iter().position(|x| *x == id).unwrap();
    assert!(pos("db") < pos("ops"), "{ids:?}");
    assert!(pos("big") < pos("Sessions/s1") || !ids.contains(&"Sessions/s1"));
    let db = &entries[pos("db")];
    assert!(
        db["why"][0]
            .as_str()
            .unwrap()
            .starts_with("linked from [[app]] (line 5:"),
        "{db}"
    );
    // The large note does not fit whole: a summary.
    assert_eq!(entries[pos("big")]["content"], "summary");
    assert!(p["used"].as_u64().unwrap() <= p["budget"].as_u64().unwrap());
    // Depth 2 reaches far (through db).
    let p2 = pack(v.path(), &["app", "--depth", "2"]);
    let far = p2["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "far")
        .expect("far at depth 2");
    assert!(
        far["why"][0]
            .as_str()
            .unwrap()
            .contains("2 hops from [[app]], via [[db]]")
    );
}

#[test]
fn tight_budget_excerpts_the_seed_and_omits_the_rest() {
    let v = sample();
    let p = pack(
        v.path(),
        &[
            "--query",
            "kubernetes rollout",
            "--budget",
            "400",
            "--mode",
            "lexical",
        ],
    );
    let entries = p["items"].as_array().unwrap();
    let big = &entries[0];
    assert_eq!(big["id"], "big");
    assert_eq!(big["content"], "excerpt");
    let text = big["text"].as_str().unwrap();
    // The matching section, with the title kept.
    assert!(text.starts_with("# big\n...\n"), "{text}");
    assert!(text.contains("## Kubernetes rollout"), "{text}");
    assert!(
        big["why"][0]
            .as_str()
            .unwrap()
            .starts_with("search hit: matched")
    );
    assert!(p["used"].as_u64().unwrap() <= 400, "{}", p["used"]);
    assert_eq!(p["mode"], "lexical");
}

#[test]
fn text_output_uses_note_tags() {
    let v = sample();
    let out = run(v.path(), &["context", "db", "--budget", "2000"]);
    let s = String::from_utf8(out.stdout).unwrap();
    let mut lines = s.lines();
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with("# context budget=2000 used=")
    );
    assert!(
        s.contains("<note id=\"db\" role=\"seed\" content=\"full\" lines=\"1-2\""),
        "{s}"
    );
    assert!(
        s.contains("<why>the note asked for</why>\n# db\nPostgres notes."),
        "{s}"
    );
    assert!(s.contains("</note>"));
}

#[test]
fn context_needs_a_note_or_a_query() {
    let v = sample();
    assert_eq!(run(v.path(), &["context"]).status.code(), Some(2));
    assert_eq!(run(v.path(), &["context", "nope"]).status.code(), Some(2));
}

#[test]
fn budgets_are_never_exceeded() {
    let v = sample();
    for budget in (200..2400).step_by(37) {
        for args in [
            vec!["app".to_string()],
            vec!["big".to_string()],
            vec![
                "--query".into(),
                "kubernetes rollout".into(),
                "--mode".into(),
                "lexical".into(),
            ],
        ] {
            let b = budget.to_string();
            let mut a: Vec<&str> = args.iter().map(String::as_str).collect();
            a.extend(["--budget", &b]);
            let p = pack(v.path(), &a);
            let used = p["used"].as_u64().unwrap();
            assert!(used <= budget as u64, "{a:?}: used {used}");
            let sum: u64 = p["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["tokens"].as_u64().unwrap())
                .sum();
            assert_eq!(sum, used, "{a:?}");
        }
    }
}

#[test]
fn a_long_line_is_cut_around_the_match() {
    let filler = "word ".repeat(3000);
    let v = vault(
        "",
        &[(
            "huge.md",
            &format!("# huge\n\n{filler} quokkaterm appears here {filler}\n"),
        )],
    );
    let p = pack(
        v.path(),
        &[
            "--query",
            "quokkaterm",
            "--budget",
            "600",
            "--mode",
            "lexical",
        ],
    );
    let e = &p["items"][0];
    assert_eq!(e["content"], "excerpt");
    assert!(e["text"].as_str().unwrap().contains("quokkaterm"), "{e}");
    assert!(p["used"].as_u64().unwrap() <= 600);
}

#[test]
fn non_ascii_and_crlf_excerpts() {
    let ru = "Длинная строка про сервер и его настройки.\r\n".repeat(200);
    let v = vault(
        "",
        &[(
            "ru.md",
            &format!(
                "---\r\ntype: x\r\n---\r\n# сервер\r\n\r\n## Введение\r\n{ru}## Перезапуск\r\nкак перезапустить сервер\r\n"
            ),
        )],
    );
    let p = pack(
        v.path(),
        &[
            "--query",
            "перезапустить",
            "--budget",
            "500",
            "--mode",
            "lexical",
        ],
    );
    let e = &p["items"][0];
    assert_eq!(e["content"], "excerpt", "{e}");
    assert!(
        e["text"].as_str().unwrap().contains("как перезапустить"),
        "{e}"
    );
}

#[test]
fn bad_arguments_are_rejected() {
    let v = sample();
    for args in [
        &["context", "app", "--type", "x"][..],
        &["context", "--query", ""],
        &["context", "app", "--depth", "0"],
        &["context", "--query", "x", "--seeds", "0"],
        &["context", "app", "--budget", "10"],
    ] {
        let out = run(v.path(), args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
