<!-- wikiknit:start -->
## Vault links (wikiknit)

<!-- From wikiknit {version}. After an upgrade, run `wikiknit snippet --into <this file>` to replace this block. -->
`wikiknit` checks the links in the vault against its `wikiknit.toml`. It never edits notes: you do, with the `fix` it gives for each finding.

- Before you rely on a note about a repository or a live system, run `wikiknit check <note>`. `drift`: the repository has commits since the note's `verified_commit`; read the `git log` in the fix and update the note. `stale`: a date such as `last_verified` is past its limit; check the note against its source.
- Link each note you write to the notes it is about. Quote links in frontmatter lists: `related: ["[[path/to/note]]"]`.
- Before a vault commit, run `wikiknit check --changed` and `wikiknit mentions --changed`. Apply the `fix` of each error. Link the mentions that are real references.
- Git does not copy hooks into a clone: in a fresh clone of this repository, run `wikiknit init` once (it keeps the existing `wikiknit.toml` and adds the hooks).
- A git pre-commit hook runs `wikiknit check --staged`. If it blocks a commit, that commit adds the errors it printed: apply each `fix`, stage the fixes, and commit again. Warnings do not block. Do not skip the hook with `--no-verify` unless the user tells you to.
- wikiknit finds `wikiknit.toml` in the current folder and its parents. From another folder or repository, name it: `wikiknit --config <path>/wikiknit.toml check ...`.
<!-- wikiknit:end -->
