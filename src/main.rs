use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use wikiknit::config::{CONFIG_FILE, Config};
use wikiknit::filter::Filter;
use wikiknit::git::{Repo, Source};
use wikiknit::links::{self, Direction};
use wikiknit::{Vault, catalog, graph, mentions, rules};

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

/// Lint wikilinks in a Markdown vault.
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
    /// Only what the uncommitted changes add: findings in the vault as it is on disk that the
    /// last commit does not have (needs git).
    #[arg(long, conflicts_with_all = ["staged", "paths"])]
    changed: bool,
    /// Only what the next commit adds: findings in the staged vault that the last commit does
    /// not have. For a git pre-commit hook.
    #[arg(long, conflicts_with = "paths")]
    staged: bool,
    /// Notes to include (ids, vault paths, or file paths). Default: every note.
    paths: Vec<String>,
}

impl Select {
    /// The vault version to compare with the last commit, if any.
    fn since_head(&self) -> Option<Change> {
        if self.staged {
            Some(Change::Staged)
        } else if self.changed {
            Some(Change::WorkTree)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// The vault on disk.
    WorkTree,
    /// The index.
    Staged,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report rule violations. Exits 1 if any error is found.
    Check(Select),
    /// List unlinked mentions: text naming another note (name or alias) without a link.
    Mentions(Select),
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
    /// Set a vault up for checking: find the vault and its git repository, write a starter
    /// wikiknit.toml with what the vault shows (link style, index notes, templates folder), and
    /// install the pre-commit and pre-merge-commit hooks that run `wikiknit check --staged`.
    /// Never overwrites: an existing config is kept, and a hook wikiknit did not write is left
    /// alone (it prints the line to add). A second run changes nothing. Exit 1 when a step needs
    /// you. `--vault` names the vault; `--config` where the config is or goes.
    Init {
        /// Print what would change; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Print the agent instructions for this version, between `<!-- wikiknit:start -->` and
    /// `<!-- wikiknit:end -->`, for the project's instruction file (AGENTS.md, CLAUDE.md, ...).
    Snippet {
        /// Write the block into this file instead: replace the block between the markers, or
        /// add it at the end (the file is created if missing).
        #[arg(long, value_name = "FILE")]
        into: Option<PathBuf>,
    },
    /// Removed in 0.7 (the Claude Code hooks). Kept as a no-op that exits 0, so a session
    /// whose plugin still has the old hooks is not blocked by an unknown subcommand.
    #[command(hide = true)]
    Hook {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        _args: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Hook { .. }) {
        return ExitCode::SUCCESS;
    }
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wikiknit: {e:#}");
            ExitCode::from(2)
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

/// The vault as `change` has it and as the last commit has it, each with the `wikiknit.toml`
/// of that version when the repository has one (its rules, its vault folder), and the notes
/// renamed in between (old id -> new id). Copies live in `tmp`. `None`: the repository being
/// committed (a hook's) is not the vault's, so the commit changes nothing in the vault.
struct Versions {
    after: Vault,
    before: Vault,
    renamed: BTreeMap<String, String>,
}

/// `a/./b/../c` -> `a/c`, as a path from the top with `/`; `None` if it climbs out.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn versions(disk: &Config, change: Change, tmp: &tempfile::TempDir) -> Result<Option<Versions>> {
    let mut repo = Repo::of(&disk.vault, change == Change::Staged)?;
    if !repo.in_commit {
        eprintln!(
            "wikiknit: the repository being committed ({}) is not the vault's ({}); nothing to check",
            repo.committing.as_deref().unwrap_or(&repo.top).display(),
            repo.top.display()
        );
        return Ok(None);
    }
    let source = match change {
        Change::WorkTree => Source::WorkTree,
        Change::Staged => Source::Staged,
    };
    if source == Source::Staged {
        repo.scratch_objects(&tmp.path().join("objects"))?;
    }
    // The config file as a path from the repository top, when it is inside it.
    let config_rel: Option<String> = disk
        .source
        .as_ref()
        .and_then(|s| s.canonicalize().ok())
        .and_then(|s| {
            s.strip_prefix(&repo.top)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
        });
    let disk_prefix = repo.prefix.trim_end_matches('/').to_string();
    // One version: its config (if the repository has it there), its vault, and the vault
    // folder as a path from the top (with a trailing `/`, or empty for the top).
    let copy = |source: Source, name: &str| -> Result<(Vault, String)> {
        let root = tmp.path().join(name);
        let text = match &config_rel {
            Some(r) => repo.read(source, r)?,
            None => None,
        };
        // That version's vault folder, as written in its config.
        let written = text.as_ref().and_then(|t| {
            let table: toml::Table = std::str::from_utf8(t).ok()?.parse().ok()?;
            let vault = table.get("vault").and_then(|v| v.as_str()).unwrap_or(".");
            let dir = config_rel
                .as_deref()?
                .rsplit_once('/')
                .map_or("", |(d, _)| d);
            normalize(&format!("{dir}/{vault}"))
        });
        let mut paths = vec![disk_prefix.clone()];
        paths.extend(written.clone());
        paths.extend(config_rel.clone());
        let content: Vec<String> = config_rel.iter().cloned().collect();
        repo.export(source, &root, &paths, &content)?;
        let root = root.canonicalize()?;
        let fallback = || -> Result<Config> {
            let dir = root.join(&disk_prefix);
            std::fs::create_dir_all(&dir)?;
            let mut c = disk.clone();
            c.vault = dir.canonicalize()?;
            Ok(c)
        };
        let config = match (&config_rel, &written) {
            (Some(r), Some(w)) => {
                std::fs::create_dir_all(root.join(w))?;
                match Config::load_with(&root.join(r), false) {
                    Ok(c) if c.vault.starts_with(&root) => c,
                    Ok(_) => fallback()?,
                    // The commit would record a config that does not load.
                    Err(e) if source == Source::Staged => {
                        bail!("the staged {r} does not load: {e}")
                    }
                    Err(e) => {
                        eprintln!(
                            "wikiknit: the {name} version of {r} does not load ({e}); using the one on disk"
                        );
                        fallback()?
                    }
                }
            }
            _ => fallback()?,
        };
        let mut prefix = config
            .vault
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if !prefix.is_empty() {
            prefix.push('/');
        }
        Ok((Vault::load(config)?, prefix))
    };
    let (after, after_prefix) = copy(source, "after")?;
    let (before, before_prefix) = copy(Source::Head, "before")?;
    let id = |path: &str, prefix: &str| {
        path.strip_prefix(prefix)?
            .strip_suffix(".md")
            .map(str::to_string)
    };
    let renamed = repo
        .renames(source)?
        .into_iter()
        .filter_map(|(old, new)| Some((id(&old, &before_prefix)?, id(&new, &after_prefix)?)))
        .collect();
    Ok(Some(Versions {
        after,
        before,
        renamed,
    }))
}

/// The records of `after` that `before` does not have, by key (each record of `before`
/// cancels one record of `after` with the same key).
fn introduced<T, K: Ord>(
    after: Vec<T>,
    before: &[T],
    key_after: impl Fn(&T) -> K,
    key_before: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut old: BTreeMap<K, usize> = BTreeMap::new();
    for b in before {
        *old.entry(key_before(b)).or_default() += 1;
    }
    after
        .into_iter()
        .filter(|a| match old.get_mut(&key_after(a)) {
            Some(n) if *n > 0 => {
                *n -= 1;
                false
            }
            _ => true,
        })
        .collect()
}

/// A note id after the renames.
fn renamed<'a>(v: &'a Versions, id: &'a str) -> &'a str {
    v.renamed.get(id).map_or(id, String::as_str)
}

/// The findings `v.after` has and `v.before` does not.
fn new_findings(v: &Versions) -> Vec<rules::Finding> {
    let key = |f: &rules::Finding, note: &str| (note.to_string(), f.rule, f.key.clone());
    introduced(
        rules::check(&v.after, None),
        &rules::check(&v.before, None),
        |f| key(f, &f.note),
        |f| {
            (
                renamed(v, &f.note).to_string(),
                f.rule,
                renamed(v, &f.key).to_string(),
            )
        },
    )
}

/// The mentions `v.after` has and `v.before` does not.
fn new_mentions(v: &Versions) -> Vec<mentions::Mention> {
    let key = |m: &mentions::Mention, note: &str, target: &str| {
        (note.to_string(), m.term.to_lowercase(), target.to_string())
    };
    introduced(
        mentions::find(&v.after, None),
        &mentions::find(&v.before, None),
        |m| key(m, &m.note, &m.target),
        |m| key(m, renamed(v, &m.note), renamed(v, &m.target)),
    )
}

fn selection(vault: &Vault, sel: &Select) -> Result<Option<BTreeSet<String>>> {
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

fn opt(s: Option<&String>) -> String {
    s.cloned().unwrap_or_default()
}

fn init(cli: &Cli, dry_run: bool) -> Result<ExitCode> {
    if cli.format == Format::Json {
        bail!("init prints text only");
    }
    let report = wikiknit::init::run(&wikiknit::init::Options {
        cwd: std::env::current_dir()?,
        vault: cli.vault.clone(),
        config: cli.config.clone(),
        dry_run,
    })?;
    for line in &report.lines {
        out!("{line}");
    }
    if !report.next.is_empty() {
        out!("\nNext:");
        for (i, step) in report.next.iter().enumerate() {
            out!("{}. {step}", i + 1);
        }
    }
    Ok(if report.incomplete {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn snippet(into: Option<&std::path::Path>) -> Result<ExitCode> {
    use wikiknit::snippet::{self, Outcome};
    let Some(path) = into else {
        print!("{}", snippet::text());
        return Ok(ExitCode::SUCCESS);
    };
    let what = match snippet::write_into(path)? {
        Outcome::Created => "created with the wikiknit block",
        Outcome::Appended => "wikiknit block added at the end",
        Outcome::Replaced => "wikiknit block replaced",
        Outcome::Unchanged => "wikiknit block already current; not changed",
    };
    out!("{}: {what}", path.display());
    Ok(ExitCode::SUCCESS)
}

fn run(cli: Cli) -> Result<ExitCode> {
    match &cli.command {
        Command::Init { dry_run } => return init(&cli, *dry_run),
        Command::Snippet { into } => return snippet(into.as_deref()),
        _ => {}
    }
    let config = load_config(&cli)?;
    // `--changed`/`--staged` read their own copies of the vault.
    let since = match &cli.command {
        Command::Check(s) | Command::Mentions(s) => s.since_head(),
        _ => None,
    };
    let loaded = match since {
        Some(_) => None,
        None => Some(Vault::load(config.clone())?),
    };
    let vault = || loaded.as_ref().expect("loaded unless --changed/--staged");
    let fmt = cli.format;
    match &cli.command {
        Command::Check(sel) => {
            let findings = match sel.since_head() {
                Some(change) => {
                    let tmp = tempfile::tempdir()?;
                    versions(&config, change, &tmp)?
                        .map(|v| new_findings(&v))
                        .unwrap_or_default()
                }
                None => rules::check(vault(), selection(vault(), sel)?.as_ref()),
            };
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
            if errors > 0 && sel.staged {
                eprintln!(
                    "wikiknit: this commit adds {errors} link error(s) to the vault (above). Fix \
                     them and stage the fixes, then commit again."
                );
            }
            Ok(if errors > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Mentions(sel) => {
            let found = match sel.since_head() {
                Some(change) => {
                    let tmp = tempfile::tempdir()?;
                    versions(&config, change, &tmp)?
                        .map(|v| new_mentions(&v))
                        .unwrap_or_default()
                }
                None => mentions::find(vault(), selection(vault(), sel)?.as_ref()),
            };
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
        Command::Links {
            note,
            depth,
            outbound,
            inbound,
            filter,
        } => {
            let vault = vault();
            let start = vault.find_note(note)?;
            let filter = filter.build(vault)?;
            let dir = match (outbound, inbound) {
                (true, _) => Direction::Out,
                (_, true) => Direction::In,
                _ => Direction::Both,
            };
            let edges = links::neighbors(vault, start, *depth, dir, &filter);
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
            let vault = vault();
            let filter = filter.build(vault)?;
            let entries: Vec<catalog::Entry> = catalog::build(vault)
                .into_iter()
                .filter(|e| {
                    filter.is_empty() || vault.get(&e.id).is_some_and(|n| filter.matches(vault, n))
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
        Command::Hook { .. } => unreachable!("handled in main"),
        Command::Init { .. } | Command::Snippet { .. } => unreachable!("handled above"),
        Command::Graph => {
            let vault = vault();
            let r = graph::report(vault);
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
    }
}
