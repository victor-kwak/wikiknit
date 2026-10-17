//! Split a note into frontmatter and body, and find its links.

use std::ops::Range;
use std::sync::LazyLock;

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use regex::Regex;
use serde_json::Value;

static WIKILINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[\[([^\[\]\n]+)\]\]").expect("valid regex"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// `[[target]]`
    Wiki,
    /// `![[target]]`
    Embed,
    /// `[text](target.md)`
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Body,
    /// Inside a frontmatter value; `key` is the top-level property.
    Frontmatter {
        key: String,
    },
}

/// A link as written, before resolution.
#[derive(Debug, Clone)]
pub struct RawLink {
    pub kind: LinkKind,
    /// Target path or name, without `#subpath` and `|alias`.
    pub target: String,
    /// Byte range of the whole link in the note text.
    pub span: Range<usize>,
    pub location: Location,
}

/// Parsed frontmatter block.
#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    /// Byte range of the YAML text (between the `---` fences) in the note.
    pub yaml: Option<Range<usize>>,
    pub value: Option<Value>,
    pub error: Option<String>,
}

impl Frontmatter {
    #[must_use]
    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.value.as_ref()?.get(key)?.as_str()
    }

    /// `aliases` (or `alias`) as a list of strings; a single string is accepted.
    #[must_use]
    pub fn aliases(&self) -> Vec<String> {
        let Some(v) = self.value.as_ref() else {
            return Vec::new();
        };
        let raw = v.get("aliases").or_else(|| v.get("alias"));
        match raw {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Everything wikiknit needs from one note's text.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub frontmatter: Frontmatter,
    /// Byte offset where the body starts.
    pub body_start: usize,
    pub links: Vec<RawLink>,
    /// Wikilinks found inside code spans or blocks (Obsidian ignores these).
    pub code_links: Vec<RawLink>,
    /// Byte ranges of code spans and blocks.
    pub code: Vec<Range<usize>>,
    /// Wikilinks inside raw HTML (not rendered as links).
    pub html_links: Vec<RawLink>,
    /// Byte ranges that are not prose for mention matching: raw HTML and external links.
    pub non_prose: Vec<Range<usize>>,
    /// Frontmatter keys holding `[[link]]` that YAML read as nested lists (unquoted).
    pub unquoted_frontmatter_links: Vec<String>,
}

#[must_use]
pub fn parse(text: &str) -> Parsed {
    let mut out = Parsed::default();
    // A UTF-8 byte order mark must not hide the frontmatter.
    let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
    let (yaml, body_start) = split_frontmatter(&text[bom..]);
    let yaml = yaml.map(|r| r.start + bom..r.end + bom);
    let body_start = body_start + bom;
    out.body_start = body_start;
    if let Some(range) = yaml {
        let src = &text[range.clone()];
        out.frontmatter.yaml = Some(range.clone());
        if src.trim().is_empty() {
            out.frontmatter.value = Some(Value::Object(serde_json::Map::new()));
        } else {
            match serde_saphyr::from_str::<Value>(src) {
                Ok(v) => {
                    collect_frontmatter_links(&v, text, &range, &mut out);
                    out.frontmatter.value = Some(v);
                }
                Err(e) => out.frontmatter.error = Some(first_line(&e.to_string())),
            }
        }
    }
    parse_body(text, body_start, &mut out);
    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

/// Returns the YAML byte range (if the note opens with `---`) and the body start.
fn split_frontmatter(text: &str) -> (Option<Range<usize>>, usize) {
    let first_end = match text.find('\n') {
        Some(i) => i,
        None => return (None, 0),
    };
    if text[..first_end].trim_end_matches('\r') != "---" {
        return (None, 0);
    }
    let yaml_start = first_end + 1;
    let mut pos = yaml_start;
    while pos <= text.len() {
        let end = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
        let line = text[pos..end].trim_end_matches('\r');
        if line == "---" || line == "..." {
            let body = (end + 1).min(text.len());
            return (Some(yaml_start..pos), body);
        }
        if end == text.len() {
            break;
        }
        pos = end + 1;
    }
    (None, 0)
}

fn collect_frontmatter_links(v: &Value, text: &str, yaml: &Range<usize>, out: &mut Parsed) {
    let Value::Object(map) = v else { return };
    let src = &text[yaml.clone()];
    for (key, val) in map {
        if has_nested_single_string_list(val) {
            out.unquoted_frontmatter_links.push(key.clone());
        }
        let mut strings = Vec::new();
        string_leaves(val, &mut strings);
        for s in strings {
            for m in WIKILINK.captures_iter(s) {
                let whole = m.get(0).expect("match").as_str();
                // Locate the text in the YAML for a line number; fall back to the block start.
                let span = src.find(whole).map_or(yaml.start..yaml.start, |i| {
                    yaml.start + i..yaml.start + i + whole.len()
                });
                let kind = if &m[1] == "!" {
                    LinkKind::Embed
                } else {
                    LinkKind::Wiki
                };
                out.links.push(RawLink {
                    kind,
                    target: link_target(&m[2]),
                    span,
                    location: Location::Frontmatter { key: key.clone() },
                });
            }
        }
    }
}

/// `key: [[a]]` and `- [[a]]` parse as lists of one-string lists.
fn has_nested_single_string_list(v: &Value) -> bool {
    match v {
        Value::Array(items) => items.iter().any(|i| match i {
            Value::Array(inner) => {
                (inner.len() == 1 && inner[0].is_string()) || has_nested_single_string_list(i)
            }
            _ => false,
        }),
        _ => false,
    }
}

fn string_leaves<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(items) => items.iter().for_each(|i| string_leaves(i, out)),
        Value::Object(map) => map.values().for_each(|i| string_leaves(i, out)),
        _ => {}
    }
}

/// Strip `|alias`, `#subpath`, a table-escape backslash, and whitespace.
#[must_use]
pub fn link_target(raw: &str) -> String {
    let no_alias = raw.split('|').next().unwrap_or(raw);
    let no_alias = no_alias.strip_suffix('\\').unwrap_or(no_alias);
    let no_sub = no_alias.split('#').next().unwrap_or(no_alias);
    no_sub
        .split('^')
        .next()
        .unwrap_or(no_sub)
        .trim()
        .to_string()
}

/// URLs, any `scheme:` (`mailto:`, `tel:`, `obsidian:`), anchors, and empty targets.
fn is_external(url: &str) -> bool {
    let scheme_len = url.find(':').filter(|&i| {
        let s = &url[..i];
        // One letter is a Windows drive (`C:`), not a scheme.
        s.len() > 1
            && s.starts_with(|c: char| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    scheme_len.is_some() || url.starts_with('#') || url.is_empty()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_body(text: &str, body_start: usize, out: &mut Parsed) {
    let body = &text[body_start..];
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_WIKILINKS);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut code_block: Option<usize> = None;
    let mut html: Vec<Range<usize>> = Vec::new();
    for (ev, range) in Parser::new_ext(body, opts).into_offset_iter() {
        let abs = body_start + range.start..body_start + range.end;
        match ev {
            Event::Start(Tag::CodeBlock(_)) => code_block = Some(abs.start),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = code_block.take() {
                    out.code.push(start..abs.end);
                }
            }
            Event::Code(_) => out.code.push(abs),
            Event::Html(_) | Event::InlineHtml(_) => html.push(abs),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                push_link(out, link_type, &dest_url, abs, false);
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => {
                push_link(out, link_type, &dest_url, abs, true);
            }
            _ => {}
        }
    }
    for range in &html {
        for m in WIKILINK.captures_iter(&text[range.clone()]) {
            let whole = m.get(0).expect("match");
            out.html_links.push(RawLink {
                kind: if &m[1] == "!" {
                    LinkKind::Embed
                } else {
                    LinkKind::Wiki
                },
                target: link_target(&m[2]),
                span: range.start + whole.start()..range.start + whole.end(),
                location: Location::Body,
            });
        }
    }
    out.non_prose.extend(html);
    for range in out.code.clone() {
        for m in WIKILINK.captures_iter(&text[range.clone()]) {
            let whole = m.get(0).expect("match");
            out.code_links.push(RawLink {
                kind: if &m[1] == "!" {
                    LinkKind::Embed
                } else {
                    LinkKind::Wiki
                },
                target: link_target(&m[2]),
                span: range.start + whole.start()..range.start + whole.end(),
                location: Location::Body,
            });
        }
    }
}

fn push_link(out: &mut Parsed, link_type: LinkType, dest: &str, span: Range<usize>, image: bool) {
    match link_type {
        LinkType::WikiLink { .. } => out.links.push(RawLink {
            kind: if image {
                LinkKind::Embed
            } else {
                LinkKind::Wiki
            },
            target: link_target(dest),
            span,
            location: Location::Body,
        }),
        LinkType::Inline | LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut
            if !image && !is_external(dest) =>
        {
            let path = dest.split('#').next().unwrap_or(dest);
            out.links.push(RawLink {
                kind: LinkKind::Markdown,
                target: percent_decode(path),
                span,
                location: Location::Body,
            });
        }
        // External links and images: their text is not prose that names a note.
        _ => out.non_prose.push(span),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_target_strips_suffixes() {
        assert_eq!(link_target("A/b#Head|Alias"), "A/b");
        assert_eq!(link_target("t\\"), "t");
        assert_eq!(link_target(" x ^blockid"), "x");
    }

    #[test]
    fn frontmatter_and_body_links() {
        let text = "---\nrelated: [\"[[A/b]]\"]\nbad: [[C]]\n---\n# T\n[[D|d]] `[[E]]` ![[img.png]] [m](F%20g.md) [w](https://x.y)\n";
        let p = parse(text);
        let targets: Vec<_> = p
            .links
            .iter()
            .map(|l| (l.target.as_str(), l.kind))
            .collect();
        assert!(targets.contains(&("A/b", LinkKind::Wiki)));
        assert!(targets.contains(&("D", LinkKind::Wiki)));
        assert!(targets.contains(&("img.png", LinkKind::Embed)));
        assert!(targets.contains(&("F g.md", LinkKind::Markdown)));
        assert!(!targets.iter().any(|(t, _)| t.contains("x.y")));
        assert_eq!(p.code_links.len(), 1);
        assert_eq!(p.unquoted_frontmatter_links, vec!["bad".to_string()]);
    }

    #[test]
    fn invalid_frontmatter_is_reported() {
        let p = parse("---\nrelated: [[A]], [[B]]\n---\nbody\n");
        assert!(p.frontmatter.error.is_some());
    }

    #[test]
    fn bom_does_not_hide_frontmatter() {
        let p = parse("\u{feff}---\naliases: [Bommy]\n---\nbody [[x]]\n");
        assert_eq!(p.frontmatter.aliases(), vec!["Bommy".to_string()]);
        assert_eq!(p.links.len(), 1);
    }

    #[test]
    fn schemes_are_external() {
        let p = parse(
            "[call](tel:123) [m](mailto:a@b.c) [o](obsidian://open?x) [n](note.md) [w](C:/x.md)\n",
        );
        let targets: Vec<_> = p.links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, ["note.md", "C:/x.md"]);
    }

    #[test]
    fn html_links_and_non_prose() {
        let p = parse(
            "<div>\n[[Target]]\n</div>\n\n[learn python](https://python.org) <span title=\"x\">y</span>\n",
        );
        assert_eq!(p.html_links.len(), 1);
        assert!(p.links.is_empty());
        assert!(p.non_prose.len() >= 3);
    }

    #[test]
    fn no_frontmatter() {
        let p = parse("# Title\n---\nnot yaml\n");
        assert!(p.frontmatter.yaml.is_none());
        assert_eq!(p.body_start, 0);
    }
}
