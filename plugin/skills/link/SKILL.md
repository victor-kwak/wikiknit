---
name: link
description: Link the notes you write in a Markdown/Obsidian vault checked by wikiknit (a project with a `wikiknit.toml`). Use when creating or editing vault notes, before committing vault changes, or when a wikiknit check (or the git pre-commit hook running it) reports errors.
allowed-tools: Bash(wikiknit:*), Read, Edit, Grep, Glob
---

# Linking vault notes

Goal: each note you create or edit links the notes it is about, and those notes link back where the relation matters from their side. Add the links a reader would follow, not every related word.

wikiknit never edits notes. It reports problems with a fix; you make the edits.

Read the vault's own instructions first (a `CLAUDE.md`, `AGENTS.md`, or templates folder): they say where links go and how they are written. Follow them.

## While you write

- Link what the note is about, by meaning: the project, system, issue, person, company, or source. You know this from the task; no tool knows it better.
- Find link targets with Grep and Read, or `wikiknit catalog` (one line per note; filter with `--path`, `--type`, `--where key=regex`). `wikiknit links <note>` shows what a note links and what links it, with the lines.
- Frontmatter list properties: quote every link, `related: ["[[Path/to/note]]"]`. Unquoted `[[...]]` in YAML is not a link. Never put links inside code spans or blocks.
- When the text names a note by a word the target does not have (a client name, a product, an abbreviation), add that word to the target's `aliases:` frontmatter, so `wikiknit mentions` finds it next time. Keep aliases specific; `check` reports an alias two notes share.

## Before you commit

```bash
wikiknit check --changed       # what your uncommitted changes add; exit 1 on errors
wikiknit mentions --changed    # text you wrote that names a note without linking it
```

Each `check` finding has a `fix` column: the replacement link, the quoted frontmatter line, the closest existing notes for a broken link, or where a required link goes (for `missing-required-inbound` the edit is in the other note). Apply the errors' fixes. For `mentions`, link the ones that are real references.

Warnings, and what to do:

- `orphan`: no note links the new note. Link it from the note it belongs under.
- `stale`: a date such as `last_verified` is past its limit. Update it only if you checked the note against its source in this task.
- `drift`: the repository a project note describes has commits since its `verified_commit`. Before you rely on the note, read the `git log` the fix gives and update what changed; then set `verified_commit` to the new head.

A git pre-commit hook (installed by `wikiknit init`) runs `wikiknit check --staged`. If it blocks a commit, it printed the errors that commit adds: fix them, stage the fixes, and commit again.

## Other folders, and setup

- wikiknit finds `wikiknit.toml` in the current folder and its parents. From another folder or repository, name it: `wikiknit --config <path>/wikiknit.toml check ...`.
- Before you rely on a note about a repository or a live system, run `wikiknit check <note>`: it reports `drift` and `stale` for that note.
- No `wikiknit.toml` yet: follow https://github.com/victor-kwak/wikiknit/blob/main/docs/agent-install.md (`wikiknit init`, then `wikiknit snippet --into CLAUDE.md`).
