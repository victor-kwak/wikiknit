# wikiknit

Rust CLI + Claude Code plugin that lints and knits wikilinks in Markdown vaults. User docs: `README.md`.

- Layout: `src/` (lib + `main.rs` CLI), `tests/cli.rs` (end-to-end against `tests/fixtures/<vault>/`), `plugin/` (Claude Code plugin: hooks + `link` skill), `.claude-plugin/marketplace.json` (this repo as a marketplace).
- Checks before commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. After plugin edits: `claude plugin validate ./plugin` and `claude plugin validate .`.
- Tests run the real binary on fixture vaults. No mocks. New rule -> new fixture case.
- Hook contract: exit 0 = allow, exit 2 + stderr = hold with reason, exit 1 = non-blocking error. Internal errors in `hook` must never exit 2.
- The core stays agent-agnostic. Anything Claude-specific lives in `hook.rs` and `plugin/`.
- Install locally: `cargo install --path . --locked` (to `~/.cargo/bin`).
