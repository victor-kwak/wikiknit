//! The instructions an agent needs to work in a checked vault, for the project's own instruction
//! file (`AGENTS.md`, `CLAUDE.md`, ...), between two marker lines so an upgrade replaces them in
//! place.

use std::path::Path;

use crate::Error;

pub const START: &str = "<!-- wikiknit:start -->";
pub const END: &str = "<!-- wikiknit:end -->";

/// The block for this version of wikiknit, markers included, ending with a newline.
#[must_use]
pub fn text() -> String {
    include_str!("snippet.md").replace("{version}", env!("CARGO_PKG_VERSION"))
}

/// What `write_into` did to the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The file did not exist; it holds the block now.
    Created,
    /// The file had no block; the block is at its end now.
    Appended,
    /// The block between the markers was replaced.
    Replaced,
    /// The block was already this version's.
    Unchanged,
}

/// For each line, whether it is inside a fenced code block (or is its fence): a marker shown as
/// an example in a code block is not the block.
fn in_fence(lines: &[&str]) -> Vec<bool> {
    let mut open: Option<(char, usize)> = None;
    lines
        .iter()
        .map(|line| {
            let t = line.trim_start_matches('\u{feff}').trim_start();
            let fence = ['`', '~'].into_iter().find_map(|c| {
                let n = t.chars().take_while(|&x| x == c).count();
                (n >= 3).then_some((c, n))
            });
            match (open, fence) {
                (None, Some(f)) => {
                    open = Some(f);
                    true
                }
                (Some((c, n)), Some((c2, n2)))
                    if c == c2 && n2 >= n && t.trim_end().chars().all(|x| x == c) =>
                {
                    open = None;
                    true
                }
                (o, _) => o.is_some(),
            }
        })
        .collect()
}

/// `existing` with `block` in place of the lines from the start marker to the end marker, or
/// at its end when it has no markers (and whether it replaced a block). A marker is a line that holds only the marker. Markers
/// that do not make one block (one without the other, an end before the start, two starts) are
/// an error: replacing would guess which lines are the old block.
pub fn splice(existing: &str, block: &str) -> Result<(String, bool), String> {
    let crlf = existing.contains("\r\n");
    let block = if crlf {
        block.replace('\n', "\r\n")
    } else {
        block.to_string()
    };
    let eol = if crlf { "\r\n" } else { "\n" };
    // Each line with its end, so the text around the block stays byte for byte.
    let lines: Vec<&str> = existing.split_inclusive('\n').collect();
    let fenced = in_fence(&lines);
    let is = |i: usize, marker: &str| {
        !fenced[i] && lines[i].trim_start_matches('\u{feff}').trim() == marker
    };
    let starts: Vec<usize> = (0..lines.len()).filter(|&i| is(i, START)).collect();
    let ends: Vec<usize> = (0..lines.len()).filter(|&i| is(i, END)).collect();
    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => {
            let mut out = existing.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(eol);
                }
                if !out.ends_with(&format!("{eol}{eol}")) {
                    out.push_str(eol);
                }
            }
            out.push_str(&block);
            Ok((out, false))
        }
        ([s], [e]) if s < e => {
            let mut out: String = lines[..*s].concat();
            // A byte order mark before the start marker stays.
            if *s == 0 && existing.starts_with('\u{feff}') {
                out.push('\u{feff}');
            }
            out.push_str(&block);
            // The end marker's own line ending came with the block.
            out.push_str(&lines[e + 1..].concat());
            Ok((out, true))
        }
        _ => Err(format!(
            "the {START} and {END} lines do not make one block ({} start, {} end); fix them by \
             hand, then run this again",
            starts.len(),
            ends.len()
        )),
    }
}

/// Put this version's block into the instruction file at `path` (created if missing).
pub fn write_into(path: &Path) -> Result<Outcome, Error> {
    let block = text();
    let existing = match std::fs::read(path) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| Error::Config {
            path: path.to_path_buf(),
            message: "not UTF-8 text; not changed".into(),
        })?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(Error::io(path, e)),
    };
    let (new, replaced) = match &existing {
        None => (block, false),
        Some(text) => splice(text, &block).map_err(|message| Error::Config {
            path: path.to_path_buf(),
            message,
        })?,
    };
    if existing.as_ref() == Some(&new) {
        return Ok(Outcome::Unchanged);
    }
    replace(path, &new)?;
    Ok(match (existing, replaced) {
        (None, _) => Outcome::Created,
        (Some(_), true) => Outcome::Replaced,
        (Some(_), false) => Outcome::Appended,
    })
}

/// Write `text` to `path` through a new file beside it, renamed over it: a crash leaves the old
/// file or the new one, never half of one. A symlinked file is written at its target (the link
/// stays), with the target's permissions.
fn replace(path: &Path, text: &str) -> Result<(), Error> {
    let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let dir = match real.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let mut tmp = tempfile::Builder::new()
        .prefix(".wikiknit-")
        .tempfile_in(&dir)
        .map_err(|e| Error::io(&dir, e))?;
    use std::io::Write as _;
    tmp.write_all(text.as_bytes())
        .map_err(|e| Error::io(tmp.path(), e))?;
    let perms = match std::fs::metadata(&real) {
        Ok(m) => m.permissions(),
        Err(_) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::Permissions::from_mode(0o644)
            }
            #[cfg(not(unix))]
            {
                tmp.as_file()
                    .metadata()
                    .map_err(|e| Error::io(tmp.path(), e))?
                    .permissions()
            }
        }
    };
    std::fs::set_permissions(tmp.path(), perms).map_err(|e| Error::io(tmp.path(), e))?;
    tmp.persist(&real).map_err(|e| Error::io(&real, e.error))?;
    Ok(())
}
