//! `wikiknit init`: set a vault up for checking. Find the vault and its git repository, write a
//! starter `wikiknit.toml` with only what the vault shows (link style, index notes, a templates
//! folder), and install the `pre-commit` and `pre-merge-commit` hooks that run
//! `wikiknit check --staged`. It never overwrites: an existing config is kept, and a hook it did
//! not write is left alone with the line to add to it. A second run changes nothing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::config::{CONFIG_FILE, Config};
use crate::parse::LinkKind;
use crate::vault::{Form, Target, Vault};
use crate::{Error, git};

/// The hooks a commit runs: `pre-merge-commit` for a merge that commits by itself.
pub const HOOKS: [&str; 2] = ["pre-commit", "pre-merge-commit"];

/// The second line of a hook `init` wrote: it may add lines to that file, and to no other.
const MARKER: &str = "# Written by `wikiknit init`";

/// A note linking at least this share of the other notes (and `HUB_MIN` of them) is a hub.
const HUB_SHARE: f64 = 0.25;
const HUB_MIN: usize = 5;
/// ... and at least this many times the median note's links.
const HUB_OVER_MEDIAN: usize = 3;

/// Links into folders needed to read a link style from the notes, and the share that must use
/// the full path for `full-path`.
const STYLE_MIN: usize = 5;
const STYLE_SHARE: f64 = 0.95;

#[derive(Debug, Clone)]
pub struct Options {
    pub cwd: PathBuf,
    /// The vault folder (`--vault`); found when not given.
    pub vault: Option<PathBuf>,
    /// The config file (`--config`): kept when it exists, written there when not.
    pub config: Option<PathBuf>,
    /// Report what would change; write nothing.
    pub dry_run: bool,
}

/// What `init` did, for the person or agent that ran it.
#[derive(Debug, Default)]
pub struct Report {
    /// One line per step.
    pub lines: Vec<String>,
    /// What to do next.
    pub next: Vec<String>,
    /// A step needs the user (a hook `init` will not edit): exit 1.
    pub incomplete: bool,
    /// Something was written (or would be, with `dry_run`).
    pub changed: bool,
}

/// A git work tree: its top and the folder that holds its hooks by default.
struct Repo {
    top: PathBuf,
    common: PathBuf,
}

/// git in `dir` as the user's own git would run there (its config read as is, so
/// `core.hooksPath` is the real one), but with no repository from the environment.
fn git_in(dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("git");
    c.args(["-c", "core.fsmonitor=false", "--no-pager"])
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    for var in git::REPO_ENV {
        c.env_remove(var);
    }
    c
}

fn repo_of(dir: &Path) -> Option<Repo> {
    let mut c = git_in(dir);
    c.args([
        "rev-parse",
        "--path-format=absolute",
        "--show-toplevel",
        "--git-common-dir",
    ]);
    let out = git::run(c, "").ok()?;
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines();
    let top = PathBuf::from(lines.next()?).canonicalize().ok()?;
    let common = PathBuf::from(lines.next()?).canonicalize().ok()?;
    Some(Repo { top, common })
}

/// `path` with its longest existing ancestor resolved (for a path that may not exist yet).
fn resolve(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut p = path;
    loop {
        if let Ok(real) = p.canonicalize() {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                p = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `path` for display: from `cwd` when inside it, else absolute.
fn show(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.display().to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// `path` as a single-quoted shell word.
fn shell_quote(path: &str) -> String {
    if !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return path.to_string();
    }
    format!("'{}'", path.replace('\'', r"'\''"))
}

fn has_obsidian(dir: &Path) -> bool {
    dir.join(".obsidian").is_dir()
}

/// Find the vault from `cwd`: the folder with Obsidian's `.obsidian/` at or above `cwd` (up to
/// the repository top), or the one below the repository top (or `cwd` outside git). Without
/// one, the error names the folder that holds the linked notes, for `--vault`. Returns the
/// folder and why.
fn discover(cwd: &Path, top: Option<&Path>) -> Result<(PathBuf, String), Error> {
    for dir in cwd.ancestors() {
        if has_obsidian(dir) {
            return Ok((dir.to_path_buf(), "it holds .obsidian/".into()));
        }
        if top.is_none_or(|t| dir == t) {
            break;
        }
    }
    // Folders below the repository top (or `cwd`), as git sees them (ignored and hidden
    // folders left out).
    let root = top.unwrap_or(cwd);
    let walk = || {
        ignore::WalkBuilder::new(root)
            .hidden(true)
            .max_depth(Some(8))
            .follow_links(false)
            .sort_by_file_name(std::cmp::Ord::cmp)
            .build()
            .filter_map(Result::ok)
    };
    let found: Vec<PathBuf> = walk()
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()) && has_obsidian(e.path()))
        .map(|e| e.path().to_path_buf())
        .collect();
    match found.as_slice() {
        [one] => return Ok((one.clone(), "it holds .obsidian/".into())),
        [] => {}
        many => {
            let names: Vec<String> = many.iter().map(|p| show(p, root)).collect();
            return Err(Error::Usage(format!(
                "several Obsidian vaults here ({}); name one with --vault <folder>",
                names.join(", ")
            )));
        }
    }
    let mut with_links = 0usize;
    let mut common: Option<PathBuf> = None;
    for e in walk() {
        let path = e.path();
        if !e.file_type().is_some_and(|t| t.is_file())
            || !path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let parsed = crate::parse::parse(&text);
        if !parsed
            .links
            .iter()
            .any(|l| matches!(l.kind, LinkKind::Wiki | LinkKind::Embed))
        {
            continue;
        }
        with_links += 1;
        let dir = path.parent().unwrap_or(root).to_path_buf();
        common = Some(match common {
            None => dir,
            Some(c) => c
                .ancestors()
                .find(|a| dir.starts_with(a))
                .unwrap_or(root)
                .to_path_buf(),
        });
    }
    // No `.obsidian/`: say where the linked notes are, but do not guess (a stray link in a
    // README would make the whole repository the vault).
    let hint = match common {
        Some(dir) => format!(
            "; the {with_links} Markdown file(s) with a [[wikilink]] are all in {}: if that is \
             the vault, run `wikiknit --vault {} init`",
            show(&dir, cwd),
            shell_quote(&show(&dir, cwd))
        ),
        None => String::new(),
    };
    Err(Error::Usage(format!(
        "no Obsidian vault (a folder with .obsidian/) in {}{hint}; name the vault with \
         --vault <folder>",
        root.display()
    )))
}

/// One line of the starter config: the key and value as TOML, and why.
struct Setting {
    toml: String,
    why: String,
}

/// A folder name from an Obsidian setting (`/Templates/`, `./x`) as a vault-relative prefix.
fn folder_setting(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches("./").trim_matches('/');
    (!v.is_empty() && !v.split('/').any(|p| p == ".." || p == ".")).then(|| v.to_string())
}

fn json_field(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get(key)?.as_str().map(str::to_string)
}

/// The templates folder: the one Obsidian's Templates or Templater plugin names, else a
/// top-level folder named `Templates` (any case, `_` prefix allowed).
fn templates_folder(vault: &Path) -> Option<(String, String)> {
    let named = [
        (
            ".obsidian/templates.json",
            "folder",
            "the templates folder of Obsidian's Templates plugin",
        ),
        (
            ".obsidian/plugins/templater-obsidian/data.json",
            "templates_folder",
            "the templates folder of the Templater plugin",
        ),
    ];
    for (file, key, why) in named {
        if let Some(dir) = json_field(&vault.join(file), key)
            .as_deref()
            .and_then(folder_setting)
            && vault.join(&dir).is_dir()
        {
            return Some((dir, format!("{why}, in {file}")));
        }
    }
    let mut dirs: Vec<String> = std::fs::read_dir(vault)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| matches!(n.to_lowercase().as_str(), "templates" | "_templates"))
        .collect();
    dirs.sort();
    let dir = dirs.into_iter().next()?;
    Some((dir, "a top-level templates folder".into()))
}

/// `s` as a one-line TOML basic string.
fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What the vault shows: the settings to write, and notes on what it could not tell.
fn infer(vault_dir: &Path) -> Result<(Vec<Setting>, Vec<String>), Error> {
    let mut settings = Vec::new();
    let mut unset = Vec::new();
    let mut cfg = Config::for_vault(vault_dir.to_path_buf());
    let templates = templates_folder(vault_dir);
    if let Some((dir, _)) = &templates {
        cfg.ignore = vec![format!("{dir}/")];
    }
    let vault = Vault::load(cfg)?;

    // Link style, from links that name a note in a folder (only there do the styles differ).
    let (mut full, mut short) = (0usize, 0usize);
    for note in &vault.notes {
        for r in vault.resolve_links(note) {
            if r.raw.kind == LinkKind::Markdown {
                continue;
            }
            if let Target::Note(i) = r.target
                && vault.notes[i].id.contains('/')
            {
                match r.form {
                    Form::FullPath => full += 1,
                    Form::Short => short += 1,
                    Form::Relative | Form::Ambiguous => {}
                }
            }
        }
    }
    let n = full + short;
    let app = json_field(&vault_dir.join(".obsidian/app.json"), "newLinkFormat");
    #[allow(clippy::cast_precision_loss)]
    let share = |k: usize| k as f64 / n.max(1) as f64;
    if n >= STYLE_MIN && share(full) >= STYLE_SHARE {
        settings.push(Setting {
            toml: format!("link_style = {}", toml_str("full-path")),
            why: format!("{full} of {n} links to notes in folders give the full path"),
        });
    } else if n >= STYLE_MIN && short * 2 > n {
        settings.push(Setting {
            toml: format!("link_style = {}", toml_str("shortest")),
            why: format!("{short} of {n} links to notes in folders give a shorter name"),
        });
    } else if n < STYLE_MIN && short == 0 && app.as_deref() == Some("absolute") {
        settings.push(Setting {
            toml: format!("link_style = {}", toml_str("full-path")),
            why: "Obsidian writes new links with the full path, in .obsidian/app.json".into(),
        });
    } else if n >= STYLE_MIN {
        unset.push(format!(
            "link_style: not set (default \"shortest\", which accepts both): links to notes in \
             folders are mixed ({full} full path, {short} shorter)"
        ));
    } else {
        unset.push(format!(
            "link_style: not set (default \"shortest\"): {n} links to notes in folders, too few \
             to tell"
        ));
    }

    if let Some((dir, why)) = templates {
        settings.push(Setting {
            toml: format!("ignore = [{}]", toml_str(&format!("{dir}/"))),
            why,
        });
    }

    // Hubs: notes that link a large share of the others, far more than a typical note does
    // (in a small, dense vault every note links a quarter of the others; none is an index).
    let others = vault.notes.len().saturating_sub(1);
    let outs: Vec<usize> = vault.outbound_all().iter().map(BTreeSet::len).collect();
    let mut sorted = outs.clone();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let mut hubs: Vec<(String, usize)> = outs
        .iter()
        .enumerate()
        .map(|(i, &k)| (vault.notes[i].id.clone(), k))
        .filter(|&(_, k)| {
            #[allow(clippy::cast_precision_loss)]
            let big = k as f64 >= HUB_SHARE * others as f64;
            k >= HUB_MIN && big && k >= HUB_OVER_MEDIAN * median.max(1)
        })
        .collect();
    hubs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !hubs.is_empty() {
        let list: Vec<String> = hubs.iter().map(|(id, _)| toml_str(id)).collect();
        let why: Vec<String> = hubs
            .iter()
            .map(|(id, k)| format!("{id} links {k} of {others} other notes"))
            .collect();
        settings.push(Setting {
            toml: format!("hubs = [{}]", list.join(", ")),
            why: format!("index notes: {}", why.join("; ")),
        });
    }
    Ok((settings, unset))
}

/// `s` on one line: a note name with a line break must not end a TOML comment early.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// The starter config text.
fn config_text(vault_rel: &str, settings: &[Setting]) -> String {
    let mut out = format!(
        "# wikiknit config, started by `wikiknit init` (wikiknit {}).\n\
         # Rules only you know (require_link, require_inbound, forbid_link, stale, drift):\n\
         # https://github.com/victor-kwak/wikiknit#configure\n\
         vault = {}\n",
        env!("CARGO_PKG_VERSION"),
        toml_str(vault_rel)
    );
    for s in settings {
        out.push_str(&format!("\n# {}\n{}\n", one_line(&s.why), s.toml));
    }
    out
}

/// The hook's check for a config: `wikiknit check --staged`, with `--config` when git (which
/// runs hooks at the top of the work tree) would not find the config from there.
struct Check {
    command: String,
    /// The config as the hook names it: from the top, or absolute when outside the repository.
    config: String,
}

fn check_for(config: &Path, top: &Path) -> Check {
    let path = config.strip_prefix(top).map_or_else(
        |_| config.display().to_string(),
        |rel| rel.to_string_lossy().replace('\\', "/"),
    );
    let command = if path == CONFIG_FILE {
        "wikiknit check --staged".to_string()
    } else {
        format!("wikiknit --config {} check --staged", shell_quote(&path))
    };
    Check {
        command,
        config: path,
    }
}

fn hook_line(command: &str) -> String {
    format!("{command} || exit $?")
}

fn new_hook(command: &str) -> String {
    format!(
        "#!/bin/sh\n\
         {MARKER}: blocks a commit that adds a link error to the vault.\n\
         # Skip once with `git commit --no-verify`.\n\
         command -v wikiknit >/dev/null 2>&1 || {{ echo \"wikiknit is not on PATH; vault not checked\" >&2; exit 0; }}\n\
         {}\n",
        hook_line(command)
    )
}

/// The line to add to a hook `init` does not edit. It runs only where the config is, so in a
/// hooks folder other repositories share it does not block their commits.
fn manual_line(check: &Check) -> String {
    format!(
        "[ -f {} ] && command -v wikiknit >/dev/null 2>&1 && {{ {} ; }}",
        shell_quote(&check.config),
        hook_line(&check.command)
    )
}

/// The tool that owns a repository's hook files, if one does: where the check goes instead.
fn hook_manager(top: &Path, hooks: &Path, check: &Check) -> Option<String> {
    let command = &check.command;
    if top.join(".pre-commit-config.yaml").is_file() {
        return Some(format!(
            "the pre-commit framework writes this hook: add a `repo: local` hook with `entry: \
             {command}`, `language: system`, `pass_filenames: false`, `always_run: true` to \
             .pre-commit-config.yaml instead"
        ));
    }
    if hooks.components().any(|c| c.as_os_str() == ".husky") {
        return Some(format!(
            "husky writes this folder: add `{command}` to .husky/pre-commit instead (and to \
             .husky/pre-merge-commit)"
        ));
    }
    [
        "lefthook.yml",
        ".lefthook.yml",
        "lefthook.yaml",
        ".lefthook.yaml",
    ]
    .iter()
    .find(|f| top.join(f).is_file())
    .map(|f| {
        format!(
            "lefthook writes this hook: add a `wikiknit` command running `{command}` under \
                 pre-commit (and pre-merge-commit) in {f} instead"
        )
    })
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt as _;
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mut perm = meta.permissions();
    perm.set_mode(perm.mode() | 0o755);
    std::fs::set_permissions(path, perm).map_err(|e| Error::io(path, e))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), Error> {
    Ok(())
}

/// The folder git runs this repository's hooks from, or why `init` will not write there.
fn hooks_dir(repo: &Repo) -> Result<PathBuf, String> {
    let mut c = git_in(&repo.top);
    c.args(["config", "--show-scope", "--get", "core.hooksPath"]);
    if let Ok(out) = git::run(c, "") {
        let text = String::from_utf8_lossy(&out);
        let (scope, value) = text.trim().split_once('\t').unwrap_or(("", text.trim()));
        if !matches!(scope, "local" | "worktree") {
            return Err(format!(
                "core.hooksPath is {value} in your {scope} git config: those hooks serve other \
                 repositories too, so wikiknit does not edit them"
            ));
        }
    }
    let mut c = git_in(&repo.top);
    c.args(["rev-parse", "--path-format=absolute", "--git-path", "hooks"]);
    let out = git::run(c, "finding the hooks folder").map_err(|e| e.to_string())?;
    let dir = resolve(Path::new(String::from_utf8_lossy(&out).trim()));
    if !(dir.starts_with(&repo.top) || dir.starts_with(&repo.common)) {
        return Err(format!(
            "core.hooksPath is {}, outside this repository: those hooks may serve other \
             repositories too, so wikiknit does not edit them",
            dir.display()
        ));
    }
    Ok(dir)
}

/// A path git does not ignore (so a hook in a tracked hooks folder gets committed).
fn tracked_place(repo: &Repo, path: &Path) -> bool {
    if !path.starts_with(&repo.top) || path.starts_with(&repo.common) {
        return false;
    }
    let mut c = git_in(&repo.top);
    c.args(["check-ignore", "-q", "--no-index", "--"]).arg(path);
    // Exit 1: not ignored.
    c.status().is_ok_and(|s| s.code() == Some(1))
}

/// Install one hook. Returns its report line, whether it was written, and whether it needs
/// the user.
fn install_hook(
    path: &Path,
    check: &Check,
    cwd: &Path,
    dry_run: bool,
    manager: Option<&str>,
) -> Result<(String, bool, bool), Error> {
    let command = check.command.as_str();
    let shown = show(path, cwd);
    // What was done, or with `dry_run` what would be.
    let would = |done: &str, plan: &str| {
        if dry_run {
            format!("would {plan}")
        } else {
            done.to_string()
        }
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::io(path, e)),
    };
    let Some(meta) = meta else {
        if !dry_run {
            let dir = path.parent().unwrap_or(path);
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(|e| Error::io(path, e))?;
            f.write_all(new_hook(command).as_bytes())
                .map_err(|e| Error::io(path, e))?;
            make_executable(path)?;
        }
        return Ok((
            format!("hook    {}: {shown}", would("wrote", "write")),
            true,
            false,
        ));
    };
    let text = std::fs::read(path)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let line = hook_line(command);
    let ours = !meta.file_type().is_symlink()
        && text.lines().nth(1).is_some_and(|l| l.starts_with(MARKER));
    if ours {
        let has_line = text.lines().any(|l| l.trim() == line);
        let executable = is_executable(&meta);
        if has_line && executable {
            return Ok((format!("hook    ok: {shown}"), false, false));
        }
        if !dry_run {
            if !has_line {
                let mut new = text.clone();
                if !new.ends_with('\n') {
                    new.push('\n');
                }
                new.push_str(&line);
                new.push('\n');
                // Into a new file beside it (a fresh name, never an existing file or link),
                // then over it: never a half-written hook.
                let dir = path.parent().unwrap_or(path);
                let mut tmp = tempfile::Builder::new()
                    .prefix(".wikiknit-")
                    .tempfile_in(dir)
                    .map_err(|e| Error::io(dir, e))?;
                use std::io::Write as _;
                tmp.write_all(new.as_bytes())
                    .map_err(|e| Error::io(tmp.path(), e))?;
                make_executable(tmp.path())?;
                tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
            } else {
                make_executable(path)?;
            }
        }
        let what = if has_line {
            ("made executable", "make executable")
        } else {
            ("added this config's check to", "add this config's check to")
        };
        return Ok((
            format!("hook    {}: {shown}", would(what.0, what.1)),
            true,
            false,
        ));
    }
    // A hook that already runs this config's check (written by hand, as `exec ...`).
    let runs_wikiknit = text.lines().any(|l| {
        let code = l.split('#').next().unwrap_or("");
        code.contains(&check.command)
    });
    if runs_wikiknit {
        let target = std::fs::metadata(path).ok();
        if target.as_ref().is_some_and(is_executable) {
            return Ok((
                format!(
                    "hook    ok: {shown} (not written by wikiknit; it runs wikiknit check --staged)"
                ),
                false,
                false,
            ));
        }
        return Ok((
            format!(
                "hook    {shown} runs wikiknit but is not executable, so git skips it: \
                 chmod +x {}",
                shell_quote(&path.display().to_string())
            ),
            false,
            true,
        ));
    }
    let mut msg = format!(
        "hook    not changed: {shown} exists and wikiknit did not write it. Add this line to it, \
         before any `exec` or `exit`:\n          {}",
        manual_line(check)
    );
    if let Some(manager) = manager {
        msg.push_str(&format!("\n          ({manager})"));
    }
    Ok((msg, false, true))
}

/// Run `init`.
pub fn run(opts: &Options) -> Result<Report, Error> {
    let cwd = opts
        .cwd
        .canonicalize()
        .map_err(|e| Error::io(&opts.cwd, e))?;
    let mut report = Report::default();
    let abs = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    let vault_arg = opts
        .vault
        .as_deref()
        .map(|v| abs(v).canonicalize().map_err(|e| Error::io(v, e)))
        .transpose()?;
    if let Some(v) = &vault_arg
        && !v.is_dir()
    {
        return Err(Error::Usage(format!(
            "vault {} is not a folder",
            v.display()
        )));
    }

    // The vault to set up: named, or found from the current folder.
    let chosen = |report: &mut Report| -> Result<PathBuf, Error> {
        if let Some(v) = &vault_arg {
            report
                .lines
                .push(format!("vault   {} (--vault)", show(v, &cwd)));
            return Ok(v.clone());
        }
        let top = repo_of(&cwd).map(|r| r.top);
        let (v, why) = discover(&cwd, top.as_deref())?;
        report
            .lines
            .push(format!("vault   {} ({why})", show(&v, &cwd)));
        Ok(v)
    };

    // The config: named (`--config`), found from the current folder, one that already names
    // the vault (in its folder or above, up to the repository top), or a new one at the top.
    let (config_path, existing, vault): (PathBuf, Option<Config>, PathBuf) =
        if let Some(c) = &opts.config {
            let c = resolve(&abs(c));
            if c.is_file() {
                let cfg = Config::load(&c)?;
                let v = cfg.vault.clone();
                (c, Some(cfg), v)
            } else {
                (c, None, chosen(&mut report)?)
            }
        } else if let Some(file) = Config::find(&cwd).filter(|_| vault_arg.is_none()) {
            let cfg = Config::load(&file)?;
            let v = cfg.vault.clone();
            (
                file.canonicalize().map_err(|e| Error::io(&file, e))?,
                Some(cfg),
                v,
            )
        } else {
            let v = chosen(&mut report)?;
            let top = repo_of(&v).map(|r| r.top);
            let mut found = None;
            for dir in v.ancestors() {
                let file = dir.join(CONFIG_FILE);
                if file.is_file()
                    && let Ok(cfg) = Config::load(&file)
                    && cfg.vault == v
                {
                    found = Some((file, cfg));
                    break;
                }
                if top.as_deref().is_none_or(|t| t == dir) {
                    break;
                }
            }
            match found {
                Some((file, cfg)) => (file, Some(cfg), v),
                None => {
                    // Outside git, the config goes in the vault folder itself.
                    let file = top.as_deref().unwrap_or(&v).join(CONFIG_FILE);
                    if file.is_file() {
                        return Err(Error::Usage(format!(
                            "{} exists and names another vault ({}); pass --config <new file> \
                             to keep both",
                            file.display(),
                            Config::load(&file)?.vault.display()
                        )));
                    }
                    (file, None, v)
                }
            }
        };

    let vault = match &existing {
        Some(cfg) => {
            if let Some(v) = &vault_arg
                && *v != cfg.vault
            {
                return Err(Error::Usage(format!(
                    "{} names another vault ({}); pass --config <new file> to keep both",
                    config_path.display(),
                    cfg.vault.display()
                )));
            }
            report.lines.push(format!(
                "config  ok: {} exists (vault {}); not changed",
                show(&config_path, &cwd),
                show(&cfg.vault, &cwd)
            ));
            vault
        }
        None => {
            let base = config_path.parent().unwrap_or(&cwd).to_path_buf();
            let base = resolve(&base);
            let Ok(rel) = vault.strip_prefix(&base) else {
                return Err(Error::Usage(format!(
                    "the vault {} is not inside the config's folder {}: a config names its vault \
                     by a path inside its own folder",
                    vault.display(),
                    base.display()
                )));
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let rel = if rel.is_empty() { ".".to_string() } else { rel };
            let (settings, unset) = infer(&vault)?;
            let text = config_text(&rel, &settings);
            if !opts.dry_run {
                std::fs::create_dir_all(&base).map_err(|e| Error::io(&base, e))?;
                // Written beside its place and loaded there first: a config that does not load
                // (a vault that resolves to the home folder, say) is never left behind.
                use std::io::Write as _;
                let mut tmp = tempfile::Builder::new()
                    .prefix(".wikiknit-")
                    .suffix(".toml")
                    .tempfile_in(&base)
                    .map_err(|e| Error::io(&base, e))?;
                tmp.write_all(text.as_bytes())
                    .map_err(|e| Error::io(tmp.path(), e))?;
                Config::load(tmp.path()).map_err(|e| match e {
                    Error::Config { message, .. } => Error::Config {
                        path: config_path.clone(),
                        message,
                    },
                    e => e,
                })?;
                tmp.persist_noclobber(&config_path)
                    .map_err(|e| Error::io(&config_path, e.error))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o644))
                        .map_err(|e| Error::io(&config_path, e))?;
                }
            }
            report.changed = true;
            report.lines.push(format!(
                "config  {}: {}",
                if opts.dry_run { "would write" } else { "wrote" },
                show(&config_path, &cwd)
            ));
            report
                .lines
                .push(format!("          vault = {}", toml_str(&rel)));
            for s in &settings {
                report
                    .lines
                    .push(format!("          {}  # {}", s.toml, one_line(&s.why)));
            }
            for u in &unset {
                report.lines.push(format!("          {u}"));
            }
            vault
        }
    };

    // Hooks, in the vault's repository.
    let mut commit: Vec<String> = Vec::new();
    if existing.is_none() {
        commit.push(show(&config_path, &cwd));
    }
    let mut in_git_hooks = false;
    match repo_of(&vault) {
        None => {
            report.lines.push(format!(
                "hooks   skipped: {} is not in a git work tree (after `git init`, run \
                 `wikiknit init` again)",
                show(&vault, &cwd)
            ));
            commit.clear();
        }
        Some(repo) => {
            if !config_path.starts_with(&repo.top) || !tracked_place(&repo, &config_path) {
                commit.clear();
            }
            let check = check_for(&config_path, &repo.top);
            match hooks_dir(&repo) {
                Err(why) => {
                    report.incomplete = true;
                    report.lines.push(format!("hooks   not changed: {why}."));
                    for name in HOOKS {
                        report.lines.push(format!(
                            "          Add to its {name}: {}",
                            manual_line(&check)
                        ));
                    }
                }
                Ok(dir) => {
                    let manager = hook_manager(&repo.top, &dir, &check);
                    for name in HOOKS {
                        let path = dir.join(name);
                        let (line, wrote, needs_user) =
                            install_hook(&path, &check, &cwd, opts.dry_run, manager.as_deref())?;
                        report.lines.push(line);
                        report.changed |= wrote;
                        report.incomplete |= needs_user;
                        if wrote && tracked_place(&repo, &path) {
                            commit.push(show(&path, &cwd));
                        }
                    }
                    in_git_hooks = dir.starts_with(&repo.common);
                }
            }
        }
    }

    // Next steps.
    if !report.changed && !report.incomplete {
        report
            .lines
            .push("Nothing to change: this vault is set up.".into());
    }
    report.next.push(
        "Put the agent instructions in this project's instruction file (the one your agent \
         reads): `wikiknit snippet --into AGENTS.md` (or CLAUDE.md, ...)."
            .into(),
    );
    report.next.push(
        "Add the rules only you know (require_link, require_inbound, forbid_link, stale, drift): \
         https://github.com/victor-kwak/wikiknit#configure"
            .into(),
    );
    report.next.push(
        "Run `wikiknit check` and fix or accept what it reports: the hook blocks only errors a \
         commit adds."
            .into(),
    );
    if !commit.is_empty() {
        report.next.push(format!(
            "Commit {} with the instruction file from step 1, and nothing else.",
            commit.join(" and ")
        ));
    }
    if in_git_hooks {
        report.next.push(
            "Hooks in .git/hooks are not committed: in a new clone, run `wikiknit init` again."
                .into(),
        );
    }
    let found_from_cwd = Config::find(&cwd)
        .and_then(|f| f.canonicalize().ok())
        .is_some_and(|f| f == config_path);
    if !found_from_cwd && (existing.is_some() || !opts.dry_run) {
        report.next.push(format!(
            "From here, wikiknit does not find this config by itself: add `--config {}`.",
            shell_quote(&config_path.display().to_string())
        ));
    }
    Ok(report)
}
