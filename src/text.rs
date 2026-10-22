//! Small text helpers shared by catalog and links.

use std::sync::LazyLock;

use regex::Regex;

static WIKILINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"!?\[\[([^\[\]|\n]+)(?:\|([^\[\]\n]+))?\]\]").expect("valid regex")
});
static MD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[([^\]]*)\]\([^)]*\)").expect("valid regex"));

/// Display text for a link: its alias, else the note's name (`Projects/x/overview` -> `x`),
/// without `#subpath`.
#[must_use]
pub fn link_display(target: &str, alias: Option<&str>, generic: &[String]) -> String {
    if let Some(a) = alias {
        return a.trim().to_string();
    }
    let path = target.split('#').next().unwrap_or(target).trim();
    let mut parts = path.rsplit('/');
    let stem = parts.next().unwrap_or(path);
    if generic.iter().any(|g| g.eq_ignore_ascii_case(stem))
        && let Some(parent) = parts.next()
    {
        return parent.to_string();
    }
    stem.to_string()
}

/// `s` without its wikilinks and Markdown links.
#[must_use]
pub fn strip_links(s: &str) -> String {
    let s = WIKILINK.replace_all(s, "");
    MD_LINK.replace_all(&s, "").into_owned()
}

/// Replace wikilinks and Markdown links with their display text.
#[must_use]
pub fn links_to_text(s: &str, generic: &[String]) -> String {
    let s = WIKILINK.replace_all(s, |c: &regex::Captures<'_>| {
        link_display(&c[1], c.get(2).map(|a| a.as_str()), generic)
    });
    MD_LINK.replace_all(&s, "$1").into_owned()
}

/// Tracks fenced code blocks line by line: a fence closes only with the same marker
/// character and at least the same length (CommonMark).
#[derive(Debug, Default)]
pub struct Fence {
    open: Option<(char, usize)>,
}

impl Fence {
    /// Feed the next line; true if the line is inside a fence or is a fence line.
    pub fn line(&mut self, line: &str) -> bool {
        let l = line.trim_start();
        let marker = l.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = marker.map_or(0, |m| l.chars().take_while(|c| *c == m).count());
        match (self.open, marker) {
            (None, Some(m)) if run >= 3 => {
                self.open = Some((m, run));
                true
            }
            (Some((m, n)), Some(c))
                if c == m && run >= n && l[run * c.len_utf8()..].trim().is_empty() =>
            {
                self.open = None;
                true
            }
            (Some(_), _) => true,
            _ => false,
        }
    }
}

/// A window of at most `max` chars of `line` around byte `focus`, with `...` where cut.
#[must_use]
pub fn window(line: &str, focus: usize, max: usize) -> String {
    let line = line.trim_end();
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.len() <= max {
        return line.trim_start().to_string();
    }
    let at = chars.partition_point(|(b, _)| *b < focus);
    let before = max / 3;
    let start = at.saturating_sub(before).min(chars.len() - max);
    let end = start + max;
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.extend(chars[start..end].iter().map(|(_, c)| c));
    if end < chars.len() {
        out.push_str("...");
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_close_with_the_same_marker() {
        let mut f = Fence::default();
        let inside: Vec<bool> = ["a", "```", "~~~", "x", "````", "b"]
            .iter()
            .map(|l| f.line(l))
            .collect();
        assert_eq!(inside, [false, true, true, true, true, false]);
    }

    #[test]
    fn window_centers_on_focus() {
        let line = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let w = window(&line, 100, 30);
        assert!(
            w.contains("needle") && w.starts_with("...") && w.ends_with("..."),
            "{w}"
        );
        assert_eq!(window("short line", 0, 30), "short line");
    }

    #[test]
    fn links_become_display_text() {
        let g = vec!["overview".to_string()];
        assert_eq!(
            links_to_text("see [[Projects/x/overview]] and [[A/b|Bee]] [m](u)", &g),
            "see x and Bee m"
        );
    }
}
