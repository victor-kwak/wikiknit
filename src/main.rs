use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use wikiknit::config::{CONFIG_FILE, Config};
use wikiknit::hook::{self, Outcome};
use wikiknit::state::{self, Store};
use wikiknit::{Vault, catalog, graph, mentions, rules};

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
    /// One line per note: id, type, aliases, summary.
    Catalog,
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

fn print_json<T: Serialize>(value: &T) {
    out!(
        "{}",
        serde_json::to_string_pretty(value).expect("serializable")
    );
}

fn run(cli: Cli) -> Result<ExitCode> {
    let config = load_config(&cli)?;
    let vault = Vault::load(config)?;
    let json = cli.format == Format::Json;
    match &cli.command {
        Command::Check(sel) => {
            let selected = selection(&vault, sel)?;
            let findings = rules::check(&vault, selected.as_ref());
            let errors = findings.iter().filter(|f| f.is_error()).count();
            if json {
                print_json(&findings);
            } else {
                for f in &findings {
                    out!(
                        "{}.md:{}: {} [{}] {}",
                        f.note,
                        f.line,
                        f.severity,
                        f.rule,
                        f.message
                    );
                }
                let notes: BTreeSet<_> = findings.iter().map(|f| &f.note).collect();
                let checked = selected.as_ref().map_or(vault.notes.len(), BTreeSet::len);
                eprintln!(
                    "{checked} note(s) checked: {errors} error(s), {} warning(s), in {} note(s)",
                    findings.len() - errors,
                    notes.len()
                );
            }
            Ok(if errors > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Mentions(sel) => {
            let selected = selection(&vault, sel)?;
            let found = mentions::find(&vault, selected.as_ref());
            if json {
                print_json(&found);
            } else {
                for m in &found {
                    let times = if m.count > 1 {
                        format!(" ({}x)", m.count)
                    } else {
                        String::new()
                    };
                    out!(
                        "{}.md:{}: \"{}\" -> [[{}]]{times}  | {}",
                        m.note,
                        m.line,
                        m.term,
                        m.target,
                        m.context
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Catalog => {
            let entries = catalog::build(&vault);
            if json {
                print_json(&entries);
            } else {
                for e in &entries {
                    let kind = e
                        .kind
                        .as_deref()
                        .map(|k| format!(" [{k}]"))
                        .unwrap_or_default();
                    let hub = if e.hub { " (hub)" } else { "" };
                    let aliases = if e.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" {{{}}}", e.aliases.join(", "))
                    };
                    let title = e
                        .title
                        .as_deref()
                        .map(|t| format!("{t} — "))
                        .unwrap_or_default();
                    out!("{}{kind}{hub}{aliases}: {title}{}", e.id, e.summary);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Graph => {
            let r = graph::report(&vault);
            if json {
                print_json(&r);
            } else {
                out!(
                    "{} notes, {} links; hubs excluded: {}",
                    r.notes,
                    r.links,
                    r.hubs.join(", ")
                );
                out!("isolated ({}):", r.isolated.len());
                for id in &r.isolated {
                    out!("  {id}");
                }
                out!("components ({}):", r.components.len());
                for c in &r.components {
                    if c.len() <= 12 {
                        out!("  [{}] {}", c.len(), c.join(", "));
                    } else {
                        out!("  [{}] {}, ...", c.len(), c[..6].join(", "));
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Changed { session, all } => {
            let changed = if *all {
                Store::open(&vault.config.vault)?.pending(&state::current_hashes(&vault))?
            } else {
                session_changed(&vault, session.as_deref())?.2
            };
            if json {
                print_json(&changed);
            } else {
                changed.iter().for_each(|id| out!("{id}"));
            }
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
            let marked = store.mark(&ids, &now)?;
            if json {
                print_json(&marked);
            } else {
                eprintln!("marked {} note(s) as linked", marked.len());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Baseline { session, reset } => {
            let store = Store::open(&vault.config.vault)?;
            if *reset {
                let _ = std::fs::remove_file(store.session_path(session));
            } else if store.load_session(session)?.is_some() {
                eprintln!("session `{session}` already started (use --reset to restart it)");
                return Ok(ExitCode::SUCCESS);
            }
            store.start_session(session, &state::current_hashes(&vault), false)?;
            eprintln!(
                "session `{session}` started: {}",
                store.session_path(session).display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { .. } => unreachable!("handled in main"),
    }
}
