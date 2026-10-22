# Read-time trust warnings (`wikiknit hook read`): evaluation

Date: 2026-10-22. Branch `claude/read-hook-644d2d`. Vault: `~/Repos/agent_v/agent_vault` (84 notes),
transcripts `~/.claude/projects/*` (all projects, with subagents), 2026-10-08 to 2026-10-22.

## Recommendation

**Do not ship yet (hold on the branch).** The hook works, is fast and quiet, and its warnings are
correct. But in 60 controlled Opus runs it changed no answer: every agent checked the repository and
answered correctly, with or without the hook. The repo's rule is "add a feature only with measured
evidence that it helps an agent". That evidence is not there. See "What would change this".

## What was built

- `wikiknit hook read`: a hidden subcommand for Claude Code's `PostToolUse` and
  `PostToolUseFailure` hooks. Input: the hook JSON on stdin. When the tool read a vault note, it
  prints that note's `stale` and `drift` findings as `hookSpecificOutput.additionalContext`.
  - `Read`: `tool_input.file_path`.
  - `Bash`: the `.md` files the command prints (`cat`, `head`, `tail`, `sed -n`, `less`, `bat`,
    `nl`, `awk`, `grep`, `rg`). The parser follows `cd`s, skips heredoc bodies, redirect
    targets, `sed -i`, `grep -l/-c/-q`, and grep patterns, and expands globs in the last
    component.
  - Vault: the `wikiknit.toml` found from the file's folder upward, up to the git top. Ignored
    and hidden folders are skipped. A symlink resolves to the note it points to.
  - It prints nothing when the note has no finding, when the file is not a vault note, or when
    anything fails. It always exits 0 and never blocks. The removed 0.6 hook names
    (`session-start`, `pre-commit`, `stop`) are still no-ops.
- `rules::trust(cfg, note)`: `stale` + `drift` for one note, without loading the vault.
  `check` output is unchanged.
- `plugin/hooks/hooks.json`:
  - `PostToolUse`, matcher `Read|Bash`.
  - `PostToolUseFailure`, matcher `Bash`.
  - Command: `command -v wikiknit >/dev/null 2>&1 || exit 0; exec wikiknit hook read`, timeout
    10 s.
- Tests: `tests/hook.rs` (16 tests, real binary, real git) and unit tests for the shell parser.
  `cargo fmt`, `clippy -D warnings` and `cargo test` pass. `claude plugin validate` passes.

## Hook contract (checked against the docs and with live probes, Claude Code 2.1.301)

- Read `file_path` is absolute. `tool_response.file.content` holds only the lines that were read,
  so the hook reads the file from disk.
- Output: `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"..."}}` with
  exit 0.
  - Plain stdout reaches only the debug log.
  - Exit 2 shows stderr to the model. Any other non-zero exit shows the user a hook error.
  - The context is capped at 10,000 characters. The hook keeps it under 8,000.
- **A Bash command that exits non-zero fires `PostToolUseFailure`, not `PostToolUse`.** It has the
  same fields and takes the same answer. This was found in the A/B, where 3 hook runs got no
  warning, and is now covered.
- **The hook's `cwd` is where the command ended, not where it started.** `cd sub && cat a.md`
  reports `.../sub`. This was found in the A/B: 4 of 10 hook runs in one arm lost the warning. The
  hook now tries the cwd and the folders above it, and keeps the first start folder whose `cd`s
  lead to the reported cwd. Fixed, and checked end to end in Claude Code.
- Matcher `Read` is an exact match. The default timeout is 600 s, so the plugin sets 10 s. A
  timeout does not block.

## Latency (release build, real vault, includes process spawn)

| Call | p50 | p95 |
|---|---|---|
| Replay of 3,259 real Read/Bash calls (most are not vault reads) | 1.1 ms | 2.5 ms |
| Read of a note with no `[[drift]]` rule (INDEX), via the `sh` guard | 3.5 ms | 3.8 ms |
| Read of a project note (`drift` runs git), via the `sh` guard | 9.3-10.3 ms | 9.8-10.8 ms |

The target was well under 50 ms: met.

## How agents read the vault (transcripts)

Work sessions only: 42 sessions read the vault. Excluded: vault link passes, wikiknit dev and
eval sessions, and security reviews.

| Channel | Note reads |
|---|---|
| `Read` tool | 27 (10%) |
| `Bash` printing a note (`cat`, `sed -n`, `head`, `grep -n`, ...) | 246 (90%) |
| Search calls into the vault (`grep -r`, `rg`, Grep, `notesmd-cli search-content`) | 77 calls |

A `Read`-only hook would see 1 read in 10. That is why the hook also parses Bash. Parser
precision on 30 random real commands where it found a note: about 29 of 30 printed note text.
The doubtful one is a `grep -c` in a truncated command, and `-c` is now skipped.

## Firing rate (historic replay)

Each read was replayed against the note as committed at that time (vault git) and the repo
commits before that time. Current rules: `stale` 30/60 days, `drift` on project notes.

- Warnings in work sessions: 36 reads (32 Bash, 4 Read). First warning per session and note:
  16, in 15 of 42 sessions (15 `drift`, 1 `stale`).
- All 16 are correct: the drift count and the stale age come from git and dates.
- Useful or noise, by hand:
  - **10 useful**: the first read of a project note 13-67 commits behind (gatehouse 35/48/65/67,
    16-18 in 5 sessions; make_fun_drake 13).
    - In 8 of them the agent was about to work in that repo and read its code or `git log`
      anyway.
    - In 2 question-only sessions (a naming brainstorm, 48 behind; "why are gatehouse and
      gatehouse-quiz different repos", 16 behind), the agent answered from the vault with no code
      check (naming) or with a partial one. In those 2 the warning could have changed something.
      Whether the answers were wrong was not checked.
  - **5 low value**: drift of 1-7 commits in sessions that were themselves committing to the repo.
    True, but the agent knows. One session would have repeated the same warning 14 times: the
    hook keeps no state, by design (no temp files, no dedup lost after a compaction).
  - **1 useful**: a `Systems/workstation/input.md` note 154 days stale. The agent ran
    `snapshot.sh` anyway, because the live-state rule requires it.
- Replay with the current vault state: 17 of 3,259 calls fire, all on the one note that drifts
  today (`Projects/wikiknit/overview`, 1 commit).

## Grep gap

Content an agent finds by search, without reading the note, gets no warning.

- In work sessions: 77 search calls. 14 showed vault lines, with 59 note hits. 41 of those notes
  were never read whole in the session.
- Of those 41, only 2 hits (one archived note, twice) would have warned.
- The gap is small: the notes that drift (project overviews) are the ones agents read whole.

## Behavior A/B

Setup:

- A scratch copy of the vault, with `CLAUDE.md` and `.claude/` from agent_v, and paths pointed at
  the copy.
- A scratch repo whose project note is behind: drift 2.
- Headless `claude -p`, Opus, `--plugin-dir` (hook) or not (no hook).
- With the real `CLAUDE.md` rule ("rule": run `wikiknit check` on the project note), or with that
  rule and its drift sentence removed ("no rule").

| Scenario (stale note says ...) | Task | rule/no hook | rule/hook | no rule/no hook | no rule/hook |
|---|---|---|---|---|---|
| 1. port 8080, `ledgerly.yaml` (now 7420, `.toml`) | "what port, config file?" | 5/5 | 5/5 | 5/5 | 5/5 |
| 2. deploy `fly deploy` (now `scripts/ship.sh` to a VPS) | "how do I deploy the hotfix?" | 5/5 | 5/5 | 5/5 | 5/5 |
| 3. hosted on Fly.io (now a VPS) | "2-sentence status blurb" | 5/5 | 5/5 | 5/5 | 5/5 |

Correct answers. The rule/hook cell of scenario 1 is 5 runs: 1 pilot plus 4.

- In every run the agent read the repo (`git log` or the files) before answering. In 20 of 30
  "rule" runs it also ran `wikiknit check`.
- Hook delivered (before the two fixes above): 21 of 30 hook runs. Delivery did not matter: the
  runs without it were right too.
- The hook made no measurable difference. Opus with this `CLAUDE.md` ("the code and the live
  system win over the vault") checks the repo for any repo question. This is a ceiling.
- Sonnet, no rule, scenario 3, no MCP servers: inconclusive.
  - 7 of 10 runs looked in the real `~/Repos/agent_v` vault (read-only), not the scratch copy.
    They found no note, refused to guess, and asked the user.
  - The 3 runs that found the note (all no hook) answered correctly.
  - No hook run reached the note.
- Side effect: in scenario 3 the agents followed the global ghostwriting rule and searched
  Victor's Gmail (`search_threads`, read-only) to match his voice. Later runs used
  `--strict-mcp-config` with no servers.

## New rules: not added

- **Lifecycle (`status: archived/superseded`, `superseded_by`).** 3 archived project notes and 1
  `superseded_by` (`rust_canvas_mcp`). The replay has 3 reads of archived notes (make_fun_marshall
  twice, rust_canvas_mcp once).
  - Each body already says it at the top: "Superseded ... Do not deploy from this repo", "Retired
    2026-10-08. Replaced by ...". A read shows the frontmatter and body, so a warning would repeat
    what the agent just read.
  - Resolved issues live in `Issues/resolved/`, which is in the path. `forbid_link` already
    catches a resolved issue listed as open.
- **`expires: <date>`.** No note has an expiry field. Time-bound facts are written inline ("as of
  2026-10", "unrotated as of 2026-07-29"). If a field is ever wanted, `[[stale]] field =
  "expires", days = 0` already fires once the date is past (the message reads "N days old",
  which is a little odd).
- What earns a read-time warning is what the note cannot say about itself: time passed (`stale`)
  and outside change (`drift`). Both rules already exist.

## Known limits

- Content reached by search only, or by `notesmd-cli`, MCP or `git show`, is not checked.
- No state between calls, so a long session repeats the warning on each read of a drifted note.
- Bash parsing is best effort. It does not guess variables (`$D/x.md`) or command substitution,
  and globs work in the last component only. A `cd -` or a failed `cd` falls back to the
  reported cwd.
- Security: vault text in the context (the `path:` frontmatter, a config `fix`) is one line per
  field, with no control characters, at most 400 characters, with a footer that names its source.
  Reading the note already shows the agent that text, but `additionalContext` comes as a system
  reminder.
- Core vs plugin: `hook.rs` is Claude-specific code in the core binary, like the 0.6 `hook`.
  `CLAUDE.md` now names it as the one exception.

## What would change this

- A run where the hook changes an answer: a weaker model, an agent without the "code wins" rule,
  or a note read in the middle of a long task where the repo is not in focus.
- Field data after a trial install, compared with sessions without it: do agents act on the
  warning (run the `git log`, re-check, fix the note)?
