---
name: context
description: Gather background from a Markdown/Obsidian vault managed by wikiknit (a project with a wikiknit.toml). Use before answering or acting on anything the vault may already record (a project, system, person, past decision, earlier session, known issue), or when asked what the notes say about a topic. Builds a ranked, token-budgeted context pack with wikiknit context, then reads further only where needed.
allowed-tools: Bash(wikiknit:*), Read, Grep, Glob
---

# wikiknit context

Goal: read what the vault already knows before you act, within a token budget, and know why each note is relevant.

Run commands from the project (wikiknit finds `wikiknit.toml` by searching upward).

## 1. Get a pack

```bash
wikiknit context --query "<the question or topic>" --budget 4000   # a question or topic
wikiknit context <note> --budget 4000                              # a note you know (id, path, name, alias)
```

The pack lists the best notes first. Each `<note>` entry has:

- `role`: `seed` (a search hit, or the note you named) or `linked` (linked to or from a seed),
- `content`: `full`, `excerpt` (small frontmatter, the lead section, and the sections that match the query; `...` marks what is left out), or `summary` (one line),
- `lines`: the file lines included, `why`: the search match (and how much of the query it covers) or the link line that put it there.

`<omitted>` lists ranked notes that did not fit.

`weak=true` in the header (and a note on stderr) means no note matched the question by meaning or by most of its words: the vault probably does not cover it. Say so rather than build on the pack; try other words, or `wikiknit catalog` to see what the vault holds.

To read one known note with its neighbors, focused on a topic: `wikiknit context <note> --query "<topic>"`.

Narrow the seeds with the same filters as `search` (`--type project`, `--path Systems/`, `--where status=active`, `--tag x`). Use `--depth 2` for a wider neighborhood. Raise `--budget` when the task needs detail; lower it for a quick check.

## 2. Read further only where needed

- An `excerpt` or `summary` that matters: `Read` the file at the lines given. Its path is the id plus `.md`, inside the vault folder (the `vault` key of `wikiknit.toml`, relative to that file).
- Something missing: `wikiknit search <words> --limit 10`, `wikiknit links <note>`, or `wikiknit catalog --path <folder>`.

## 3. Trust, then verify

Notes record what was true when written. Check dates such as `last_verified` and the live system or code before you rely on a value. `wikiknit check <note>` reports a `stale` warning when the vault sets freshness limits.
