---
name: link
description: Link pass for a Markdown/Obsidian vault managed by wikiknit. Use when a wikiknit hook reports vault notes that changed without a link pass (a held commit or a Stop note), before committing vault changes, or when asked to connect related notes. Adds missing wikilinks between related notes in both directions, adds aliases, then records the pass with `wikiknit mark`.
allowed-tools: Bash(wikiknit:*), Read, Edit, Grep, Glob
---

# wikiknit link pass

Goal: every note changed in this session is linked to the notes it is about, and those notes link back. Links carry meaning in the graph; add the ones a reader would follow, not every word match.

## 1. Gather

Run these from the project (wikiknit finds `wikiknit.toml` by searching upward). The session id is in the hook message; `$WIKIKNIT_SESSION` holds it when the SessionStart hook ran. Pass `--session <id>` to `changed`, `check --changed`, `mentions --changed`, and `mark` whenever `$WIKIKNIT_SESSION` is unset: without it wikiknit uses the newest baseline, which may belong to another open session.

```bash
wikiknit changed            # notes changed this session, not yet marked
wikiknit catalog            # every note: id, type, aliases, one-line summary
wikiknit check --changed    # rule errors in the changed notes
wikiknit mentions --changed # text naming another note (name/alias) without a link
```

Read the vault's own instructions first if it has any (a `CLAUDE.md`, `AGENTS.md`, or templates folder): they say where links go and how they are written. Follow them over the defaults below.

## 2. Decide what each changed note relates to

For each changed note, use what you know about why it changed in this task, plus the catalog:

- Name the things it is about: the project or repo, the system or component, the company, the person, the issue, the report or source it came from, the session that produced it.
- Find the note for each in the catalog. Match by meaning, not only by shared words: a client name in one note can mean a company note that never uses that word.
- Check `mentions` output: each hit is a candidate, not an order. Link it only if the note really refers to that target (a common word that happens to match is not a mention).
- Skip hub/index notes (marked `(hub)` in the catalog) as link targets unless the vault says otherwise.

## 3. Add links in both directions

- In the changed note: link each related note once, in context where it is mentioned, or in its related-notes section (default: a `## Related` list at the end).
- In each related note: add a link back when the relation matters from that side too (a project note listing its sessions and issues, a company note listing the projects done for it). Do not add back-links from a broad note to every small note if the vault keeps those in an index instead.
- Frontmatter list properties: quote every link, `related: ["[[Path/to/note]]"]`. Unquoted `[[...]]` in YAML is not a link.
- Use the link style the config asks for. With `link_style = "full-path"`, write the full vault-relative path without `.md`: `[[Projects/alpha/overview]]`, `[[Projects/alpha/overview|alpha]]`.
- Never put links inside code spans or blocks.

## 4. Grow the vocabulary

When you link something the `mentions` list did not find (a synonym, a product name, an abbreviation, a client name), add that word to the target note's `aliases:` frontmatter, so the next pass finds it automatically. Keep aliases specific: avoid common words, and do not reuse an alias another note already has (`check` reports duplicates).

## 5. Verify and record

```bash
wikiknit check --changed           # must report 0 errors
wikiknit mark --session <id>       # record the pass; held commits and Stop notes clear
```

Run `mark` as its own command. The commit hook checks before a command runs, so `wikiknit mark && git commit` in one command is still held.

Notes you edited for back-links count as changed too: include them in the check, and `mark` records all of them. A pass is recorded for the vault, so other sessions see it too. The list can include notes another session or a person changed while yours ran. Link them too: any session may do the pass for any note, and the vault stays linked either way. Then continue the task (for example, retry the held commit).

If a changed note truly needs no new links (a typo fix, a note already well linked), say so in one line and run `wikiknit mark` anyway.
