//! The git repository a vault lives in: copies of the vault as committed (`HEAD`), as the next
//! commit will record it (the index), or as the work tree has it (tracked and not ignored), for
//! checks that report only what a change introduces; and commit counts in the repositories that
//! project notes describe, for the `drift` rule.

use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::Error;

/// Environment variables that point git at a repository. A git hook runs with some of them
/// set (`GIT_DIR`, and `GIT_INDEX_FILE`, often relative to the work-tree top).
pub(crate) const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
];

/// The repository variables that hold a path.
const PATH_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
];

/// git with settings that keep a repository's own config from running programs: no fsmonitor,
/// no hooks, no pager, no signature checks, no network, no prompts; and pathspecs taken
/// literally (a folder named `:x` or `*` is that folder).
fn base(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "log.showSignature=false",
        "-c",
        "protocol.allow=never",
        "--no-pager",
        "--no-replace-objects",
        "--literal-pathspecs",
    ])
    .current_dir(dir)
    .env("GIT_OPTIONAL_LOCKS", "0")
    .env("GIT_TERMINAL_PROMPT", "0")
    .env("GIT_NO_LAZY_FETCH", "1")
    .env_remove("GIT_EXTERNAL_DIFF")
    .stdin(Stdio::null());
    cmd
}

/// git for a repository found from `dir` alone: nothing from the environment's repository
/// leaks in (another repository for `drift`, or the vault's own while it is being found).
pub(crate) fn other(dir: &Path) -> Command {
    let mut cmd = base(dir);
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

pub(crate) fn run(mut cmd: Command, what: &str) -> Result<Vec<u8>, Error> {
    let out = cmd
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::Usage(format!("{what}: cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(Error::Usage(format!(
            "{what}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

/// The work-tree top and absolute git dir that `cmd` (a `rev-parse`) finds.
fn locate(mut cmd: Command) -> Option<(PathBuf, PathBuf)> {
    cmd.args(["rev-parse", "--show-toplevel", "--absolute-git-dir"]);
    let out = cmd.stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let top = PathBuf::from(lines.next()?).canonicalize().ok()?;
    let dir = PathBuf::from(lines.next()?).canonicalize().ok()?;
    Some((top, dir))
}

/// A path from git output as raw bytes.
#[cfg(unix)]
fn os(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::OsStr::from_bytes(bytes).to_os_string()
}

#[cfg(not(unix))]
fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// Which version of the vault to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The last commit.
    Head,
    /// What `git commit` will record: the index (as a tree, so `git add -N` entries, which
    /// are not committed, are left out).
    Staged,
    /// The work tree: tracked files and new files git does not ignore.
    WorkTree,
}

/// The repository that holds a vault.
#[derive(Debug)]
pub struct Repo {
    /// Top folder of the work tree.
    pub top: PathBuf,
    /// The vault's path inside it, with a trailing `/` (empty when the vault is the top).
    pub prefix: String,
    /// Whether the repository the environment names (a hook's) is this one. A hook run for
    /// another repository (a superproject whose config names a vault inside a submodule)
    /// commits nothing of the vault.
    pub in_commit: bool,
    /// The work-tree top of the repository being committed, when it is another one.
    pub committing: Option<PathBuf>,
    /// Repository variables from the environment, made absolute.
    env: Vec<(&'static str, PathBuf)>,
    /// Objects `--staged` writes (the index as a tree) go here, not into the repository.
    scratch: Option<(PathBuf, PathBuf)>,
    /// The tree `Staged` records, once written.
    staged_tree: std::cell::OnceCell<String>,
}

/// A file in a version of the vault.
struct Entry {
    /// Path from the repository top, as git prints it (raw bytes).
    path: Vec<u8>,
    kind: Kind,
}

enum Kind {
    /// A blob: its object id.
    Blob(String),
    /// A symlink: the object id of its target text.
    Link(String),
    /// A submodule: copied from its work tree on disk.
    Submodule,
    /// A work-tree file or symlink, read from disk.
    Disk,
}

impl Repo {
    /// The repository `vault` is in, and how the environment relates to it.
    ///
    /// The vault's own repository is found from the vault folder with the repository
    /// variables cleared. When a hook set them, the repository they name is found too (from
    /// the current folder; relative values that do not resolve there are taken from the
    /// vault's work-tree top, where git ran the hook before it changed folder). If it is the
    /// vault's, its variables are kept (they name the index being committed); if it is
    /// another, the vault is not in this commit.
    ///
    /// Only a commit (`for_commit`) takes the environment's repository into account; for
    /// other runs it is used only when the vault has no repository of its own (a separate git
    /// dir), so a stray `GIT_DIR` in a shell does not change what `--changed` reads.
    pub fn of(vault: &Path, for_commit: bool) -> Result<Self, Error> {
        let vault = vault.canonicalize().map_err(|e| Error::io(vault, e))?;
        let own = locate(other(&vault));
        let set: Vec<(&'static str, OsString)> = PATH_ENV
            .iter()
            .filter_map(|&v| {
                std::env::var_os(v)
                    .filter(|x| !x.is_empty())
                    .map(|x| (v, x))
            })
            .filter(|_| for_commit || own.is_none())
            .collect();
        let absolute = |base: &Path| -> Vec<(&'static str, PathBuf)> {
            set.iter()
                .map(|(v, x)| {
                    let p = PathBuf::from(x);
                    (*v, if p.is_relative() { base.join(p) } else { p })
                })
                .collect()
        };
        let with = |env: &[(&'static str, PathBuf)], dir: &Path| {
            let mut c = other(dir);
            for (v, p) in env {
                c.env(v, p);
            }
            c
        };
        let mut env = Vec::new();
        let mut in_commit = true;
        let mut committing_top = None;
        let top = if set.is_empty() {
            own.as_ref()
                .ok_or_else(|| Error::Usage("the vault is not in a git work tree".into()))?
                .0
                .clone()
        } else {
            let cwd = std::env::current_dir().map_err(|e| Error::io(Path::new("."), e))?;
            let from_cwd = absolute(&cwd);
            let from_top = own.as_ref().map(|(t, _)| absolute(t));
            // A reading of the variables holds when every path it names exists. Git gives a
            // hook one relative value (`GIT_INDEX_FILE=.git/index` for a plain commit),
            // relative to the top it ran the hook at; a hook that changed folder still has it.
            let holds = |e: &[(&'static str, PathBuf)]| e.iter().all(|(_, p)| p.exists());
            let committing = Some(from_cwd)
                .filter(|e| holds(e))
                .and_then(|e| locate(with(&e, &cwd)).map(|found| (found, e)))
                .or_else(|| {
                    let (t, _) = own.as_ref()?;
                    let e = from_top.filter(|e| holds(e))?;
                    locate(with(&e, t)).map(|found| (found, e))
                });
            match (&own, committing) {
                (Some((own_top, own_dir)), Some(((top, dir), e))) => {
                    // The committing repository tracks the vault's files itself (a dotfiles
                    // repository around a vault that is also its own repository).
                    let tracks_vault = || {
                        let Ok(rel) = vault.strip_prefix(&top) else {
                            return false;
                        };
                        let mut c = with(&e, &top);
                        c.args(["ls-files", "-z", "--"]).arg(rel);
                        run(c, "").is_ok_and(|out| !out.is_empty())
                    };
                    if dir == *own_dir || tracks_vault() {
                        env = e;
                        top
                    } else {
                        in_commit = false;
                        committing_top = Some(top);
                        own_top.clone()
                    }
                }
                // A repository named only by the environment (a separate git dir, such as a
                // bare dotfiles repository with its work tree elsewhere).
                (None, Some(((top, _), e))) => {
                    env = e;
                    top
                }
                (Some((own_top, _)), None) => own_top.clone(),
                (None, None) => {
                    return Err(Error::Usage("the vault is not in a git work tree".into()));
                }
            }
        };
        let rel = vault.strip_prefix(&top).map_err(|_| {
            Error::Usage(format!(
                "vault {} is outside its repository {}",
                vault.display(),
                top.display()
            ))
        })?;
        let mut prefix = rel.to_string_lossy().replace('\\', "/");
        if !prefix.is_empty() {
            prefix.push('/');
        }
        if !env.iter().any(|(v, _)| *v == "GIT_WORK_TREE") {
            env.push(("GIT_WORK_TREE", top.clone()));
        }
        if let Some((_, index)) = env.iter().find(|(v, _)| *v == "GIT_INDEX_FILE")
            && !index.exists()
        {
            return Err(Error::Usage(format!(
                "GIT_INDEX_FILE names {}, which does not exist",
                index.display()
            )));
        }
        Ok(Self {
            top,
            prefix,
            in_commit,
            committing: committing_top,
            env,
            scratch: None,
            staged_tree: std::cell::OnceCell::new(),
        })
    }

    /// Write new objects (the staged tree) into `dir`, reading the repository's own objects
    /// as alternates: the repository is only read.
    pub fn scratch_objects(&mut self, dir: &Path) -> Result<(), Error> {
        let mut c = self.git();
        c.args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ]);
        let real = run(c, "finding the object store")?;
        let real = PathBuf::from(String::from_utf8_lossy(&real).trim());
        // The real store as an alternate, in `info/alternates` (one path per line), which takes
        // any path; the variable would split one with a `:`.
        let info = dir.join("info");
        std::fs::create_dir_all(&info).map_err(|e| Error::io(&info, e))?;
        let mut list = real.clone().into_os_string();
        list.push("\n");
        let file = info.join("alternates");
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            std::fs::write(&file, list.as_bytes()).map_err(|e| Error::io(&file, e))?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&file, list.to_string_lossy().as_bytes())
                .map_err(|e| Error::io(&file, e))?;
        }
        self.scratch = Some((dir.to_path_buf(), real));
        Ok(())
    }

    /// git in the vault's repository, at its top.
    fn git(&self) -> Command {
        let mut cmd = other(&self.top);
        for (var, value) in &self.env {
            cmd.env(var, value);
        }
        if let Some((dir, _)) = &self.scratch {
            cmd.env("GIT_OBJECT_DIRECTORY", dir);
            // The environment's own alternates (if any) still apply.
            if let Some(more) = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
                cmd.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", more);
            }
        }
        cmd
    }

    fn has_head(&self) -> bool {
        self.git()
            .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The tree a version records, if it has one (`HEAD` before the first commit has none).
    fn tree(&self, source: Source) -> Result<Option<String>, Error> {
        match source {
            Source::Head => Ok(self.has_head().then(|| "HEAD".to_string())),
            Source::Staged => {
                if let Some(t) = self.staged_tree.get() {
                    return Ok(Some(t.clone()));
                }
                let mut c = self.git();
                c.arg("write-tree");
                let out = run(c, "writing the index as a tree")?;
                let t = String::from_utf8_lossy(&out).trim().to_string();
                let _ = self.staged_tree.set(t.clone());
                Ok(Some(t))
            }
            Source::WorkTree => Ok(None),
        }
    }

    /// The content of the file at `path` (from the top) in `source`, if it is there.
    pub fn read(&self, source: Source, path: &str) -> Result<Option<Vec<u8>>, Error> {
        if source == Source::WorkTree {
            return Ok(std::fs::read(self.top.join(path)).ok());
        }
        let Some(tree) = self.tree(source)? else {
            return Ok(None);
        };
        let out = self
            .git()
            .args(["cat-file", "blob", &format!("{tree}:{path}")])
            .stderr(Stdio::null())
            .output()
            .map_err(|e| Error::Usage(format!("reading {path}: cannot run git: {e}")))?;
        Ok(out.status.success().then_some(out.stdout))
    }

    /// Copy `paths` (files or folders, from the top; empty paths mean the whole tree) from
    /// `source` into `dest`, laid out as in the repository. Notes get their content, symlinks
    /// their target, submodules their work tree's files; other files are written empty, since
    /// only their existence matters to links. Files in `content` (paths from the top, such as
    /// the config) get their content too.
    pub fn export(
        &self,
        source: Source,
        dest: &Path,
        paths: &[String],
        content: &[String],
    ) -> Result<(), Error> {
        let targets = self.export_paths(source, dest, paths, content)?;
        // Symlinks out of the exported folders to elsewhere in the repository: their
        // targets too (one level), so links through them resolve as they do on disk.
        let outside: Vec<String> = targets
            .into_iter()
            .filter(|t| !paths.iter().any(|p| p.is_empty() || under(t, p)))
            .collect();
        if !outside.is_empty() {
            self.export_paths(source, dest, &outside, content)?;
        }
        Ok(())
    }

    /// `export` without following symlinks out; returns the symlinks' targets that stay in
    /// the repository (paths from the top).
    fn export_paths(
        &self,
        source: Source,
        dest: &Path,
        paths: &[String],
        content: &[String],
    ) -> Result<Vec<String>, Error> {
        std::fs::create_dir_all(dest).map_err(|e| Error::io(dest, e))?;
        let entries = match (source, self.tree(source)?) {
            (Source::WorkTree, _) => self.work_tree_entries(paths)?,
            (_, Some(tree)) => self.tree_entries(&tree, paths)?,
            (_, None) => return Ok(Vec::new()),
        };
        let note = |p: &[u8]| {
            p.to_ascii_lowercase().ends_with(b".md") || content.iter().any(|c| c.as_bytes() == p)
        };
        let mut blobs: Vec<(String, &Entry)> = Vec::new();
        for e in &entries {
            match &e.kind {
                Kind::Blob(id) if note(&e.path) => blobs.push((id.clone(), e)),
                Kind::Blob(_) => write(dest, &e.path, b"")?,
                Kind::Link(id) => blobs.push((id.clone(), e)),
                Kind::Submodule => copy_tree(&self.top, dest, &e.path)?,
                Kind::Disk => copy_from_disk(&self.top, dest, &e.path, note(&e.path))?,
            }
        }
        let mut targets: Vec<String> = Vec::new();
        // Work-tree symlinks are written already; their targets count too.
        for e in entries.iter().filter(|e| matches!(e.kind, Kind::Disk)) {
            if let Ok(t) = std::fs::read_link(self.top.join(os(&e.path))) {
                targets.extend(link_target(&e.path, t.to_string_lossy().as_bytes()));
            }
        }
        if blobs.is_empty() {
            return Ok(targets);
        }
        let ids: Vec<String> = blobs.iter().map(|(id, _)| id.clone()).collect();
        self.read_blobs(ids, |k, body| {
            let e = blobs[k].1;
            match e.kind {
                Kind::Link(_) => {
                    targets.extend(link_target(&e.path, body));
                    link(dest, &e.path, body)
                }
                _ => write(dest, &e.path, body),
            }
        })?;
        Ok(targets)
    }

    /// Files, symlinks, and submodules of `tree` at or under `paths`.
    fn tree_entries(&self, tree: &str, paths: &[String]) -> Result<Vec<Entry>, Error> {
        let mut c = self.git();
        c.args(["ls-tree", "-r", "-z", "--full-tree", tree, "--"]);
        // An empty path is the whole tree.
        if !paths.iter().any(String::is_empty) {
            c.args(paths);
        }
        let listing = run(c, "listing the vault's files")?;
        let mut out: Vec<Entry> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let (meta, path) = (String::from_utf8_lossy(&entry[..tab]), &entry[tab + 1..]);
            // mode type id
            let fields: Vec<&str> = meta.split_whitespace().collect();
            let (Some(mode), Some(id)) = (fields.first(), fields.get(2)) else {
                continue;
            };
            let kind = match *mode {
                "100644" | "100755" => Kind::Blob((*id).to_string()),
                "120000" => Kind::Link((*id).to_string()),
                "160000" => Kind::Submodule,
                _ => continue,
            };
            if safe(path) && seen.insert(path.to_vec()) {
                out.push(Entry {
                    path: path.to_vec(),
                    kind,
                });
            }
        }
        // A well-formed tree has no entry inside another (a file or link `x` and `x/y`); a
        // crafted one can, to write through the link. Drop such entries.
        let leaves: std::collections::HashSet<Vec<u8>> =
            out.iter().map(|e| e.path.clone()).collect();
        out.retain(|e| {
            !e.path
                .iter()
                .enumerate()
                .any(|(i, &b)| b == b'/' && leaves.contains(&e.path[..i]))
        });
        Ok(out)
    }

    /// Tracked files, and new files git does not ignore, at or under `paths`.
    fn work_tree_entries(&self, paths: &[String]) -> Result<Vec<Entry>, Error> {
        let mut c = self.git();
        c.args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
        ]);
        // An empty path is the whole tree.
        if paths.is_empty() || paths.iter().any(String::is_empty) {
            c.arg(".");
        } else {
            c.args(paths);
        }
        let listing = run(c, "listing the vault's files")?;
        let mut paths: Vec<Vec<u8>> = listing
            .split(|&b| b == 0)
            .filter(|e| !e.is_empty() && safe(e))
            .map(<[u8]>::to_vec)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths
            .into_iter()
            .map(|path| Entry {
                path,
                kind: Kind::Disk,
            })
            .collect())
    }

    /// Read blobs with one `cat-file --batch`, in order, handing each to `each`. Its input is
    /// written from another thread so a large vault cannot fill both pipes; on an error git
    /// is stopped before that thread is joined.
    fn read_blobs(
        &self,
        ids: Vec<String>,
        mut each: impl FnMut(usize, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut child = self
            .git()
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Usage(format!("reading the vault's notes: cannot run git: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let n = ids.len();
        let writer = std::thread::spawn(move || {
            for id in ids {
                if writeln!(stdin, "{id}").is_err() {
                    break;
                }
            }
        });
        let mut reader = BufReader::new(child.stdout.take().expect("piped"));
        let result = (|| {
            for k in 0..n {
                let mut header = String::new();
                reader
                    .read_line(&mut header)
                    .map_err(|e| Error::Usage(format!("reading the vault's notes: {e}")))?;
                let size: usize = header
                    .split_whitespace()
                    .nth(2)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| {
                        Error::Usage(format!("reading a note from git: `{}`", header.trim()))
                    })?;
                let mut body = vec![0; size + 1];
                reader
                    .read_exact(&mut body)
                    .map_err(|e| Error::Usage(format!("reading a note from git: {e}")))?;
                body.pop(); // the newline after each object
                each(k, &body)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
        }
        drop(reader);
        let _ = writer.join();
        let _ = child.wait();
        result
    }

    /// Files renamed between `HEAD` and `source`, as (old, new) paths from the top. For the
    /// work tree, renames git sees (tracked files) and files moved without git (`mv`: a file
    /// gone from `HEAD`'s paths whose exact content is now at a new path) both count.
    pub fn renames(&self, source: Source) -> Result<Vec<(String, String)>, Error> {
        if !self.has_head() {
            return Ok(Vec::new());
        }
        let mut c = self.git();
        match (source, self.tree(source)?) {
            (Source::Staged, Some(tree)) => {
                c.args([
                    "diff-tree",
                    "-r",
                    "-z",
                    "-M",
                    "--name-status",
                    "HEAD",
                    &tree,
                ]);
            }
            // The index against HEAD (staged renames); the work tree is not read, so no
            // clean filter runs. Moves not staged are found by content below.
            (Source::WorkTree, _) => {
                c.args([
                    "diff-index",
                    "--cached",
                    "-z",
                    "-M",
                    "--name-status",
                    "HEAD",
                ]);
            }
            _ => return Ok(Vec::new()),
        }
        let out = run(c, "finding renamed notes")?;
        let fields: Vec<String> = out
            .split(|&b| b == 0)
            .map(|f| String::from_utf8_lossy(f).to_string())
            .collect();
        let mut renames = Vec::new();
        let mut i = 0;
        while i < fields.len() {
            let status = &fields[i];
            if status.starts_with('R') || status.starts_with('C') {
                if status.starts_with('R')
                    && let (Some(old), Some(new)) = (fields.get(i + 1), fields.get(i + 2))
                {
                    renames.push((old.clone(), new.clone()));
                }
                i += 3;
            } else {
                i += 2;
            }
        }
        if source == Source::WorkTree {
            renames.extend(self.moved_without_git(&renames)?);
        }
        Ok(renames)
    }

    /// Notes gone from their `HEAD` path (not on disk) whose exact content is in a new,
    /// untracked note.
    fn moved_without_git(
        &self,
        known: &[(String, String)],
    ) -> Result<Vec<(String, String)>, Error> {
        let md = |p: &str| p.to_ascii_lowercase().ends_with(".md");
        let mut c = self.git();
        c.args(["ls-tree", "-r", "-z", "--full-tree", "HEAD"]);
        let listing = run(c, "listing the last commit")?;
        let mut gone: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            if md(path)
                && !known.iter().any(|(o, _)| o == path)
                && std::fs::symlink_metadata(self.top.join(path)).is_err()
                && let Some(id) = meta.split_whitespace().nth(2)
            {
                gone.insert(id.to_string(), path.to_string());
            }
        }
        if gone.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = self.git();
        c.args(["ls-files", "-z", "--others", "--exclude-standard"]);
        let new: Vec<String> = run(c, "listing new files")?
            .split(|&b| b == 0)
            .filter(|e| !e.is_empty())
            .map(|e| String::from_utf8_lossy(e).to_string())
            .filter(|p| md(p))
            // Regular files only: a symlink to a device or a FIFO would hang or stop git.
            .filter(|p| {
                std::fs::symlink_metadata(self.top.join(p)).is_ok_and(|m| m.file_type().is_file())
            })
            .collect();
        if new.is_empty() {
            return Ok(Vec::new());
        }
        // Content ids without filters: nothing in the repository's config runs.
        let mut child = self
            .git()
            .args(["hash-object", "--no-filters", "--stdin-paths"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| Error::Usage(format!("hashing new notes: cannot run git: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let paths = new.clone();
        let writer = std::thread::spawn(move || {
            for p in paths {
                if writeln!(stdin, "{p}").is_err() {
                    break;
                }
            }
        });
        let mut ids = String::new();
        let _ = child.stdout.take().expect("piped").read_to_string(&mut ids);
        let _ = writer.join();
        let _ = child.wait();
        Ok(new
            .into_iter()
            .zip(ids.lines())
            .filter_map(|(path, id)| gone.remove(id).map(|old| (old, path)))
            .collect())
    }
}

/// The path from the top a relative symlink at `path` (from the top) points to, if it stays
/// in the repository.
fn link_target(path: &[u8], target: &[u8]) -> Option<String> {
    let path = String::from_utf8_lossy(path);
    let target = String::from_utf8_lossy(target);
    if target.starts_with('/') {
        return None;
    }
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let joined = format!("{dir}/{target}");
    let mut parts: Vec<&str> = Vec::new();
    for p in joined.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// `path` is `prefix` or under it (paths from the top; `prefix` may end in `/`).
fn under(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// A path from git with only normal components (no `..`, no absolute path).
fn safe(path: &[u8]) -> bool {
    !path.is_empty()
        && Path::new(&os(path))
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Refuse a path in the copy whose parent folders include a symlink: writing there would
/// land outside `dest`.
fn inside(dest: &Path, path: &Path) -> Result<(), Error> {
    let rel = path
        .strip_prefix(dest)
        .map_err(|_| Error::Usage(format!("{} is outside the copy", path.display())))?;
    let mut at = dest.to_path_buf();
    let parts: Vec<_> = rel.components().collect();
    for c in parts.iter().take(parts.len().saturating_sub(1)) {
        at.push(c);
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Usage(format!(
                "{} is written through a symlink; the repository's tree is malformed",
                path.display()
            )));
        }
    }
    Ok(())
}

fn write(dest: &Path, rel: &[u8], body: &[u8]) -> Result<(), Error> {
    let path = dest.join(os(rel));
    inside(dest, &path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    std::fs::write(&path, body).map_err(|e| Error::io(&path, e))
}

/// A symlink at `rel` with target text `target`, as git stores it.
fn link(dest: &Path, rel: &[u8], target: &[u8]) -> Result<(), Error> {
    let path = dest.join(os(rel));
    inside(dest, &path)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(os(target), &path).map_err(|e| Error::io(&path, e))
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        std::fs::write(&path, b"").map_err(|e| Error::io(&path, e))
    }
}

/// Copy a work-tree file (content when `content`, else empty) or symlink into `dest`. A file
/// that is gone (deleted, not yet staged) is skipped.
fn copy_from_disk(top: &Path, dest: &Path, rel: &[u8], content: bool) -> Result<(), Error> {
    let src = top.join(os(rel));
    let Ok(meta) = std::fs::symlink_metadata(&src) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&src).map_err(|e| Error::io(&src, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            return link(dest, rel, target.as_os_str().as_bytes());
        }
        #[cfg(not(unix))]
        {
            let _ = target;
            return write(dest, rel, b"");
        }
    }
    if meta.is_dir() {
        // A submodule's work tree (git lists it as one entry).
        return copy_tree(top, dest, rel);
    }
    if !meta.is_file() {
        return Ok(());
    }
    if content {
        let body = std::fs::read(&src).map_err(|e| Error::io(&src, e))?;
        write(dest, rel, &body)
    } else {
        write(dest, rel, b"")
    }
}

/// Copy a submodule's work tree from disk (notes with content, other files empty, no `.git`).
fn copy_tree(top: &Path, dest: &Path, rel: &[u8]) -> Result<(), Error> {
    let root = top.join(os(rel));
    if !root.is_dir() {
        return Ok(());
    }
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(inner) = path.strip_prefix(top) else {
                continue;
            };
            let out = dest.join(inner);
            inside(dest, &out)?;
            if let Some(d) = out.parent() {
                std::fs::create_dir_all(d).map_err(|e| Error::io(d, e))?;
            }
            let is_note = path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("md"));
            if ft.is_file() && is_note {
                std::fs::copy(&path, &out).map_err(|e| Error::io(&path, e))?;
            } else if ft.is_file() {
                std::fs::write(&out, b"").map_err(|e| Error::io(&out, e))?;
            }
        }
    }
    Ok(())
}

/// What happened in a repository since a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// `dir` is not a git repository (or git is missing).
    NoRepo,
    /// The commit is not in the repository (a typo, or history was rewritten).
    UnknownCommit,
    /// Commits on `HEAD` since the commit, and `HEAD`'s short id and date.
    Since {
        commits: u64,
        head: String,
        date: String,
    },
}

/// Commits on `dir`'s `HEAD` since `commit` (a hex commit id).
#[must_use]
pub fn drift(dir: &Path, commit: &str) -> Drift {
    let ok = |args: &[&str]| -> Option<String> {
        let out = other(dir).args(args).stderr(Stdio::null()).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if ok(&["rev-parse", "--git-dir"]).is_none() {
        return Drift::NoRepo;
    }
    let Some(full) = ok(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{commit}^{{commit}}"),
    ]) else {
        return Drift::UnknownCommit;
    };
    let Some(count) = ok(&["rev-list", "--count", &format!("{full}..HEAD")]) else {
        return Drift::UnknownCommit;
    };
    let head = ok(&["rev-parse", "--short", "HEAD"]).unwrap_or_default();
    let date =
        ok(&["show", "-s", "--no-show-signature", "--format=%cs", "HEAD"]).unwrap_or_default();
    Drift::Since {
        commits: count.parse().unwrap_or(0),
        head,
        date,
    }
}
