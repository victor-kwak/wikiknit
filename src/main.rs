use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use wikiknit::config::{CONFIG_FILE, Config};
use wikiknit::filter::Filter;
use wikiknit::hook::{self, Outcome};
use wikiknit::links::{self, Direction};
use wikiknit::state::{self, Store};
use wikiknit::{Vault, catalog, graph, mentions, rules, search};

/// `context --query`: hits after the first must score this share of the best hit...
const SEED_SCORE: f64 = 0.5;
/// ...and match by meaning or cover this share of the query's weight.
const SEED_COVERAGE: f64 = 0.6;

/// Version of the `--format json` envelope and record fields.
const SCHEMA_VERSION: u32 = 1;

/// `println!` that exits quietly when stdout is closed (`wikiknit ... | head`).
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if let Err(e) = writeln!(std::io::stdout().lock(), $($arg)*) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            eprintln!("wikiknit: writing output: {e}");
            std::process::exit(2);
        }
    }};
}

/// Lint and knit wikilinks in a Markdown vault.
///
/// Output is for agents: one record per line, tab-separated, after a `# field...` header;
/// nothing when there are no records. Lists inside a field are `;`-separated. `--format json` prints one compact JSON object:
/// `{"schema_version":1,"command":...,"items":[...]}`.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to wikiknit.toml (default: search upward from the current directory).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Vault directory (uses its wikiknit.toml if present, else defaults).
    #[arg(long, global = true)]
    vault: Option<PathBuf>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Text)]
    format: Format,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Hybrid,
    Lexical,
    Semantic,
}

/// Notes ranked by meaning for `query`, and the mode actually used. Never downloads a model:
/// without one (or without the `semantic` feature) search is lexical, with a note on stderr.
#[cfg(feature = "semantic")]
fn meaning(
    vault: &Vault,
    query: &str,
    include_hubs: bool,
    mode: Mode,
) -> Result<(Option<Vec<search::Meaning>>, Mode)> {
    if mode == Mode::Lexical {
        return Ok((None, Mode::Lexical));
    }
    let sem = match wikiknit::semantic::Semantic::open(vault, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wikiknit: semantic search unavailable ({e}); using lexical");
            return Ok((None, Mode::Lexical));
        }
    };
    let Some(mut sem) = sem else {
        eprintln!(
            "wikiknit: semantic search is off until `wikiknit embed` downloads the model and indexes the vault; using lexical"
        );
        return Ok((None, Mode::Lexical));
    };
    // Keep the index current for a few changed or new notes (a few seconds at most); more
    // needs `embed`. Done before the coverage check when the missing notes could fit, so a
    // few new notes do not turn semantic search off.
    let update = refresh(&mut sem, vault)?;
    // A partial index would rank only the notes it happens to have: use it only when it
    // covers nearly the whole vault.
    let (indexed, total) = sem.coverage(vault);
    if !sem.covers(vault) {
        eprintln!(
            "wikiknit: the semantic index covers {indexed} of {total} notes; run `wikiknit embed`; using lexical"
        );
        return Ok((None, Mode::Lexical));
    }
    if update.stale > 0 {
        eprintln!(
            "wikiknit: {} note(s) changed since `wikiknit embed`; their old vectors are used",
            update.stale
        );
    }
    let ranked = sem.rank(vault, query, include_hubs)?;
    Ok((Some(ranked), mode))
}

/// The semantic index for `suggest`, refreshed for a few changed notes when the model is
/// available; `None` without an index.
/// Chunks `search` and `suggest` embed themselves before answering; more needs `embed`.
#[cfg(feature = "semantic")]
const REFRESH_CHUNKS: usize = 64;

/// Embed up to [`REFRESH_CHUNKS`] changed or new notes, unless the index is so far behind that
/// one refresh cannot make it cover the vault (then only `embed` helps, and every search
/// would pay for a partial refresh).
#[cfg(feature = "semantic")]
fn refresh(
    sem: &mut wikiknit::semantic::Semantic,
    vault: &Vault,
) -> Result<wikiknit::semantic::Update> {
    let (indexed, total) = sem.coverage(vault);
    if !sem.covers(vault) && total - indexed > REFRESH_CHUNKS {
        return Ok(sem.update(vault, Some(0))?);
    }
    Ok(sem.update(vault, Some(REFRESH_CHUNKS))?)
}

#[cfg(feature = "semantic")]
fn semantic_index(vault: &Vault) -> Result<Option<wikiknit::vectors::Index>> {
    if let Ok(Some(mut sem)) = wikiknit::semantic::Semantic::open(vault, false) {
        refresh(&mut sem, vault)?;
        if !sem.covers(vault) {
            let (indexed, total) = sem.coverage(vault);
            eprintln!(
                "wikiknit: the semantic index covers {indexed} of {total} notes; run `wikiknit embed` for similarity-based suggestions"
            );
        }
        return Ok(Some(sem.index));
    }
    index_file(vault)
}

#[cfg(not(feature = "semantic"))]
fn semantic_index(vault: &Vault) -> Result<Option<wikiknit::vectors::Index>> {
    index_file(vault)
}

/// The semantic index as saved (no model needed to read it).
fn index_file(vault: &Vault) -> Result<Option<wikiknit::vectors::Index>> {
    let store = Store::open(&vault.config.vault)?;
    let model = &vault.config.embedding_model;
    Ok(wikiknit::vectors::Index::load(
        &wikiknit::vectors::path(store.dir(), model),
        model,
    )?)
}

#[cfg(not(feature = "semantic"))]
fn meaning(
    _vault: &Vault,
    _query: &str,
    _include_hubs: bool,
    mode: Mode,
) -> Result<(Option<Vec<search::Meaning>>, Mode)> {
    if mode != Mode::Lexical {
        eprintln!("wikiknit: built without the `semantic` feature; using lexical search");
    }
    Ok((None, Mode::Lexical))
}

/// Note filters (each kind must match; repeat a flag to accept any of its values).
#[derive(Debug, Args, Default)]
struct FilterArgs {
    /// Frontmatter `type`.
    #[arg(long = "type", value_name = "TYPE")]
    types: Vec<String>,
    /// Note id prefix (folder), e.g. `Projects/`.
    #[arg(long = "path", value_name = "PREFIX")]
    paths: Vec<String>,
    /// Frontmatter field matches a regex anywhere in the value (case-insensitive):
    /// `stack=cloudflare`; anchor with `^...$` for an exact value.
    #[arg(long = "where", value_name = "KEY=REGEX")]
    wheres: Vec<String>,
    /// Tag, from frontmatter `tags` or inline `#tag` (nested tags match their parent).
    #[arg(long = "tag")]
    tags: Vec<String>,
    /// Only notes that link to this note (id, path, name, or alias).
    #[arg(long)]
    linked_to: Option<String>,
}

impl FilterArgs {
    fn is_set(&self) -> bool {
        !(self.types.is_empty()
            && self.paths.is_empty()
            && self.wheres.is_empty()
            && self.tags.is_empty()
            && self.linked_to.is_none())
    }

    fn build(&self, vault: &Vault) -> Result<Filter> {
        Ok(Filter::new(
            vault,
            &self.types,
            &self.paths,
            &self.wheres,
            &self.tags,
            self.linked_to.as_deref(),
        )?)
    }
}

#[derive(Debug, Args)]
struct Select {
    /// Only notes changed in the session that still need a link pass.
    #[arg(long)]
    changed: bool,
    /// Session id (default: $WIKIKNIT_SESSION, else the one active session).
    #[arg(long)]
    session: Option<String>,
    /// Notes to include (ids, vault paths, or file paths). Default: every note.
    paths: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report rule violations. Exits 1 if any error is found.
    Check(Select),
    /// List unlinked mentions: text naming another note (name or alias) without a link.
    Mentions(Select),
    /// Ranked full-text search: title, aliases, headings, frontmatter, body.
    Search {
        /// Words and "quoted phrases".
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Also search hub notes (excluded by default: they mention everything).
        #[arg(long)]
        include_hubs: bool,
        /// hybrid: words + meaning (needs `wikiknit embed` once; falls back to lexical);
        /// lexical: words only (BM25); semantic: meaning only.
        #[arg(long, value_enum, default_value_t = Mode::Hybrid)]
        mode: Mode,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Ranked link candidates for a note, each with reasons and the edit to apply.
    Suggest {
        /// The note: id, path, name, alias, or folder.
        #[arg(required_unless_present = "changed")]
        note: Option<String>,
        /// Every note changed in the session that still needs a link pass.
        #[arg(long, conflicts_with = "note")]
        changed: bool,
        #[arg(long)]
        session: Option<String>,
        /// Suggestions per note.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Leave out weaker candidates (a mention scores about 3; a weak text match under 0.5;
        /// one shared value or two shared links about 0.5-1).
        #[arg(long, default_value_t = 0.75)]
        min_score: f64,
    },
    /// A context pack sized to a token budget: the note (or the best search hits for a
    /// question), then the notes linked to and from it. Whole notes while they fit, an excerpt
    /// of a large seed, summaries for the rest; each entry says why it is included.
    Context {
        /// The note: id, path, name, or alias.
        #[arg(required_unless_present = "query")]
        note: Option<String>,
        /// A question or topic: its top search hits are the seeds. With a note: the
        /// note's excerpt focuses on the sections about it.
        #[arg(long)]
        query: Option<String>,
        /// Token budget (estimated: ~4 characters per token).
        #[arg(long, default_value_t = 4000, value_parser = clap::value_parser!(u64).range(200..))]
        budget: u64,
        /// Link hops from the seeds.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        depth: u64,
        /// Search hits used as seeds for --query.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
        seeds: u64,
        #[arg(long, value_enum, default_value_t = Mode::Hybrid)]
        mode: Mode,
        /// Filters for the --query seeds.
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Alias candidates: terms the vault uses for a note that do not name it yet, with the
    /// evidence and the frontmatter edit.
    Aliases {
        /// Only aliases for this note (id, path, name, or alias).
        note: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Notes that must use a term (as link text, or capitalized in prose) before it counts.
        #[arg(long, default_value_t = 2)]
        min_notes: usize,
    },
    /// Build or update the semantic index (downloads the embedding model on first use).
    Embed {
        /// Re-embed every note, not only changed ones.
        #[arg(long)]
        rebuild: bool,
    },
    /// Notes linked to or from a note, with the line each link is written on.
    Links {
        /// The note: id, path, name, or alias.
        note: String,
        /// Hops to follow (hubs are never expanded through).
        #[arg(long, default_value_t = 1)]
        depth: usize,
        /// Only links from the note.
        #[arg(long = "out", conflicts_with = "inbound")]
        outbound: bool,
        /// Only links to the note.
        #[arg(long = "in")]
        inbound: bool,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// One line per note: id, type, tags, aliases, title, summary.
    Catalog {
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Link graph summary: isolated notes and components, hub notes excluded.
    Graph,
    /// List notes changed in the session that still need a link pass.
    Changed {
        #[arg(long)]
        session: Option<String>,
        /// Every note in the vault that needs a link pass, from any session or editor.
        #[arg(long)]
        all: bool,
    },
    /// Record a link pass: the notes' current content counts as linked, for every session.
    Mark {
        #[arg(long)]
        session: Option<String>,
        /// Mark every pending note in the vault, not only this session's.
        #[arg(long)]
        all: bool,
        /// Notes to mark. Default: the session's changed notes.
        paths: Vec<String>,
    },
    /// Start a session record (hooks do this); --reset restarts it from the vault as it is now.
    Baseline {
        #[arg(long)]
        session: String,
        #[arg(long)]
        reset: bool,
    },
    /// Run as a Claude Code hook (JSON on stdin).
    Hook {
        #[arg(value_enum)]
        event: hook::Event,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Hook { event } = cli.command {
        return run_hook(event);
    }
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wikiknit: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Hook errors exit 1 (non-blocking in Claude Code); a hold exits 2. `pre-commit` holds a
/// commit it cannot check (an error, bad input that mentions a commit, running out of time):
/// exit 1 would let the commit through.
fn run_hook(event: hook::Event) -> ExitCode {
    let pre_commit = event == hook::Event::PreCommit;
    let mut stdin = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin) {
        eprintln!("wikiknit hook: reading stdin: {e}");
        return ExitCode::from(1);
    }
    let input: hook::Input = match serde_json::from_str(&stdin) {
        Ok(i) => i,
        Err(e) => {
            if pre_commit && stdin.contains("commit") {
                eprintln!(
                    "Commit held by wikiknit: it could not read the hook input ({e}), so it \
                     cannot check this commit."
                );
                return ExitCode::from(2);
            }
            eprintln!("wikiknit hook: bad input JSON: {e}");
            return ExitCode::from(1);
        }
    };
    let result = if pre_commit {
        pre_commit_within_deadline(input)
    } else {
        hook::run(event, &input)
    };
    match result {
        Ok(Outcome::Allow) => ExitCode::SUCCESS,
        Ok(Outcome::Block(msg)) => {
            eprintln!("{msg}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("wikiknit hook: {e}");
            ExitCode::from(1)
        }
    }
}

/// Default time the pre-commit check may take; below the plugin's 10 s hook timeout, after
/// which Claude Code would go ahead with the commit.
const HOOK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// Run the pre-commit check, holding a commit that cannot be checked in time
/// (`$WIKIKNIT_HOOK_TIMEOUT_MS` overrides the deadline).
fn pre_commit_within_deadline(input: hook::Input) -> Result<Outcome, wikiknit::Error> {
    let deadline = std::env::var("WIKIKNIT_HOOK_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(HOOK_DEADLINE, std::time::Duration::from_millis);
    // From the command text alone, so it is known even if the check hangs.
    let may_commit = hook::may_commit(&input);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(hook::run(hook::Event::PreCommit, &input));
    });
    match rx.recv_timeout(deadline) {
        Ok(result) => result,
        Err(_) if may_commit => Ok(Outcome::Block(format!(
            "Commit held by wikiknit: checking the vault ran out of time ({} ms), so this \
             commit was not checked. Retry; if it keeps timing out, run `wikiknit changed` to \
             see why.",
            deadline.as_millis()
        ))),
        Err(_) => Err(wikiknit::Error::Usage(format!(
            "pre-commit check timed out after {} ms",
            deadline.as_millis()
        ))),
    }
}

fn load_config(cli: &Cli) -> Result<Config> {
    if let Some(path) = &cli.config {
        return Ok(Config::load(path)?);
    }
    if let Some(vault) = &cli.vault {
        let root = vault
            .canonicalize()
            .with_context(|| format!("vault {}", vault.display()))?;
        // A wikiknit.toml in the vault, or above it naming this vault.
        for dir in root.ancestors() {
            let file = dir.join(CONFIG_FILE);
            if !file.is_file() {
                continue;
            }
            match Config::load(&file) {
                Ok(cfg) if cfg.vault == root => return Ok(cfg),
                Ok(_) => {}
                // The vault's own config is broken: its rules must not silently fall away.
                Err(e) if dir == root => return Err(e.into()),
                Err(e) => eprintln!("wikiknit: ignoring {}: {e}", file.display()),
            }
        }
        return Ok(Config::for_vault(root));
    }
    let cwd = std::env::current_dir()?;
    match Config::find(&cwd) {
        Some(file) => Ok(Config::load(&file)?),
        None => bail!(
            "no {CONFIG_FILE} in {} or its parents; pass --vault or --config",
            cwd.display()
        ),
    }
}

/// The session's id and its changed notes (pending and changed while it ran).
fn session_changed(vault: &Vault, flag: Option<&str>) -> Result<(Store, String, BTreeSet<String>)> {
    let store = Store::open(&vault.config.vault)?;
    let guessed =
        flag.is_none() && std::env::var_os(state::SESSION_ENV).is_none_or(|v| v.is_empty());
    let session = store.resolve_session(flag)?;
    if guessed {
        eprintln!("wikiknit: session {session} (the only active one; pass --session to choose)");
    }
    let now = state::current_hashes(vault);
    // A session the hooks never saw start (plugin added mid-session): start it the way the
    // hooks do, with pending notes counted as its own.
    let (s, created) = store.open_session(&session, &now, true)?;
    let changed = store.changed(&s, &now)?;
    if created {
        // An empty list here must not read as "nothing changed": the session was unknown.
        eprintln!(
            "wikiknit: session {session} had no record (hooks not run, or a mistyped id?); \
             started it now, counting the {} note(s) pending from any session as its changes",
            changed.len()
        );
    }
    Ok((store, session, changed))
}

fn selection(vault: &Vault, sel: &Select) -> Result<Option<BTreeSet<String>>> {
    if sel.changed {
        return Ok(Some(session_changed(vault, sel.session.as_deref())?.2));
    }
    if sel.paths.is_empty() {
        return Ok(None);
    }
    Ok(Some(ids_for(vault, &sel.paths)?))
}

fn ids_for(vault: &Vault, paths: &[String]) -> Result<BTreeSet<String>> {
    let cwd = std::env::current_dir()?;
    paths
        .iter()
        .map(|p| {
            if vault.id_for_arg(p, &cwd).is_none() && vault.is_ignored_arg(p, &cwd) {
                bail!("`{p}` is in a folder the config ignores");
            }
            // The same note references as `links` and `suggest`: id, path, name, alias.
            Ok(vault.notes[vault.find_note(p)?].id.clone())
        })
        .collect()
}

/// One output field: tabs and newlines become spaces.
fn field(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Print records: tab-separated lines after a `# header`, or the JSON envelope.
fn emit<T: Serialize>(
    format: Format,
    command: &str,
    header: &[&str],
    items: &[T],
    row: impl Fn(&T) -> Vec<String>,
) {
    emit_with(format, command, header, items, row, serde_json::Map::new());
}

fn emit_with<T: Serialize>(
    format: Format,
    command: &str,
    header: &[&str],
    items: &[T],
    row: impl Fn(&T) -> Vec<String>,
    extra: serde_json::Map<String, serde_json::Value>,
) {
    match format {
        Format::Json => {
            let mut obj = serde_json::Map::new();
            obj.insert("schema_version".into(), SCHEMA_VERSION.into());
            obj.insert("command".into(), command.into());
            obj.extend(extra);
            obj.insert(
                "items".into(),
                serde_json::to_value(items).expect("serializable"),
            );
            out!("{}", serde_json::Value::Object(obj));
        }
        Format::Text => {
            if items.is_empty() {
                return;
            }
            out!("# {}", header.join("\t"));
            for item in items {
                let fields: Vec<String> = row(item).iter().map(|f| field(f)).collect();
                out!("{}", fields.join("\t"));
            }
        }
    }
}

#[cfg(feature = "semantic")]
fn embed(vault: &Vault, fmt: Format, rebuild: bool) -> Result<ExitCode> {
    let name = &vault.config.embedding_model;
    if !wikiknit::embedder::is_cached(name)? {
        eprintln!("wikiknit: downloading embedding model {name} (first use)");
    }
    let mut sem =
        wikiknit::semantic::Semantic::open(vault, true)?.context("embedding model did not load")?;
    if rebuild {
        sem.index.notes.clear();
        sem.rebuild = true;
    }
    let started = std::time::Instant::now();
    let update = sem.update(vault, None)?;
    let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
    let mut extra = serde_json::Map::new();
    extra.insert("seconds".into(), seconds.into());
    emit_with(
        fmt,
        "embed",
        &[
            "model",
            "embedded",
            "chunks",
            "removed",
            "stale",
            "total_notes",
            "total_chunks",
            "seconds",
        ],
        &[update],
        |u| {
            vec![
                u.model.clone(),
                u.embedded.to_string(),
                u.chunks.to_string(),
                u.removed.to_string(),
                u.stale.to_string(),
                u.total_notes.to_string(),
                u.total_chunks.to_string(),
                seconds.to_string(),
            ]
        },
        extra,
    );
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(feature = "semantic"))]
fn embed(_vault: &Vault, _fmt: Format, _rebuild: bool) -> Result<ExitCode> {
    bail!("this wikiknit was built without the `semantic` feature")
}

fn opt(s: Option<&String>) -> String {
    s.cloned().unwrap_or_default()
}

fn run(cli: Cli) -> Result<ExitCode> {
    let config = load_config(&cli)?;
    let vault = Vault::load(config)?;
    let fmt = cli.format;
    match &cli.command {
        Command::Check(sel) => {
            let selected = selection(&vault, sel)?;
            let findings = rules::check(&vault, selected.as_ref());
            let errors = findings.iter().filter(|f| f.is_error()).count();
            emit(
                fmt,
                "check",
                &["note", "line", "severity", "rule", "message", "fix"],
                &findings,
                |f| {
                    vec![
                        f.note.clone(),
                        f.line.to_string(),
                        f.severity.to_string(),
                        f.rule.to_string(),
                        f.message.clone(),
                        f.fix.clone(),
                    ]
                },
            );
            Ok(if errors > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Mentions(sel) => {
            let selected = selection(&vault, sel)?;
            let found = mentions::find(&vault, selected.as_ref());
            emit(
                fmt,
                "mentions",
                &["note", "line", "term", "target", "count", "context"],
                &found,
                |m| {
                    vec![
                        m.note.clone(),
                        m.line.to_string(),
                        m.term.clone(),
                        m.target.clone(),
                        m.count.to_string(),
                        m.context.clone(),
                    ]
                },
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Search {
            query,
            limit,
            include_hubs,
            mode,
            filter,
        } => {
            let filter = filter.build(&vault)?;
            let query = query.join(" ");
            let (ranked, used) = meaning(&vault, &query, *include_hubs, *mode)?;
            let hits = search::search(
                &vault,
                &query,
                &filter,
                &search::Options {
                    include_hubs: *include_hubs,
                    limit: *limit,
                    semantic: ranked.as_deref(),
                    lexical: used != Mode::Semantic,
                },
            );
            let mut extra = serde_json::Map::new();
            extra.insert("mode".into(), format!("{used:?}").to_lowercase().into());
            emit_with(
                fmt,
                "search",
                &[
                    "id",
                    "score",
                    "type",
                    "similarity",
                    "matched",
                    "coverage",
                    "line",
                    "snippet",
                ],
                &hits,
                |h| {
                    vec![
                        h.id.clone(),
                        format!("{:.2}", h.score),
                        opt(h.kind.as_ref()),
                        h.similarity.map(|s| format!("{s:.3}")).unwrap_or_default(),
                        h.matched.join(";"),
                        format!("{:.2}", h.coverage),
                        h.line.to_string(),
                        h.snippet.clone(),
                    ]
                },
                extra,
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Embed { rebuild } => embed(&vault, fmt, *rebuild),
        Command::Context {
            note,
            query,
            budget,
            depth,
            seeds,
            mode,
            filter,
        } => {
            let mut extra = serde_json::Map::new();
            if query.as_deref().is_some_and(|q| q.trim().is_empty()) {
                bail!("--query is empty");
            }
            let focus_for = |i: usize, q: Option<&str>, extra_line: Option<usize>| {
                let mut f = q.map_or_else(Vec::new, |q| {
                    search::matching_lines(&vault, &vault.notes[i], q)
                });
                if let Some(l) = extra_line.filter(|&l| l > 0)
                    && !f.iter().any(|&(x, _)| x == l)
                {
                    f.push((l, 1.0));
                }
                f
            };
            let mut weak = false;
            let seeds: Vec<wikiknit::context::Seed> = if let Some(start) = note {
                if filter.is_set() {
                    bail!(
                        "filters choose --query seeds; with a note, use `wikiknit links` to filter its neighbors"
                    );
                }
                let start = vault.find_note(start)?;
                vec![wikiknit::context::Seed {
                    note: start,
                    score: 1.0,
                    why: match query {
                        Some(q) => format!("the note asked for, focused on \"{q}\""),
                        None => "the note asked for".into(),
                    },
                    focus: focus_for(start, query.as_deref(), None),
                    snippet: None,
                }]
            } else {
                let q = query.as_deref().expect("required by clap");
                let filter = filter.build(&vault)?;
                let (ranked, used) = meaning(&vault, q, false, *mode)?;
                extra.insert("mode".into(), format!("{used:?}").to_lowercase().into());
                let hits = search::search(
                    &vault,
                    q,
                    &filter,
                    &search::Options {
                        include_hubs: false,
                        // A wider pool: weak hits are dropped below, stronger ones further
                        // down take their place.
                        limit: usize::try_from(*seeds)
                            .unwrap_or(usize::MAX)
                            .saturating_mul(3),
                        semantic: ranked.as_deref(),
                        lexical: used != Mode::Semantic,
                    },
                );
                let by_meaning = |h: &search::Hit| h.matched.contains(&"semantic");
                let strong = |h: &search::Hit| by_meaning(h) || h.coverage >= SEED_COVERAGE;
                // Seeds are the hits that match by meaning or cover most of the question,
                // close to the best of them. When none does, the best hit alone, and the
                // pack says it is weak: the vault may not cover the question.
                weak = !hits.iter().any(strong);
                let top = hits
                    .iter()
                    .find(|h| strong(h))
                    .or(hits.first())
                    .map_or(1.0, |h| h.score.max(1e-9));
                let max_seeds = usize::try_from(*seeds).unwrap_or(usize::MAX);
                hits.iter()
                    .enumerate()
                    .filter(|(k, h)| {
                        if weak {
                            *k == 0
                        } else {
                            strong(h) && h.score / top >= SEED_SCORE
                        }
                    })
                    .take(max_seeds)
                    .filter_map(|(_, h)| {
                        let note = vault.index_of(&h.id)?;
                        let sim = h
                            .similarity
                            .map(|s| format!("; similarity {s:.2}"))
                            .unwrap_or_default();
                        let at = if h.line == 0 {
                            String::new()
                        } else {
                            format!(" at line {}", h.line)
                        };
                        Some(wikiknit::context::Seed {
                            note,
                            score: h.score / top,
                            why: format!(
                                "search hit: matched {}{at}; covers {:.0}% of the query{sim}",
                                h.matched.join(", "),
                                h.coverage * 100.0
                            ),
                            focus: focus_for(note, Some(q), Some(h.line)),
                            snippet: (!h.snippet.is_empty()).then(|| h.snippet.clone()),
                        })
                    })
                    .collect()
            };
            if seeds.is_empty() {
                eprintln!("wikiknit: no note matches the query");
            }
            if weak {
                eprintln!(
                    "wikiknit: the best match covers little of the query; the vault may not cover it"
                );
            }
            let size = |v: u64| usize::try_from(v).unwrap_or(usize::MAX);
            let pack = wikiknit::context::build(&vault, &seeds, size(*depth), size(*budget), weak);
            match fmt {
                Format::Json => {
                    extra.insert("budget".into(), pack.budget.into());
                    extra.insert("used".into(), pack.used.into());
                    extra.insert("weak".into(), pack.weak.into());
                    extra.insert("omitted".into(), pack.omitted.clone().into());
                    extra.insert("omitted_more".into(), pack.omitted_more.into());
                    emit_with(fmt, "context", &[], &pack.entries, |_| Vec::new(), extra);
                }
                Format::Text => out!("{}", wikiknit::context::render(&pack).trim_end()),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Aliases {
            note,
            limit,
            min_notes,
        } => {
            let only = note.as_deref().map(|n| vault.find_note(n)).transpose()?;
            if let Some(i) = only
                && vault.config.is_hub(&vault.notes[i].id)
            {
                eprintln!(
                    "wikiknit: {} is a hub; hubs get no aliases",
                    vault.notes[i].id
                );
            }
            let found = wikiknit::aliases::suggest(&vault, only, *limit, (*min_notes).max(1));
            let joined =
                |a: &wikiknit::aliases::AliasSuggestion,
                 f: &dyn Fn(&wikiknit::aliases::Term) -> String,
                 sep: &str| { a.terms.iter().map(f).collect::<Vec<_>>().join(sep) };
            emit(
                fmt,
                "aliases",
                &[
                    "target", "score", "terms", "notes", "reasons", "line", "edit",
                ],
                &found,
                |a| {
                    vec![
                        a.target.clone(),
                        format!("{:.2}", a.score),
                        joined(a, &|x| x.term.clone(), ";"),
                        joined(a, &|x| x.notes.to_string(), ";"),
                        joined(
                            a,
                            &|x| format!("{}: {}", x.term, x.reasons.join(", ")),
                            " | ",
                        ),
                        a.line.to_string(),
                        a.edit.clone(),
                    ]
                },
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Suggest {
            note,
            changed,
            session,
            limit,
            min_score,
        } => {
            let starts: Vec<usize> = if *changed {
                session_changed(&vault, session.as_deref())?
                    .2
                    .iter()
                    .filter_map(|id| vault.index_of(id))
                    .collect()
            } else {
                vec![vault.find_note(note.as_deref().expect("required by clap"))?]
            };
            let index = semantic_index(&vault)?;
            // Vault-wide structures once, then every note (in parallel, order kept).
            let all = wikiknit::suggest::Suggester::new(&vault, index.as_ref())
                .suggest_many(&starts, *limit, *min_score);
            emit(
                fmt,
                "suggest",
                &[
                    "note", "action", "target", "score", "reasons", "line", "edit",
                ],
                &all,
                |s| {
                    vec![
                        s.note.clone(),
                        s.action.to_string(),
                        s.target.clone(),
                        format!("{:.2}", s.score),
                        s.reasons.join("; "),
                        s.line.to_string(),
                        s.edit.clone(),
                    ]
                },
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Links {
            note,
            depth,
            outbound,
            inbound,
            filter,
        } => {
            let start = vault.find_note(note)?;
            let filter = filter.build(&vault)?;
            let dir = match (outbound, inbound) {
                (true, _) => Direction::Out,
                (_, true) => Direction::In,
                _ => Direction::Both,
            };
            let edges = links::neighbors(&vault, start, *depth, dir, &filter);
            let mut extra = serde_json::Map::new();
            extra.insert("note".into(), vault.notes[start].id.clone().into());
            emit_with(
                fmt,
                "links",
                &[
                    "dir",
                    "depth",
                    "id",
                    "hub",
                    "type",
                    "via",
                    "source",
                    "line",
                    "context",
                    "back_line",
                    "back_context",
                    "count",
                    "summary",
                ],
                &edges,
                |e| {
                    vec![
                        e.dir.to_string(),
                        e.depth.to_string(),
                        e.id.clone(),
                        e.hub.to_string(),
                        opt(e.kind.as_ref()),
                        e.via.clone(),
                        e.source.clone(),
                        e.line.to_string(),
                        e.context.clone(),
                        e.back_line.map(|l| l.to_string()).unwrap_or_default(),
                        opt(e.back_context.as_ref()),
                        e.count.to_string(),
                        e.summary.clone(),
                    ]
                },
                extra,
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Catalog { filter } => {
            let filter = filter.build(&vault)?;
            let entries: Vec<catalog::Entry> = catalog::build(&vault)
                .into_iter()
                .filter(|e| {
                    filter.is_empty() || vault.get(&e.id).is_some_and(|n| filter.matches(&vault, n))
                })
                .collect();
            emit(
                fmt,
                "catalog",
                &["id", "hub", "type", "tags", "aliases", "title", "summary"],
                &entries,
                |e| {
                    vec![
                        e.id.clone(),
                        e.hub.to_string(),
                        opt(e.kind.as_ref()),
                        e.tags.join(";"),
                        e.aliases.join(";"),
                        opt(e.title.as_ref()),
                        e.summary.clone(),
                    ]
                },
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Graph => {
            let r = graph::report(&vault);
            let mut rows: Vec<serde_json::Value> = r
                .isolated
                .iter()
                .map(|id| serde_json::json!({"kind": "isolated", "size": 1, "ids": [id]}))
                .collect();
            rows.extend(
                r.components
                    .iter()
                    .map(|c| serde_json::json!({"kind": "component", "size": c.len(), "ids": c})),
            );
            let mut extra = serde_json::Map::new();
            extra.insert("notes".into(), r.notes.into());
            extra.insert("links".into(), r.links.into());
            extra.insert("hubs".into(), r.hubs.clone().into());
            if fmt == Format::Text {
                out!(
                    "# notes={} links={} hubs={}",
                    r.notes,
                    r.links,
                    r.hubs.join(",")
                );
            }
            emit_with(
                fmt,
                "graph",
                &["kind", "size", "ids"],
                &rows,
                |v| {
                    vec![
                        v["kind"].as_str().unwrap_or_default().to_string(),
                        v["size"].to_string(),
                        v["ids"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(";")
                            })
                            .unwrap_or_default(),
                    ]
                },
                extra,
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Changed { session, all } => {
            let changed: Vec<String> = if *all {
                Store::open(&vault.config.vault)?
                    .pending(&state::current_hashes(&vault))?
                    .into_iter()
                    .collect()
            } else {
                session_changed(&vault, session.as_deref())?
                    .2
                    .into_iter()
                    .collect()
            };
            emit(fmt, "changed", &["id"], &changed, |id| vec![id.clone()]);
            Ok(ExitCode::SUCCESS)
        }
        Command::Mark {
            session,
            all,
            paths,
        } => {
            let now = state::current_hashes(&vault);
            let (store, ids) = if *all {
                let store = Store::open(&vault.config.vault)?;
                let pending = store.pending(&now)?;
                (store, pending)
            } else if !paths.is_empty() {
                (Store::open(&vault.config.vault)?, ids_for(&vault, paths)?)
            } else {
                let (store, _, changed) = session_changed(&vault, session.as_deref())?;
                (store, changed)
            };
            let marked: Vec<String> = store.mark(&ids, &now)?.into_iter().collect();
            emit(fmt, "mark", &["marked"], &marked, |id| vec![id.clone()]);
            Ok(ExitCode::SUCCESS)
        }
        Command::Baseline { session, reset } => {
            let store = Store::open(&vault.config.vault)?;
            if *reset {
                let _ = std::fs::remove_file(store.session_path(session));
            }
            store.start_session(session, &state::current_hashes(&vault), false)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { .. } => unreachable!("handled in main"),
    }
}
