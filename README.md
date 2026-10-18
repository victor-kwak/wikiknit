# wikiknit

Context for AI agents working on a Markdown vault (Obsidian, and anything else that uses
`[[wikilinks]]`): find the right notes, see how they relate, and keep the links between them complete.

- **search**: hybrid search. Words (BM25F over title, aliases, headings, frontmatter, body) and
  meaning (local embeddings, no API), fused into one ranking, with filters by type, folder,
  frontmatter field, tag, and linked notes.
- **context**: a context pack for a note or a question, sized to a token budget: the note (or the
  best search hits), then the notes linked to and from it; whole notes while they fit, then excerpts
  and summaries, each with the reason it is included.
- **suggest**: ranked link candidates for a note, each with its reasons and the exact edit to apply.
- **links**: the notes linked to or from a note, each with the line the link is written on, so an agent
  sees how two notes relate without opening either.
- **catalog**: one line per note (id, type, tags, aliases, title, summary), filterable.
- **check**: broken links, wrong link style, unquoted links in frontmatter, links hidden in code or HTML,
  notes with no outgoing link, orphans (no note links them), notes missing a link your rules require
  (in either direction), stale notes (a date field past its limit), aliases two notes share. Every
  finding comes with a fix (for a broken link: the closest existing notes).
- **aliases**: terms the vault already uses for a note that do not name it yet, with the evidence and
  the frontmatter edit, so `mentions` and `suggest` find them next time.
- **mentions**: text that names another note (by its name or an alias) without linking it.
- **graph**: connected components with index ("hub") notes left out, so isolated clusters show up.
- **Claude Code plugin**: hooks that hold a vault commit until the changed notes had a link pass.

It reads the Markdown files directly: Obsidian does not need to be running, and wikiknit never edits
notes (agents do; wikiknit tells them what and where). It is fast enough for hooks: on a 5,000-note
vault, lexical `search` takes about 0.1 s and `check` 0.06 s (16 cores); hybrid search adds about
0.15 s to load the model.

## Install

```bash
cargo install --git https://github.com/victor-kwak/wikiknit   # latest from GitHub
cargo install --path .                                       # from a checkout
cargo install --path . --no-default-features                 # without semantic search (no ONNX)
```

Semantic search uses [fastembed](https://github.com/Anush008/fastembed-rs) (ONNX Runtime, linked in at
build time). The embedding model downloads on the first `wikiknit embed` into
`$XDG_CACHE_HOME/wikiknit/models` (the default model, `BGESmallENV15Q`, is about 65 MB); everything
after that runs offline.

## Use

```bash
wikiknit embed                                     # build the semantic index (once; then incremental)
wikiknit search cloudflare tunnel --type project   # ranked notes, best line of each
wikiknit search "screen goes black when idle"      # finds notes by meaning, not only by words
wikiknit search '"ssh key"' github --limit 5       # a quoted phrase must appear as written
wikiknit links gatehouse                           # neighbors of a note (id, path, name, or alias)
wikiknit links gatehouse --depth 2 --in            # notes linking to it, two hops
wikiknit catalog --path Projects/ --where status=active
wikiknit context gatehouse --budget 4000           # the note and its neighborhood, in budget
wikiknit context --query "why is DP-1 dropping"    # the best hits for a question, and their links
wikiknit suggest gatehouse                         # link candidates, with reasons and edits
wikiknit aliases                                   # alias candidates, with evidence and edits
wikiknit check                                     # every note; exit 1 on errors; each with a fix
wikiknit mentions
wikiknit graph
```

Filters (`search`, `links`, `catalog`): `--type`, `--path <prefix>`, `--where key=regex` (frontmatter;
case-insensitive and matching anywhere in the value, so anchor with `^...$` for an exact value; lists match
any element), `--tag` (frontmatter `tags` and inline `#tags`; a tag also matches its nested tags),
`--linked-to <note>`. Repeat a flag to accept any of its values. A note can be named by its id, path,
link target, name, alias, or folder (`Projects/x` for `Projects/x/overview`), in every command that
takes notes (`check`, `mentions`, and `mark` too).

`links` rows give `dir` (`out`, `in`, `both`), `depth`, the note, `via` (the note one hop closer to the
start), `source`/`line`/`context` (where the link is written, and the text around it), and for `both`
also `back_line`/`back_context` (the link back).

### Output

Output is for agents. Each command prints a `# field...` header line, then one tab-separated record
per line, and nothing at all when there are no records (a clean `check` prints nothing). Lists inside a
field are `;`-separated.
`--format json` prints one compact object instead:

```json
{"schema_version":1,"command":"search","mode":"hybrid","items":[{"id":"...","score":3.23,"type":"reference","similarity":0.712,"matched":["body","semantic"],"line":18,"snippet":"..."}]}
```

`schema_version` changes only when a record's fields change incompatibly.

`context` is the exception in text form: note text is Markdown with its own headings, so each entry is
wrapped in `<note id=... role=... content=... lines=... score=... tokens=...>` tags with a `<why>` line,
after a `# context budget=... used=...` header; notes left out are listed in `<omitted>`.

wikiknit looks for `wikiknit.toml` in the current directory and its parents. `--vault <dir>` uses
the `wikiknit.toml` in or above that folder that names it as its vault, else defaults; a broken
`wikiknit.toml` in the vault folder itself is an error, not a silent fallback to defaults.

## Configure

```toml
# wikiknit.toml
vault = "notes"                 # vault folder: a relative path inside this file's folder (default ".");
                                # it may be a symlink, but not to /, your home, or a folder above this one
link_style = "full-path"        # or "shortest" (Obsidian's default; resolution is best-effort)
ignore = ["Templates/"]         # folders not checked (links to notes there still resolve)
hubs = ["INDEX"]                # index notes: not mention targets, left out of graph components
                                # (note ids; `./INDEX.md` works too)
alias_only = ["Systems/"]       # notes named with common words: found in prose by aliases only
min_alias_len = 4               # shorter names and aliases are not matched as mentions
generic_names = ["overview", "index", "readme"]  # stems that take their folder's name
stemmer = "english"             # search stemming: a Snowball language, or "none"
embedding_model = "BGESmallENV15Q"  # semantic search model (any fastembed text model)
min_similarity = 0.5            # semantic match: at least this cosine similarity...
semantic_margin = 0.12          # ...and this far above the median note for the query
link_section = "## Related"     # where suggested links go; or "frontmatter:related"
no_backlink_from = ["Sessions/"]  # logs: their links do not ask for a link back
orphan_exempt = ["Sessions/"]   # notes that need no inbound link (the `orphan` rule skips them)

# A note whose frontmatter `scope` is "project/<name>" must link Projects/<name>/overview.
[[require_link]]
when = { field = "scope", matches = "^project/(.+)$" }
target = "Projects/$1/overview"

# Research notes must link their report.
[[require_link]]
when = { path = "^Research/notes/([^/]+)/" }
target = "Research/reports/$1"

# The notes a rule matches must be linked from another note ($1... are capture groups).
[[require_inbound]]
when = { path = "^Projects/([^/]+)/overview$" }
from = "INDEX"

# Freshness: the first rule whose `path` matches a note applies; no `path` matches every note.
[[stale]]
field = "last_verified"         # frontmatter date, YYYY-MM-DD (a time after it is fine)
days = 30
path = "^Systems/"
fix = "run scripts/snapshot.sh"  # optional: the fix to show instead of the default

[[stale]]
field = "last_verified"
days = 120
required = true                 # a note without the field is reported too

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
| `link-in-code` | warn | `[[x]]` naming a note or file inside a code span or block, or `$math$` (not a link); other `[[...]]` in code, such as a TOML `[[table]]`, is left alone |
| `no-outbound-links` | error | A note (not a hub) that links to no other note |
| `missing-required-link` | error | A note matching a `[[require_link]]` rule without that link |
| `missing-required-inbound` | error | A note matching a `[[require_inbound]]` rule that its `from` note does not link; the fix is the edit in the `from` note, next to its links to similar notes. Checked when either note is selected; with only the `from` note selected, the finding is on it |
| `orphan` | warn | A note (not a hub, not in `orphan_exempt`) that no note links to; the fix names the notes it links, the natural places to link it from |
| `stale` | warn | A note whose `[[stale]]` date field is older than `days`, is not a date, or (with `required`) is missing |
| `duplicate-alias` | error | An alias another note also uses as its name or alias |
| `link-in-html` | warn | `[[x]]` inside raw HTML (not rendered as a link) |
| `file-not-utf8` | warn | A note that is not valid UTF-8 (read with replacement characters) |
| `duplicate-note-id` | warn | Two files with one note id (`a.md` and `a.MD`); the exact `.md` file is the note, the other resolves as a file |

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

### Hybrid search

`wikiknit embed` splits each note into chunks (by section, then paragraph, about 175 tokens each,
each prefixed with the note title and heading path) and embeds them with a local model. The index
lives in the state directory, keyed by note content hash: later runs embed only changed notes, and
`search` itself refreshes up to 64 changed chunks before answering (about a second). `search` then fuses
the BM25 ranking and the meaning ranking (each note scored by its best chunk) with reciprocal rank
fusion; an exact name or alias match still comes first, and quoted phrases still must match. A note
counts as a semantic match only if its similarity clears `min_similarity` and stands `semantic_margin`
above the median note for that query (in vaults of 10 or more notes): a real match stands out from the
rest, so a query about nothing in the vault returns nothing. Hits found by meaning show `semantic` in
`matched`, their cosine in `similarity`, and point at their chunk's line. The index is used only when it
covers at least 90% of the notes (otherwise `search` says to run `embed`). Concurrent `embed` and
`search` runs merge their work under a lock. Without an index (or without the
`semantic` feature) `search` is lexical and says so on stderr. `--mode lexical|semantic|hybrid`
chooses explicitly. `embedding_model` picks the model (any fastembed text model, e.g.
`MultilingualE5Small` for many languages).

### Suggest

`suggest <note>` (or `--changed`) ranks link candidates from:

- the note's text naming the candidate (name or alias) without a link: an inline edit
  (`line 6: replace "ACME Corp" with [[Reference/acme|ACME Corp]]`);
- the candidate linking here without a link back, and the note linking to a candidate that does not
  link back (a `backlink` edit in the other note);
- similar meaning (semantic index) and similar text (BM25 on the note's title, aliases, headings);
- shared neighbors in the link graph (Adamic-Adar, hubs left out);
- rare frontmatter values both notes have (`stack: cloudflare`, tags).

Scores are absolute, so they compare across notes: one strong signal (a mention, a link here) scores
2-3, a weak text match alone under 0.5; `--min-score` (default 0.75) leaves out the weak ones. A
candidate needs a strong signal (a mention, a link either way, or note similarity of 0.85 or more) or
two weak ones (similar text, similar meaning, shared neighbors, shared values): one weak signal alone
is often chance. A shared frontmatter value counts by its rarity: a value only the two notes have
counts fully, one that 6 of 70 notes have about half. Template headings shared by many notes and
single shared neighbors do not count as similarity. Notes under `no_backlink_from` (logs, like session
notes) linking here do not ask for a link back. Every row lists its reasons.

Each row has the file to edit, the action (`mention`, `link`, `backlink`), the reasons, and the edit:
where it goes (`link_section`, `## Related` by default, or `frontmatter:related` for a frontmatter list)
and the link text in the vault's `link_style`. Without the heading, a note that already lists links in
a frontmatter key named like it (`related:` for `## Related`) gets the link there. The edits for one
note apply in output order: the first creates a missing section or key (`append after line 12: ...`,
the last line), the others add to it (`insert after line 15 (the `- [[a]]` line under ## Related): ...`),
with line numbers that count the earlier edits. A mention edit names a column when the text also
occurs earlier on the line, and replaces a name written as inline code backticks and all. wikiknit
never applies edits; an agent decides which suggestions are right and makes them.

### Context

`context <note>` or `context --query "..."` builds what an agent should read first, within
`--budget` tokens (default 4000, at least 200). Tokens are estimated: four ASCII characters per token,
one per Chinese, Japanese, Korean, or Thai character, two per other character (accented Latin,
Cyrillic, emoji). The estimate errs high, so a pack stays inside a real model budget.

- Seeds: the note, or the top `--seeds` search hits (default 5; hybrid search, with the same filters as
  `search`; filters apply only to `--query`).
- Then the notes the seeds link (weight 0.6), link the seeds (0.5), or both (0.75), more for repeated
  links, halved per hop (`--depth`, default 1). A note linked to several seeds ranks higher. Hub notes
  are left out; logs (`no_backlink_from`) count less.
- The budget fills in rank order. Seeds share it evenly (what one does not use goes to the next), so
  one long note cannot crowd out the other hits; up to 15% is kept for summaries of the rest. A note
  that fits goes in whole; a seed that does not gets an excerpt (small frontmatter, the title, and the
  section holding the matching line, extended down and then up while it fits; a matching line too long
  to fit is cut to the part around the match); others get their one-line summary; the rest are listed
  as omitted.

Each entry has `role` (`seed` or `linked`), `content` (`full`, `excerpt`, `summary`), the file `lines`
included, its `tokens`, and `why` (the search match, or the link and the line it is written on).

### Aliases

`aliases [note]` lists terms the vault uses for a note that are not yet its name or an alias:

- link text: `[[Reference/acme|ACME Corp]]` names the note `ACME Corp`, when `--min-notes` notes
  (default 2) write it, or one does and other notes use the term in prose. Link text written for
  several different notes, links to a heading (`[[install#Linux|Linux]]`), and paths or URLs as link
  text do not count;
- the note's title (up to four words, not only ordinary words), when another note writes it the
  same way;
- a capitalized term (`Proton VPN`, `KWin`) that at least three notes use, where at least two thirds of
  those lines link that same note. A term whose lines link several different notes is related to them,
  not a name for one, and is left out.

One row per note, with one frontmatter edit that adds all its terms (separate edits to one
`aliases:` line would overwrite each other). Each term (best first) has how many notes use it and
its evidence, including how many notes use it without a link: they become `mentions` once the alias
is added. In text output, `terms` and `notes` are `;`-separated, and `reasons` is
`term: reason, reason` per term with ` | ` between terms. The edit keeps the YAML valid (a trailing
comment stays; a scalar alias is quoted whole; `aliases: ~` or `null` counts as no alias; a value
written over several lines, such as a flow list `[a,` / `b]` or a block scalar, is replaced whole
and the edit names its lines: `lines 2-3: replace with: ...`). A term goes to one note only.

### Mentions

`mentions` matches each note's name (its file stem, or its folder for generic stems like
`overview`) and its `aliases:` against the text of other notes. Matching ignores case in any
script by lowercasing (`Émile` matches `émile`; it is not full case folding, so `STRASSE` does not
match `straße`), needs word boundaries except in scripts written without spaces
(Chinese, Japanese, Korean, Thai), and skips frontmatter, code, math, links, URLs, and raw HTML. An
alias that reads as a domain or path (`example.com`, no spaces) is also found as the whole text of an
inline code span (`` `example.com` ``), where such names are often written; not inside longer code or a
longer domain (`www.example.com.au`).

Links: `$math$` and `$$math$$` are not links (as code). `%% comments %%` are: Obsidian counts links in
comments for backlinks and the graph.

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
