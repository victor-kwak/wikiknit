use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use wikiknit::config::{CONFIG_FILE, Config};
use wikiknit::hook::{self, Outcome};
use wikiknit::state::{self, Baseline};
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
    /// Only notes changed since the session baseline and not yet marked.
    #[arg(long)]
    changed: bool,
    /// Session id (default: $WIKIKNIT_SESSION, else the newest baseline).
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
    /// List notes changed since the session baseline and not yet marked.
    Changed {
        #[arg(long)]
        session: Option<String>,
    },
    /// Record changed notes as linked (after a link pass).
    Mark {
        #[arg(long)]
        session: Option<String>,
        /// Notes to mark. Default: every changed note.
        paths: Vec<String>,
    },
    /// Create (or with --reset, replace) the session baseline.
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
        let file = vault.join(CONFIG_FILE);
        if file.is_file() {
            return Ok(Config::load(&file)?);
        }
        let root = vault
            .canonicalize()
            .with_context(|| format!("vault {}", vault.display()))?;
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

fn selection(vault: &Vault, sel: &Select) -> Result<Option<BTreeSet<String>>> {
    if sel.changed {
        let session = state::resolve_session(&vault.config.vault, sel.session.as_deref())?;
        let base = Baseline::load(&vault.config.vault, &session)?
            .with_context(|| format!("no baseline for session `{session}`"))?;
        return Ok(Some(base.changed(vault)));
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
            vault
                .id_for_arg(p, &cwd)
                .with_context(|| format!("`{p}` is not a note in the vault"))
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
                    "{errors} error(s), {} warning(s) in {} of {checked} note(s) checked",
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
                    out!("{}{kind}{hub}{aliases}: {}", e.id, e.summary);
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
        Command::Changed { session } => {
            let session = state::resolve_session(&vault.config.vault, session.as_deref())?;
            let base = Baseline::load(&vault.config.vault, &session)?
                .with_context(|| format!("no baseline for session `{session}`"))?;
            let changed = base.changed(&vault);
            if json {
                print_json(&changed);
            } else {
                changed.iter().for_each(|id| out!("{id}"));
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Mark { session, paths } => {
            let session = state::resolve_session(&vault.config.vault, session.as_deref())?;
            let mut base = Baseline::load(&vault.config.vault, &session)?
                .with_context(|| format!("no baseline for session `{session}`"))?;
            let only = if paths.is_empty() {
                None
            } else {
                Some(ids_for(&vault, paths)?)
            };
            let marked = base.mark(&vault, only.as_ref());
            base.save(&session)?;
            if json {
                print_json(&marked);
            } else {
                eprintln!("marked {} note(s) as linked", marked.len());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Baseline { session, reset } => {
            let root = vault.config.vault.clone();
            if !reset && Baseline::load(&root, session)?.is_some() {
                eprintln!("baseline for `{session}` already exists (use --reset to replace)");
                return Ok(ExitCode::SUCCESS);
            }
            Baseline::snapshot(&vault).save(session)?;
            eprintln!(
                "baseline recorded for `{session}`: {}",
                Baseline::path(&root, session)?.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Hook { .. } => unreachable!("handled in main"),
    }
}
