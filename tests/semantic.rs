//! End-to-end semantic search with a real embedding model. Ignored by default (it needs the
//! model, a ~65 MB download on first use): `cargo test --test semantic -- --ignored`.
//! Uses the normal model cache, so the download happens once per machine.
#![cfg(feature = "semantic")]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn run(dir: &Path, args: &[&str]) -> Value {
    let out = Command::cargo_bin("wikiknit")
        .unwrap()
        .current_dir(dir)
        .env("XDG_STATE_HOME", dir.join(".state"))
        .args(["--format", "json"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn ids(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
#[ignore = "downloads an embedding model on first use"]
fn hybrid_search_finds_meaning_without_shared_words() {
    let v = TempDir::new().unwrap();
    let p = v.path();
    fs::write(p.join("wikiknit.toml"), "").unwrap();
    fs::write(
        p.join("cats.md"),
        "# Felines\nThey purr, chase mice, and nap in the sun. [[gpu]]\n",
    )
    .unwrap();
    fs::write(
        p.join("gpu.md"),
        "# Graphics card\nDriver update broke the display output. [[taxes]]\n",
    )
    .unwrap();
    fs::write(
        p.join("taxes.md"),
        "# Filing\nQuarterly estimated payments are due in April. [[cats]]\n",
    )
    .unwrap();

    let e = run(p, &["embed"]);
    assert_eq!(e["items"][0]["embedded"], 3);

    // No word of the query appears in any note.
    let lexical = run(p, &["search", "kitten", "behaviour", "--mode", "lexical"]);
    assert!(ids(&lexical).is_empty());
    let hybrid = run(p, &["search", "kitten", "behaviour"]);
    assert_eq!(hybrid["mode"], "hybrid");
    assert_eq!(ids(&hybrid)[0], "cats");
    assert_eq!(hybrid["items"][0]["matched"][0], "semantic");
    assert_eq!(
        hybrid["items"][0]["line"], 2,
        "semantic hits point at their chunk's line"
    );

    // Incremental: nothing to do, then one edited note.
    assert_eq!(run(p, &["embed"])["items"][0]["embedded"], 0);
    fs::write(
        p.join("taxes.md"),
        "# Filing\nMonitors and screens flicker at night. [[cats]]\n",
    )
    .unwrap();
    // `search` refreshes a few changed notes by itself.
    let fresh = run(p, &["search", "screen", "flicker", "--mode", "semantic"]);
    assert_eq!(ids(&fresh)[0], "taxes");
    assert_eq!(
        run(p, &["embed"])["items"][0]["embedded"],
        0,
        "search already re-embedded it"
    );
}

#[test]
#[ignore = "downloads an embedding model on first use"]
fn search_indexes_a_few_new_notes_itself() {
    let v = TempDir::new().unwrap();
    let p = v.path();
    fs::write(p.join("wikiknit.toml"), "").unwrap();
    for i in 0..10 {
        fs::write(
            p.join(format!("n{i}.md")),
            format!("# n{i}\nnote number {i} [[n0]]\n"),
        )
        .unwrap();
    }
    run(p, &["embed"]);
    // Two new notes: below 90% coverage until indexed, but one refresh covers them.
    fs::write(
        p.join("cats.md"),
        "# Felines\nThey purr and chase mice. [[n0]]\n",
    )
    .unwrap();
    fs::write(
        p.join("dogs.md"),
        "# Canines\nThey bark and fetch. [[n0]]\n",
    )
    .unwrap();
    let r = run(
        p,
        &["search", "kittens and their hunting", "--mode", "hybrid"],
    );
    assert_eq!(r["mode"], "hybrid", "{r}");
    assert_eq!(ids(&r).first().map(String::as_str), Some("cats"), "{r}");
}
