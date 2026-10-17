---
name: link
description: Link pass for a Markdown/Obsidian vault managed by wikiknit. Use when a wikiknit hook reports vault notes that changed without a link pass (a held commit or a Stop note), before committing vault changes, or when asked to connect related notes. Gets ranked link suggestions with ready-made edits, applies the right ones in both directions, adds aliases, then records the pass with `wikiknit mark`.
allowed-tools: Bash(wikiknit:*), Read, Edit, Grep, Glob
---

# wikiknit link pass

Goal: every note changed in this session is linked to the notes it is about, and those notes link back. Links carry meaning in the graph; add the ones a reader would follow, not every candidate.

wikiknit never edits notes. It tells you what to change and where; you make the edits.

Run commands from the project (wikiknit finds `wikiknit.toml` by searching upward). The session id is in the hook message; `$WIKIKNIT_SESSION` holds it when the SessionStart hook ran. Pass `--session <id>` whenever `$WIKIKNIT_SESSION` is unset (with several sessions open, wikiknit refuses to guess).

Read the vault's own instructions first if it has any (a `CLAUDE.md`, `AGENTS.md`, or templates folder): they say where links go and how they are written. Follow them over the defaults below.

## 1. Fix the rule errors

```bash
wikiknit check --changed
```

Each finding has a `fix` column: the replacement link, the quoted frontmatter line, the closest existing notes for a broken link, or where a required link goes. Apply it.

## 2. Get suggestions

```bash
wikiknit suggest --changed      # every changed note; or: wikiknit suggest <note>
```

Each row is a candidate with:

- `note`: the file to edit, `action`: `mention` (turn text into a link), `link` (add a link), or `backlink` (the other note should link back),
- `reasons`: why (it names the target, the target links here, similar meaning or text, shared links, the same frontmatter value),
- `line` and `edit`: the exact change, in the vault's link style and link section.

Suggestions are candidates, not orders. For each, decide with what you know about why the note changed in this task:

- Apply it when a reader of the note would follow that link: the project, system, company, person, issue, or source the note is about. `mention` and "links here" are usually right; "similar meaning" alone needs judgment.
- For `backlink`, add it when the relation matters from that side too (a project note listing its sessions and issues, a company note listing the projects done for it). Skip it if the vault keeps such lists in an index instead.
- Skip candidates that only share a common word or topic.

## 3. Look for what suggestions cannot know

Suggestions come from the vault's text and links. Your task context knows more. For each changed note, name the things it is about and check they are linked:

```bash
wikiknit links <note>                       # what it links to and what links to it, with the lines
wikiknit search <topic words> --limit 10    # find a note by words or meaning
wikiknit catalog --type project             # or --path Reference/, --where stack=cloudflare
```

## 4. Edit

- Link each related note once, in context where it is mentioned, or in the related-notes section `suggest` names (`## Related` by default).
- Frontmatter list properties: quote every link, `related: ["[[Path/to/note]]"]`. Unquoted `[[...]]` in YAML is not a link.
- Never put links inside code spans or blocks.

## 5. Grow the vocabulary

When you link something no `mention` suggestion found (a synonym, a product name, an abbreviation, a client name), add that word to the target note's `aliases:` frontmatter, so the next pass finds it. Keep aliases specific: avoid common words, and do not reuse an alias another note has (`check` reports duplicates).

## 6. Verify and record

```bash
wikiknit check --changed           # must report nothing
wikiknit mark --session <id>       # record the pass; held commits and Stop notes clear
```

Run `mark` as its own command. The commit hook checks before a command runs, so `wikiknit mark && git commit` in one command is still held.

Notes you edited for back-links count as changed too: they are in the check, and `mark` records all of them. A pass is recorded for the vault, so other sessions see it. The list can include notes another session or a person changed while yours ran. Link them too: any session may do the pass for any note. Then continue the task (for example, retry the held commit).

If a changed note truly needs no new links (a typo fix, a note already well linked), say so in one line and run `wikiknit mark` anyway.
