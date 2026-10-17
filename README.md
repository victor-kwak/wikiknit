# wikiknit

Context for AI agents working on a Markdown vault (Obsidian, and anything else that uses
`[[wikilinks]]`): find the right notes, see how they relate, and keep the links between them complete.

- **search**: ranked full-text search (BM25F over title, aliases, headings, frontmatter, body), with
  filters by type, folder, frontmatter field, tag, and linked notes.
- **links**: the notes linked to or from a note, each with the line the link is written on, so an agent
  sees how two notes relate without opening either.
- **catalog**: one line per note (id, type, tags, aliases, title, summary), filterable.
- **check**: broken links, wrong link style, unquoted links in frontmatter, links hidden in code or HTML,
  notes with no outgoing link, notes missing a link your rules require, aliases two notes share.
- **mentions**: text that names another note (by its name or an alias) without linking it.
- **graph**: connected components with index ("hub") notes left out, so isolated clusters show up.
- **Claude Code plugin**: hooks that hold a vault commit until the changed notes had a link pass.

It reads the Markdown files directly: Obsidian does not need to be running, and wikiknit never edits
notes (agents do; wikiknit tells them what and where). It is fast enough for hooks: on a 5,000-note
vault, `search` takes about 0.1 s and `check` 0.06 s (16 cores).

## Install

```bash
cargo install --git https://github.com/victor-kwak/wikiknit   # latest from GitHub
cargo install --path .                                       # from a checkout
```

## Use

```bash
wikiknit search cloudflare tunnel --type project   # ranked notes, best line of each
wikiknit search '"ssh key"' github --limit 5       # a quoted phrase must appear as written
wikiknit links gatehouse                           # neighbors of a note (id, path, name, or alias)
wikiknit links gatehouse --depth 2 --in            # notes linking to it, two hops
wikiknit catalog --path Projects/ --where status=active
wikiknit check                                     # every note; exit 1 on errors
wikiknit mentions
wikiknit graph
```

Filters (`search`, `links`, `catalog`): `--type`, `--path <prefix>`, `--where key=regex` (frontmatter;
case-insensitive and matching anywhere in the value, so anchor with `^...$` for an exact value; lists match
any element), `--tag` (frontmatter `tags` and inline `#tags`; a tag also matches its nested tags),
`--linked-to <note>`. Repeat a flag to accept any of its values. A note can be named by its id, path,
link target, name, alias, or folder (`Projects/x` for `Projects/x/overview`).

`links` rows give `dir` (`out`, `in`, `both`), `depth`, the note, `via` (the note one hop closer to the
start), `source`/`line`/`context` (where the link is written, and the text around it), and for `both`
also `back_line`/`back_context` (the link back).

### Output

Output is for agents. Each command prints a `# field...` header line, then one tab-separated record
per line, and nothing at all when there are no records (a clean `check` prints nothing). Lists inside a
field are `;`-separated.
`--format json` prints one compact object instead:

```json
{"schema_version":1,"command":"search","items":[{"id":"...","score":6.58,"type":"reference","matched":["body","title"],"line":18,"snippet":"..."}]}
```

`schema_version` changes only when a record's fields change incompatibly.

wikiknit looks for `wikiknit.toml` in the current directory and its parents. `--vault <dir>` uses
the `wikiknit.toml` in or above that folder that names it as its vault, else defaults.

## Configure

```toml
# wikiknit.toml
vault = "notes"                 # vault folder: a relative path inside this file's folder (default ".");
                                # it may be a symlink, but not to /, your home, or a folder above this one
link_style = "full-path"        # or "shortest" (Obsidian's default; resolution is best-effort)
ignore = ["Templates/"]         # folders not checked (links to notes there still resolve)
hubs = ["INDEX"]                # index notes: not mention targets, left out of graph components
alias_only = ["Systems/"]       # notes named with common words: found in prose by aliases only
min_alias_len = 4               # shorter names and aliases are not matched as mentions
generic_names = ["overview", "index", "readme"]  # stems that take their folder's name
stemmer = "english"             # search stemming: a Snowball language, or "none"

# A note whose frontmatter `scope` is "project/<name>" must link Projects/<name>/overview.
[[require_link]]
when = { field = "scope", matches = "^project/(.+)$" }
target = "Projects/$1/overview"

# Research notes must link their report.
[[require_link]]
when = { path = "^Research/notes/([^/]+)/" }
target = "Research/reports/$1"

[severity]                      # per rule: "error", "warn", or "off"
link-in-code = "warn"
```

### Rules

| Rule | Default | Finds |
|---|---|---|
| `unresolved-link` | error | A link to no note or file |
| `link-style` | error | With `full-path`: a short or relative wikilink |
| `ambiguous-link` | warn | A short link several notes match |
| `frontmatter-invalid` | error | Frontmatter that is not valid YAML |
| `frontmatter-unquoted-link` | error | `key: [[x]]` or `- [[x]]` in YAML (a nested list, not a link); quote it |
| `link-in-code` | warn | `[[x]]` inside a code span or block (not a link) |
| `no-outbound-links` | error | A note (not a hub) that links to no other note |
| `missing-required-link` | error | A note matching a `[[require_link]]` rule without that link |
| `duplicate-alias` | error | An alias another note also uses as its name or alias |
| `link-in-html` | warn | `[[x]]` inside raw HTML (not rendered as a link) |
| `file-not-utf8` | warn | A note that is not valid UTF-8 (read with replacement characters) |

### How links resolve

- `[[path/to/note]]`, `[[note]]`, `[[note#Heading]]`, `[[note|text]]`, `![[embed]]`, and `[text](path.md)`.
- Links in frontmatter count when the value is a quoted string: `related: ["[[note]]"]` (as in Obsidian).
- Links inside code spans and blocks, and inside raw HTML, do not count.
- External links are skipped: URLs and any `scheme:` target (`mailto:`, `tel:`, `obsidian:`).
- Symlinked notes and folders are followed. A UTF-8 byte order mark is ignored.
- `full-path` style needs no guessing: a link is the vault-relative path or it is an error. `shortest`
  style follows Obsidian's documented rules (unique name or path suffix, case-insensitive); when several
  notes match it prefers one in the linking note's folder, then the shortest path, and reports
  `ambiguous-link`.

### Search

`search` ranks notes with BM25F: title (H1, file name) and aliases weigh most, then headings, frontmatter
values, and body text. Words are lowercased and stemmed (`stemmer`); Chinese, Japanese, Korean, and Thai
text is indexed as single characters and overlapping pairs. Identifiers like `snapshot.sh` or
`id_ed25519_vectorpoint` are indexed whole, by their parts, and by their leading sub-identifiers
(`id_ed25519`); a query keeps them whole. Links count as their display text, not their path. Term
statistics come from the whole vault, so a filter changes which notes are shown, not their scores. A query that is exactly a note's name, alias, or title
puts that note first, and notes that many others link to get a small boost. Hub notes are left out
unless `--include-hubs`. Each hit shows the fields it matched and its best line.

### Mentions

`mentions` matches each note's name (its file stem, or its folder for generic stems like
`overview`) and its `aliases:` against the text of other notes. Matching ignores case in any
script by lowercasing (`Émile` matches `émile`; it is not full case folding, so `STRASSE` does not
match `straße`), needs word boundaries except in scripts written without spaces
(Chinese, Japanese, Korean, Thai), and skips frontmatter, code, links, URLs, and raw HTML.

## AI agents: keep links complete as notes change

Agents write notes fast and forget to link them. wikiknit tracks which notes changed and asks for a
link pass before the work is committed.

```bash
wikiknit baseline --session <id>  # start a session record (hooks do this for you)
wikiknit changed                  # notes changed in this session that need a link pass
wikiknit changed --all            # every note that needs one, from any session or editor
wikiknit check --changed
wikiknit mentions --changed
wikiknit mark                     # record the link pass (--all: every pending note)
```

A note *needs a link pass* when its content differs from its content at its last pass. That record
is per vault, so a pass in one session counts for every session. Each session also records the vault
as it was when it started, so it is asked only about notes that changed while it ran. Without
`--session` and `$WIKIKNIT_SESSION`, wikiknit uses the one session active in the last 12 hours, and
refuses to guess when there are several.

State lives in `$XDG_STATE_HOME/wikiknit/` (or `~/.local/state/wikiknit/`), not in the vault, so it
works with or without git.

### Claude Code plugin

This repo is also a Claude Code plugin marketplace. The plugin adds:

- **SessionStart hook**: records the session start and exports `WIKIKNIT_SESSION` for later commands.
  After `/clear` (a new session id), notes still pending from before carry over.
- **PreToolUse hook on Bash**: a `git commit` in the vault's repository while changed notes lack a link
  pass is held, with the list of notes and a pointer to the skill. It parses the command: commits
  behind `if`/`for`/`&&`, wrappers (`sudo`, `timeout`, `xargs`, `env`), `bash -c`, `eval`, `$(...)`, and
  git aliases that run `commit` (from git config or `-c alias.*`) are caught. When it cannot tell (config
  from `GIT_CONFIG_*`, an external `git-<name>` program), it holds. Text in quotes, quoted heredocs, and
  comments is not a commit.
- **Stop hook**: if changed notes sit unlinked for a full turn without further edits, one reminder.
- **`link` skill**: the link pass itself. The agent reads the catalog, decides which notes each changed
  note is about (by meaning, using what it knows about the task), adds links both ways, adds missing
  aliases, re-checks, and runs `wikiknit mark`.

The hooks run only in projects with a `wikiknit.toml`, use no LLM calls, and do nothing if the
`wikiknit` binary is not on `PATH`.

```bash
claude plugin marketplace add victor-kwak/wikiknit   # or a local checkout path
claude plugin install wikiknit@wikiknit
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at
your option.
