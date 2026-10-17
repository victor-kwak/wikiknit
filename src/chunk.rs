//! Split notes into chunks for embedding: by section, then by paragraphs up to a size limit.
//! Each chunk carries its note title and heading path, so it embeds with its context.

use crate::catalog;
use crate::text::{self, Fence};
use crate::vault::Note;

/// Target chunk size in characters (about 175 tokens of English); a chunk ends at the next
/// paragraph break past this, or at twice this in one paragraph.
const CHUNK_CHARS: usize = 700;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// 1-based file line of the chunk's first content line.
    pub line: usize,
    /// Text to embed: `title > heading path` then the content.
    pub text: String,
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let l = line.trim_start();
    let level = l.chars().take_while(|c| *c == '#').count();
    (level > 0 && level <= 6 && l[level..].starts_with(' ')).then(|| (level, l[level..].trim()))
}

/// `s` in pieces of at most `max` characters, cut at a space when one is near the end.
fn split_long(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while rest.chars().count() > max {
        let cut = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let at = rest[..cut]
            .rfind(' ')
            .filter(|&i| i > cut / 2)
            .unwrap_or(cut);
        out.push(&rest[..at]);
        rest = rest[at..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest);
    }
    out
}

/// The note's chunks, in order. A note with no body text still gets one chunk (its title,
/// aliases, and summary), so every note can be found.
#[must_use]
pub fn chunks(note: &Note, generic: &[String]) -> Vec<Chunk> {
    let title = catalog::title(note, generic).unwrap_or_else(|| note.name(generic).to_string());
    let aliases = note.parsed.frontmatter.aliases();
    let header_base = if aliases.is_empty() {
        title.clone()
    } else {
        format!("{title} ({})", aliases.join(", "))
    };
    let first_line = note.line_of(note.parsed.body_start);
    let body = &note.text[note.parsed.body_start..];

    let mut out = Vec::new();
    let mut path: Vec<(usize, String)> = Vec::new();
    let mut buf = String::new();
    let mut buf_chars = 0usize;
    let mut buf_line = 0usize;
    let mut fence = Fence::default();
    let flush =
        |buf: &mut String, buf_line: usize, path: &[(usize, String)], out: &mut Vec<Chunk>| {
            if buf.trim().is_empty() {
                buf.clear();
                return;
            }
            let mut header = header_base.clone();
            for (_, h) in path {
                header.push_str(" > ");
                header.push_str(h);
            }
            out.push(Chunk {
                line: buf_line,
                text: format!("{header}\n{}", buf.trim()),
            });
            buf.clear();
        };
    for (k, line) in body.lines().enumerate() {
        let file_line = first_line + k;
        let in_code = fence.line(line);
        if !in_code && let Some((level, h)) = heading(line) {
            flush(&mut buf, buf_line, &path, &mut out);
            buf_chars = 0;
            if level == 1 {
                path.clear();
            } else {
                path.retain(|(l, _)| *l < level);
                path.push((level, text::links_to_text(h, generic)));
            }
            continue;
        }
        let content = text::links_to_text(line, generic);
        // A blank line ends a paragraph: a good place to cut a full chunk.
        if content.trim().is_empty() {
            if buf_chars >= CHUNK_CHARS {
                flush(&mut buf, buf_line, &path, &mut out);
                buf_chars = 0;
            } else if !buf.is_empty() {
                buf.push('\n');
            }
            continue;
        }
        // Sizes in characters, so CJK text is not cut three times too early. A line longer
        // than a whole chunk (minified text, a long table row) is split, since the model
        // truncates anything past its window.
        for piece in split_long(&content, CHUNK_CHARS * 2) {
            let piece_chars = piece.chars().count();
            if buf_chars + piece_chars > CHUNK_CHARS * 2 {
                flush(&mut buf, buf_line, &path, &mut out);
                buf_chars = 0;
            }
            if buf.is_empty() {
                buf_line = file_line;
            }
            buf.push_str(piece);
            buf.push('\n');
            buf_chars += piece_chars + 1;
        }
    }
    flush(&mut buf, buf_line, &path, &mut out);
    if out.is_empty() {
        let summary = catalog::summary(note, generic);
        out.push(Chunk {
            line: first_line,
            text: format!("{header_base}\n{summary}"),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::vault::Vault;

    fn note(text: &str) -> Vault {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("n.md"), text).unwrap();
        let v = Vault::load(Config::for_vault(dir.path().canonicalize().unwrap())).unwrap();
        std::mem::forget(dir);
        v
    }

    #[test]
    fn sections_become_chunks_with_context() {
        let v = note(
            "---\naliases: [Nee]\n---\n# Note title\nIntro.\n\n## Setup\nInstall [[x/y|it]].\n```\n# not a heading\n```\n### Sub\nDeep.\n",
        );
        let c = chunks(&v.notes[0], &[]);
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!(c[0].text, "Note title (Nee)\nIntro.");
        assert_eq!(c[0].line, 5);
        assert!(
            c[1].text
                .starts_with("Note title (Nee) > Setup\nInstall it."),
            "{c:?}"
        );
        assert!(c[1].text.contains("# not a heading"));
        assert_eq!(c[2].text, "Note title (Nee) > Setup > Sub\nDeep.");
    }

    #[test]
    fn long_sections_split_at_paragraphs() {
        let para = "word ".repeat(250);
        let v = note(&format!("# T\n{para}\n\n{para}\n\n{para}\n"));
        let c = chunks(&v.notes[0], &[]);
        assert!(c.len() >= 2, "{}", c.len());
        assert!(c.iter().all(|x| x.text.len() < 2 * CHUNK_CHARS + 200));
    }

    #[test]
    fn huge_lines_and_cjk_are_split_by_characters() {
        let long = "word ".repeat(2000);
        let v = note(&format!("# T\n{long}\n"));
        let c = chunks(&v.notes[0], &[]);
        assert!(c.len() >= 4, "{}", c.len());
        assert!(
            c.iter()
                .all(|x| x.text.chars().count() <= 2 * CHUNK_CHARS + 50)
        );
        // 1,000 CJK characters are 3,000 bytes but fit in one chunk's character budget.
        let cjk = "東".repeat(1000);
        let v = note(&format!("# T\n{cjk}\n"));
        assert_eq!(chunks(&v.notes[0], &[]).len(), 1);
    }

    #[test]
    fn empty_body_still_has_a_chunk() {
        let v = note("---\ntype: x\n---\n");
        assert_eq!(chunks(&v.notes[0], &[]).len(), 1);
    }
}
