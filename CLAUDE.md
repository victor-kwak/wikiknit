# wikiknit

Rust CLI + Claude Code plugin that lints and knits wikilinks in Markdown vaults. User docs: `README.md`.

- Layout: `src/` (lib + `main.rs` CLI), `tests/cli.rs` (end-to-end against `tests/fixtures/<vault>/`), `plugin/` (Claude Code plugin: hooks + `link` and `context` skills), `.claude-plugin/marketplace.json` (this repo as a marketplace).
- Checks before commit: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. After plugin edits: `claude plugin validate ./plugin` and `claude plugin validate .`.
- Tests run the real binary on fixture vaults. No mocks. New rule -> new fixture case.
- Hook contract: exit 0 = allow, exit 2 + stderr = hold with reason, exit 1 = non-blocking error. `pre-commit` fails closed: once it finds a commit in the command, any error (config, state, vault, timeout, panic) exits 2 with the error; the parse itself runs under the deadline, and a command it cannot classify in time is held. The parser must not recurse on input size (long pipelines): keep per-command work iterative. Other hooks exit 1 on internal errors, never 2. Keep the pre-commit deadline (`WIKIKNIT_HOOK_TIMEOUT_MS`, 8 s) below the plugin hook timeout in `plugin/hooks/hooks.json` (10 s).
- The core stays agent-agnostic. Anything Claude-specific lives in `hook.rs` and `plugin/`.
- Install locally: `cargo install --path . --locked` (to `~/.cargo/bin`).
