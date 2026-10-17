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
        #[command(flatten)]
        filter: FilterArgs,
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

/// Hook errors exit 1 (non-blocking in Claude Code); only a real hold exits 2.
fn run_hook(event: hook::Event) -> ExitCode {
    let mut stdin = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut stdin) {
        eprintln!("wikiknit hook: reading stdin: {e}");
        return ExitCode::from(1);
    }
    let input: hook::Input = match serde_json::from_str(&stdin) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("wikiknit hook: bad input JSON: {e}");
            return ExitCode::from(1);
        }
    };
    match hook::run(event, &input) {
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
            if file.is_file()
                && let Ok(cfg) = Config::load(&file)
                && cfg.vault == root
            {
                return Ok(cfg);
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
    let s = store.start_session(&session, &now, true)?;
    let changed = store.changed(&s, &now)?;
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
            vault.id_for_arg(p, &cwd).with_context(|| {
                if vault.is_ignored_arg(p, &cwd) {
                    format!("`{p}` is in a folder the config ignores")
                } else {
                    format!("`{p}` is not a note in the vault")
                }
            })
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
                &["note", "line", "severity", "rule", "message"],
                &findings,
                |f| {
                    vec![
                        f.note.clone(),
                        f.line.to_string(),
                        f.severity.to_string(),
                        f.rule.to_string(),
                        f.message.clone(),
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
            filter,
        } => {
            let filter = filter.build(&vault)?;
            let hits = search::search(&vault, &query.join(" "), &filter, *include_hubs, *limit);
            emit(
                fmt,
                "search",
                &["id", "score", "type", "matched", "line", "snippet"],
                &hits,
                |h| {
                    vec![
                        h.id.clone(),
                        format!("{:.2}", h.score),
                        opt(h.kind.as_ref()),
                        h.matched.join(";"),
                        h.line.to_string(),
                        h.snippet.clone(),
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
