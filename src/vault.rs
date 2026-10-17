//! Load a vault and resolve links the way Obsidian does (as far as its docs say).

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use crate::Error;
use crate::config::Config;
use crate::parse::{self, LinkKind, Parsed, RawLink};

/// One Markdown note.
#[derive(Debug, Clone)]
pub struct Note {
    /// Vault-relative path without `.md`, `/`-separated. Unique.
    pub id: String,
    pub path: PathBuf,
    pub text: String,
    /// The file was not valid UTF-8; `text` has replacement characters.
    pub not_utf8: bool,
    pub parsed: Parsed,
    /// Byte offsets where each line starts, for line numbers.
    line_starts: Vec<usize>,
}

impl Note {
    /// 1-based line number of a byte offset.
    #[must_use]
    pub fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= offset)
    }

    /// Byte offset where 1-based `line` starts (the end of the text past the last line).
    #[must_use]
    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or(self.text.len())
    }

    /// (1-based line number, the whole line, byte offset of `offset` within it).
    #[must_use]
    pub fn line_at(&self, offset: usize) -> (usize, &str, usize) {
        let line = self.line_of(offset);
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .map_or(self.text.len(), |e| e - 1);
        let end = end.max(start);
        (line, &self.text[start..end], offset.saturating_sub(start))
    }

    /// The text of the line containing `offset`, trimmed.
    #[must_use]
    pub fn line_text(&self, offset: usize) -> &str {
        let line = self.line_of(offset);
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .map_or(self.text.len(), |e| e - 1);
        self.text[start..end.max(start)].trim()
    }

    /// Frontmatter and inline tags, normalized and deduplicated.
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut tags = self.parsed.frontmatter.tags();
        for t in &self.parsed.tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        tags
    }

    /// File stem, e.g. `overview` for `Projects/gatehouse/overview`.
    #[must_use]
    pub fn stem(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }

    /// The word that names this note in prose: the stem, or the parent folder
    /// name when the stem is generic (`Projects/gatehouse/overview` -> `gatehouse`).
    #[must_use]
    pub fn name(&self, generic: &[String]) -> &str {
        let stem = self.stem();
        if generic.iter().any(|g| g.eq_ignore_ascii_case(stem)) {
            let mut parts = self.id.rsplit('/');
            parts.next();
            if let Some(parent) = parts.next() {
                return parent;
            }
        }
        stem
    }
}

/// How a link named its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// Full vault-relative path.
    FullPath,
    /// `./` or `../` relative to the linking note.
    Relative,
    /// Unique file name or path suffix.
    Short,
    /// Several notes share the name; resolved best-effort.
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Index into [`Vault::notes`].
    Note(usize),
    /// A non-Markdown file (attachment), vault-relative.
    File(String),
    Unresolved,
    /// `[[#Heading]]`: a link into the same note.
    SelfLink,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    pub raw: RawLink,
    pub target: Target,
    pub form: Form,
}

#[derive(Debug)]
pub struct Vault {
    pub config: Config,
    pub notes: Vec<Note>,
    by_id: HashMap<String, usize>,
    by_id_lower: HashMap<String, Vec<usize>>,
    files: BTreeSet<String>,
    files_lower: HashMap<String, Vec<String>>,
    /// Lowercase file stem -> notes, for short-link lookup.
    by_stem: HashMap<String, Vec<usize>>,
    /// Lowercase file name -> attachments.
    files_by_name: HashMap<String, Vec<String>>,
    /// Outbound note links per note, computed once on first use.
    outbound: std::sync::OnceLock<Vec<BTreeSet<usize>>>,
}

fn last_segment(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn rel_id(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect()
}

impl Vault {
    pub fn load(config: Config) -> Result<Self, Error> {
        let root = config.vault.clone();
        let mut md: Vec<(String, PathBuf)> = Vec::new();
        let mut files = BTreeSet::new();
        // Follows symlinks (as Obsidian does); the walker detects loops. A symlink whose target
        // is outside the vault is skipped, so a link to `/` or `~` cannot make every hook walk it.
        let inside_root = {
            let root = root.clone();
            move |p: &Path| p.canonicalize().is_ok_and(|c| c.starts_with(&root))
        };
        let filter_root = inside_root.clone();
        let walker = ignore::WalkBuilder::new(&root)
            .standard_filters(false)
            .hidden(true)
            .follow_links(true)
            .sort_by_file_name(std::cmp::Ord::cmp)
            .filter_entry(move |e| !e.path_is_symlink() || filter_root(e.path()))
            .build();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    // One unreadable entry must not stop the whole vault (or the hooks).
                    eprintln!("wikiknit: skipping: {e}");
                    continue;
                }
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let path = entry.path();
            let Some(rel) = rel_id(&root, path) else {
                continue;
            };
            let Ok(real) = path.canonicalize() else {
                continue;
            };
            if !real.starts_with(&root) {
                continue;
            }
            let is_md = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"));
            // A second path to a note already in the vault (an in-vault symlink): the real
            // path is the note; this one resolves as a file. Ignored notes are not checked,
            // but links to them still resolve (as files).
            let via_symlink = real != path && real.starts_with(&root);
            if !is_md || via_symlink || config.is_ignored(&rel) {
                files.insert(rel);
                continue;
            }
            md.push((rel, path.to_path_buf()));
        }
        // Read and parse in parallel; keep the walk's (sorted) order.
        use rayon::prelude::*;
        let notes: Vec<Note> = md
            .into_par_iter()
            .filter_map(|(rel, path)| {
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("wikiknit: skipping {}: {e}", path.display());
                        return None;
                    }
                };
                let (text, not_utf8) = match String::from_utf8(bytes) {
                    Ok(s) => (s, false),
                    Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
                };
                Some(Note {
                    id: rel[..rel.len() - 3].to_string(),
                    path,
                    not_utf8,
                    parsed: parse::parse(&text),
                    line_starts: line_starts(&text),
                    text,
                })
            })
            .collect();
        let mut by_id = HashMap::new();
        let mut by_id_lower: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, n) in notes.iter().enumerate() {
            by_id.insert(n.id.clone(), i);
            by_id_lower.entry(n.id.to_lowercase()).or_default().push(i);
        }
        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, n) in notes.iter().enumerate() {
            by_stem.entry(n.stem().to_lowercase()).or_default().push(i);
        }
        let mut files_lower: HashMap<String, Vec<String>> = HashMap::new();
        let mut files_by_name: HashMap<String, Vec<String>> = HashMap::new();
        for f in &files {
            files_lower
                .entry(f.to_lowercase())
                .or_default()
                .push(f.clone());
            files_by_name
                .entry(last_segment(f).to_lowercase())
                .or_default()
                .push(f.clone());
        }
        Ok(Self {
            config,
            notes,
            by_id,
            by_id_lower,
            files,
            files_lower,
            by_stem,
            files_by_name,
            outbound: std::sync::OnceLock::new(),
        })
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Note> {
        self.by_id.get(id).map(|&i| &self.notes[i])
    }

    #[must_use]
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// Map a CLI argument (note id, vault-relative path, or file path) to a note id.
    #[must_use]
    pub fn id_for_arg(&self, arg: &str, cwd: &Path) -> Option<String> {
        // An existing file path wins over a note id of the same spelling.
        if let Some(rel) = self.rel_for_path(arg, cwd) {
            let id = rel.strip_suffix(".md").unwrap_or(&rel).to_string();
            return self.by_id.contains_key(&id).then_some(id);
        }
        let trimmed = arg.trim_end_matches(".md");
        self.by_id
            .contains_key(trimmed)
            .then(|| trimmed.to_string())
    }

    /// Find a note from a reference an agent would write: an id, a file path, a link target
    /// (`gatehouse/overview`, `acme`), or a name or alias (`gatehouse`, `ACME Corp`),
    /// case-insensitive. Ambiguous or unknown references are an error that lists candidates.
    pub fn find_note(&self, reference: &str) -> Result<usize, Error> {
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Some(id) = self.id_for_arg(reference, &cwd) {
            return Ok(self.by_id[&id]);
        }
        let r = reference.trim().trim_end_matches(".md");
        if let Some(Target::Note(i)) = self.lookup_exact(r) {
            return Ok(i);
        }
        if let (Target::Note(i), Form::Short) = self.lookup_suffix(r, "") {
            return Ok(i);
        }
        // A folder whose note has a generic stem: `Projects/x` -> `Projects/x/overview`.
        for g in &self.config.generic_names {
            let folder = r.trim_end_matches('/');
            if let Some(&i) = self
                .by_id_lower
                .get(&format!("{}/{g}", folder.to_lowercase()))
                .and_then(|v| (v.len() == 1).then_some(&v[0]))
            {
                return Ok(i);
            }
        }
        let want = r.to_lowercase();
        let generic = &self.config.generic_names;
        let named: Vec<usize> = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, n)| {
                n.name(generic).to_lowercase() == want
                    || n.parsed
                        .frontmatter
                        .aliases()
                        .iter()
                        .any(|a| a.to_lowercase() == want)
            })
            .map(|(i, _)| i)
            .collect();
        if named.len() == 1 {
            return Ok(named[0]);
        }
        let candidates: Vec<&str> = if named.is_empty() {
            self.notes
                .iter()
                .filter(|n| n.id.to_lowercase().contains(&want))
                .map(|n| n.id.as_str())
                .take(8)
                .collect()
        } else {
            named.iter().map(|&i| self.notes[i].id.as_str()).collect()
        };
        Err(Error::Usage(
            match (named.is_empty(), candidates.is_empty()) {
                (true, true) => format!("no note matches `{reference}`"),
                (true, false) => format!(
                    "no note matches `{reference}`; ids containing it: {}",
                    candidates.join(", ")
                ),
                (false, _) => format!(
                    "`{reference}` matches several notes: {}",
                    candidates.join(", ")
                ),
            },
        ))
    }

    /// A CLI argument names a file in an ignored folder.
    #[must_use]
    pub fn is_ignored_arg(&self, arg: &str, cwd: &Path) -> bool {
        let rel = self
            .rel_for_path(arg, cwd)
            .unwrap_or_else(|| arg.to_string());
        self.config.is_ignored(&rel)
    }

    /// Vault-relative path of an existing file argument.
    fn rel_for_path(&self, arg: &str, cwd: &Path) -> Option<String> {
        let p = Path::new(arg);
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        let abs = abs.canonicalize().ok().filter(|a| a.is_file())?;
        rel_id(&self.config.vault, &abs)
    }

    /// Resolve every link of a note (body and frontmatter).
    #[must_use]
    pub fn resolve_links(&self, note: &Note) -> Vec<Resolved> {
        note.parsed
            .links
            .iter()
            .map(|raw| {
                let (target, form) = self.resolve(note, raw);
                Resolved {
                    raw: raw.clone(),
                    target,
                    form,
                }
            })
            .collect()
    }

    /// Outbound note links of every note (resolved, excluding self-links), computed once.
    #[must_use]
    pub fn outbound_all(&self) -> &[BTreeSet<usize>] {
        self.outbound.get_or_init(|| {
            use rayon::prelude::*;
            self.notes
                .par_iter()
                .enumerate()
                .map(|(me, note)| {
                    self.resolve_links(note)
                        .into_iter()
                        .filter_map(|r| match r.target {
                            Target::Note(i) if i != me => Some(i),
                            _ => None,
                        })
                        .collect()
                })
                .collect()
        })
    }

    /// Indexes of notes this note links to (resolved, excluding itself).
    #[must_use]
    pub fn outbound(&self, note: &Note) -> BTreeSet<usize> {
        self.index_of(&note.id)
            .map(|i| self.outbound_all()[i].clone())
            .unwrap_or_default()
    }

    /// How many non-hub notes link to each note.
    #[must_use]
    pub fn inbound_counts(&self) -> Vec<usize> {
        let mut counts = vec![0usize; self.notes.len()];
        for (i, out) in self.outbound_all().iter().enumerate() {
            if self.config.is_hub(&self.notes[i].id) {
                continue;
            }
            for &j in out {
                counts[j] += 1;
            }
        }
        counts
    }

    fn resolve(&self, from: &Note, raw: &RawLink) -> (Target, Form) {
        let t = raw.target.trim().trim_start_matches('/');
        if t.is_empty() {
            return (Target::SelfLink, Form::FullPath);
        }
        let from_dir = from.id.rsplit_once('/').map_or("", |(d, _)| d);
        if raw.kind == LinkKind::Markdown {
            // Markdown links are paths: relative to the note first, then the vault root.
            let rel = normalize(&join(from_dir, t));
            if let Some(found) = rel.as_deref().and_then(|r| self.lookup_exact(r)) {
                return (found, Form::Relative);
            }
            return match self.lookup_exact(t) {
                Some(found) => (found, Form::FullPath),
                None => (Target::Unresolved, Form::FullPath),
            };
        }
        if t.starts_with("./") || t.starts_with("../") {
            return match normalize(&join(from_dir, t))
                .as_deref()
                .and_then(|r| self.lookup_exact(r))
            {
                Some(found) => (found, Form::Relative),
                None => (Target::Unresolved, Form::Relative),
            };
        }
        if let Some(found) = self.lookup_exact(t) {
            return (found, Form::FullPath);
        }
        self.lookup_suffix(t, from_dir)
    }

    /// Exact vault-relative path, with or without `.md`; case-insensitive fallback.
    fn lookup_exact(&self, t: &str) -> Option<Target> {
        let note_id = t.strip_suffix(".md").unwrap_or(t);
        if let Some(&i) = self.by_id.get(note_id) {
            return Some(Target::Note(i));
        }
        if let Some(v) = self.by_id_lower.get(&note_id.to_lowercase())
            && v.len() == 1
        {
            return Some(Target::Note(v[0]));
        }
        if self.files.contains(t) {
            return Some(Target::File(t.to_string()));
        }
        let as_md = format!("{note_id}.md");
        if self.files.contains(&as_md) {
            return Some(Target::File(as_md));
        }
        if let Some(v) = self.files_lower.get(&t.to_lowercase())
            && v.len() == 1
        {
            return Some(Target::File(v[0].clone()));
        }
        None
    }

    /// Obsidian's short form: the target matches the end of a path.
    fn lookup_suffix(&self, t: &str, from_dir: &str) -> (Target, Form) {
        let needle = t.strip_suffix(".md").unwrap_or(t).to_lowercase();
        let suffix = format!("/{needle}");
        let notes: Vec<usize> = self
            .by_stem
            .get(last_segment(&needle))
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| {
                let id = self.notes[i].id.to_lowercase();
                id == needle || id.ends_with(&suffix)
            })
            .collect();
        match notes.len() {
            1 => return (Target::Note(notes[0]), Form::Short),
            0 => {}
            _ => {
                // Prefer a note in the linking note's folder, then the shortest path.
                let best = notes
                    .iter()
                    .copied()
                    .min_by_key(|&i| {
                        let id = &self.notes[i].id;
                        let same_dir = id.rsplit_once('/').map_or("", |(d, _)| d) == from_dir;
                        (!same_dir, id.len(), id.clone())
                    })
                    .expect("non-empty");
                return (Target::Note(best), Form::Ambiguous);
            }
        }
        let fneedle = t.to_lowercase();
        let fsuffix = format!("/{fneedle}");
        let md_needle = format!("{needle}.md");
        let md_suffix = format!("/{md_needle}");
        let files: Vec<&String> = [last_segment(&fneedle), last_segment(&md_needle)]
            .into_iter()
            .filter_map(|name| self.files_by_name.get(name))
            .flatten()
            .filter(|f| {
                let f = f.to_lowercase();
                f == fneedle || f.ends_with(&fsuffix) || f == md_needle || f.ends_with(&md_suffix)
            })
            .collect();
        let mut files = files;
        files.sort();
        files.dedup();
        match files.len() {
            0 => (Target::Unresolved, Form::Short),
            1 => (Target::File(files[0].clone()), Form::Short),
            _ => (Target::File(files[0].clone()), Form::Ambiguous),
        }
    }
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Collapse `.` and `..`; `None` if it climbs out of the vault.
fn normalize(p: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for c in Path::new(p).components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop()?;
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    Some(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_paths() {
        assert_eq!(normalize("a/./b/../c").as_deref(), Some("a/c"));
        assert_eq!(normalize("../x"), None);
    }
}
