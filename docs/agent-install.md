# Set up wikiknit for a vault (instructions for an agent)

You are a coding agent. The user asked you to set up wikiknit for their Markdown vault (a folder of
`.md` notes with `[[wikilinks]]`, often an Obsidian vault that agents keep as memory). Do these
steps in order. Show the user the output of each command that changes something.

wikiknit checks the links in the vault. It never edits notes. When you are done:

- `wikiknit.toml` describes the vault.
- A git `pre-commit` hook (and `pre-merge-commit`) runs `wikiknit check --staged`. It blocks a
  commit only when that commit adds a link error.
- The project's instruction file (the file your agent reads at the start of a session) tells
  agents how to work with the vault.

## 1. Find the project and the vault

- The project is the git repository the user works in. Run the commands below from its top folder.
- The vault is usually the folder with an `.obsidian/` folder in it. `wikiknit init` finds it.
- If there is no `.obsidian/` folder, or more than one, `init` stops (exit 2) and names the
  folders it saw. Ask the user which folder is the vault, and pass it with `--vault <folder>`. Do
  the same if `init` shows the wrong folder.

## 2. Install the binary

```bash
wikiknit --version
```

If this prints `wikiknit 0.8.0` or later, go to step 3. Otherwise install it from GitHub (this
compiles it; it takes about a minute):

```bash
cargo install --locked --git https://github.com/victor-kwak/wikiknit
```

Prebuilt binaries do not exist yet. If `cargo` is missing, stop and ask the user to install Rust
(https://rustup.rs); do not install it without their consent. `cargo install` puts `wikiknit` in
`~/.cargo/bin`: make sure that folder is on `PATH`, or the git hook prints "wikiknit is not on
PATH; vault not checked" and lets every commit through.

## 3. Run `wikiknit init`

Preview first, then run it:

```bash
wikiknit init --dry-run
wikiknit init
```

`init` does this, and prints each step:

- It writes a starter `wikiknit.toml` at the top of the vault's git repository, with only what the
  vault shows: the link style the notes already use, the index notes that link most of the vault
  (`hubs`), and the templates folder (`ignore`). Each value has its reason in a comment. It keeps
  an existing `wikiknit.toml` that names the vault, and never changes it.
- It installs the `pre-commit` and `pre-merge-commit` hooks in the folder git runs hooks from
  (`core.hooksPath` when the repository sets it, else `.git/hooks`).
- It never overwrites a hook it did not write. If one exists and does not already run this
  check, `init` exits 1 and prints the line to add to it (or, for the pre-commit framework, husky,
  or lefthook, where to add the check in their config). Show that to the user and ask before you
  edit their hook. `init` also does not write into a hooks folder other repositories share (a
  `core.hooksPath` from the global git config, or outside the repository); it prints the line for
  it, which runs only where the config exists.
- If the vault is not in a git repository, it writes the config in the vault folder and skips the
  hooks.

A second run changes nothing and prints "Nothing to change" (unless a hook still needs the
user: then it exits 1 again). Run it again in each new clone: hooks in `.git/hooks` are not
committed.

## 4. Add the agent instructions to the project's instruction file

Find the file your agent reads at session start in this project: `AGENTS.md`, `CLAUDE.md`, or
another one. If the project has more than one, use the one the others point to; if you are not
sure, ask the user. Then:

```bash
wikiknit snippet --into AGENTS.md     # or CLAUDE.md, ...
```

This writes a block between `<!-- wikiknit:start -->` and `<!-- wikiknit:end -->`. If the file
already has the block, it replaces it in place; else it adds the block at the end. Do not edit the
text between the markers: after an upgrade of wikiknit, run the same command again to replace it.
`wikiknit snippet` without `--into` prints the block.

If `wikiknit.toml` is not at the top of the project (for example, the vault is in another
repository), tell the agent where it is: add one line below the block, such as
"wikiknit config: `~/notes/wikiknit.toml`".

## 5. Ask the user for the rules only they know

`init` writes no rules: rules are decisions about the vault, and a guessed rule blocks good
commits. Ask the user which of these they want. Give the examples. Add only the rules they confirm,
to `wikiknit.toml` (the README section "Configure" has every key):

- **require_link**: notes of one kind must link a given note.
  "Must every session log link the project it is about?"
  ```toml
  [[require_link]]
  when = { field = "scope", matches = "^project/(.+)$" }
  target = "Projects/$1/overview"
  ```
- **require_inbound**: notes of one kind must be linked from a given note.
  "Must the index list every open issue?"
  ```toml
  [[require_inbound]]
  when = { path = "^Issues/open/" }
  from = "INDEX"
  ```
- **forbid_link**: links that must not be written.
  "Is a link to a resolved issue under an 'open' heading a mistake?"
  ```toml
  [[forbid_link]]
  target = "^Issues/resolved/"
  near = "(?i)\\bopen\\b"
  message = "a resolved issue is listed as open"
  ```
- **stale**: notes whose date field must be recent.
  "Should notes in `Systems/` be checked again after 30 days (their `last_verified` field)?"
  ```toml
  [[stale]]
  field = "last_verified"
  days = 30
  path = "^Systems/"
  ```
- **drift**: notes that describe a git repository (`path: ~/Repos/x` and
  `verified_commit: <commit>` in their frontmatter) and fall behind it.
  "Do your project notes record the repository and the commit they were checked against?"
  ```toml
  [[drift]]
  path = "^Projects/[^/]+/overview$"
  ```

Also ask about notes that need no inbound link (session logs, an inbox): `orphan_exempt =
["Sessions/"]`.

## 6. Check the vault

```bash
wikiknit check
```

Report the result to the user: the number of errors and warnings, by rule. Do not fix notes in
this step unless the user asks: existing findings do not block commits (the hook blocks only what
a commit adds). Each finding has a `fix` column with the edit to make.

## 7. Commit the setup

Commit only the files this setup added or changed: `wikiknit.toml`, the instruction file, and the
hooks when `init` said to commit them (a tracked `core.hooksPath` folder). Name each path; do not
use `git add -A`. The new hook runs on this commit.

```bash
git add wikiknit.toml AGENTS.md
git commit -m "Set up wikiknit"
```

## Optional: Claude Code plugin

For Claude Code there is a plugin with one skill, `link`: how to link the notes an agent writes.
It is optional; the snippet and the hook do the checking.

```bash
claude plugin marketplace add victor-kwak/wikiknit
claude plugin install wikiknit@wikiknit
```
