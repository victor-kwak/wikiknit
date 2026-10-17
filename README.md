# wikiknit

Lint and knit the wikilinks in a Markdown vault (Obsidian, and anything else that uses `[[wikilinks]]`).

A vault's value is in its links, and links are the first thing to go missing: a note gets written, the
graph view shows it floating alone, and nobody notices. wikiknit finds those gaps:

- **check**: broken links, wrong link style, unquoted links in frontmatter, links hidden in code, notes
  with no outgoing link, notes missing a link your rules require, aliases two notes share.
- **mentions**: text that names another note (by its name or an alias) without linking it.
- **catalog**: one line per note (id, type, aliases, summary), to see the whole vault at once.
- **graph**: connected components with index ("hub") notes left out, so isolated clusters show up.

It reads the Markdown files directly. Obsidian does not need to be running. It runs in milliseconds, so it
fits in editor hooks, git hooks, CI, and AI-agent hooks.

## Install

```bash
cargo install --git https://github.com/victor-kwak/wikiknit   # latest from GitHub
cargo install --path .                                       # from a checkout
```

## Use

```bash
wikiknit check                    # every note; exit 1 on errors
wikiknit check Projects/alpha/overview.md
wikiknit mentions                 # unlinked mentions
wikiknit catalog
wikiknit graph
wikiknit --format json check      # machine-readable output for every command
```

wikiknit looks for `wikiknit.toml` in the current directory and its parents. `--vault <dir>` uses
the `wikiknit.toml` in or above that folder that names it as its vault, else defaults.

## Configure

```toml
# wikiknit.toml
vault = "notes"                 # vault folder, relative to this file and inside its folder (default ".")
link_style = "full-path"        # or "shortest" (Obsidian's default; resolution is best-effort)
ignore = ["Templates/"]         # folders not checked (links to notes there still resolve)
hubs = ["INDEX"]                # index notes: not mention targets, left out of graph components
alias_only = ["Systems/"]       # notes named with common words: found in prose by aliases only
min_alias_len = 4               # shorter names and aliases are not matched as mentions
generic_names = ["overview", "index", "readme"]  # stems that take their folder's name

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
  git aliases that run `commit` are caught; text in quotes, heredocs, and comments is not a commit.
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
