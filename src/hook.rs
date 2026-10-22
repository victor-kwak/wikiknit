//! `wikiknit hook read`: a Claude Code `PostToolUse` (and `PostToolUseFailure`) hook. When the agent reads a vault note
//! (the `Read` tool, or a shell command that prints the file), it gets that note's trust
//! findings (`stale`, `drift`) next to what it read. It never blocks: no finding, or anything
//! that goes wrong, prints nothing.
//!
//! Contract (Claude Code hooks docs): stdin is one JSON object with `hook_event_name`,
//! `tool_name`, `tool_input` (`Read`: absolute `file_path`; `Bash`: `command`) and `cwd`;
//! `{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"..."}}` on
//! stdout with exit 0 puts the text next to the tool result. A `Bash` command that exits
//! non-zero fires `PostToolUseFailure` instead, with the same fields (its output is in `error`),
//! and takes the same answer under that event name. `additionalContext` is capped at
//! 10,000 characters.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::config::Config;
use crate::rules::{self, Finding};
use crate::vault::Note;

/// Notes looked at per tool call: a command that prints more is a bulk dump, not a read.
const MAX_NOTES: usize = 20;
/// Stays under Claude Code's 10,000-character cap, which moves longer text to a file.
const MAX_CONTEXT: usize = 8_000;

/// Commands that print the files they are given (all of them, or the lines asked for).
const PRINTERS: &[&str] = &[
    "cat", "head", "tail", "sed", "less", "more", "bat", "batcat", "nl", "awk", "grep", "egrep",
    "rg",
];

/// The hook's stdout for this stdin: `None` prints nothing.
#[must_use]
pub fn read(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    // A command that printed a note and then exited non-zero is a `PostToolUseFailure`.
    let event = v.get("hook_event_name")?.as_str()?;
    if event != "PostToolUse" && event != "PostToolUseFailure" {
        return None;
    }
    let tool_input = v.get("tool_input")?;
    let cwd = v.get("cwd").and_then(Value::as_str).map(PathBuf::from);
    let paths = match v.get("tool_name")?.as_str()? {
        "Read" => vec![PathBuf::from(tool_input.get("file_path")?.as_str()?)],
        "Bash" => printed_files(tool_input.get("command")?.as_str()?, cwd.as_deref()),
        _ => return None,
    };
    let text = report(&paths)?;
    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": text,
        }
    });
    Some(out.to_string())
}

/// The trust findings of the notes among `paths`, as text for the agent.
fn report(paths: &[PathBuf]) -> Option<String> {
    let mut configs: BTreeMap<PathBuf, Option<Config>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut text = String::new();
    for path in paths {
        if seen.len() == MAX_NOTES {
            break;
        }
        let Some((config_file, path)) = note_file(path) else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }
        let cfg = configs
            .entry(config_file.clone())
            // Quietly: notes about removed keys belong to `check`, not to every read.
            .or_insert_with(|| Config::load_with(&config_file, false).ok());
        let Some(cfg) = cfg.as_ref() else { continue };
        let Some(note) = load_note(cfg, &path) else {
            continue;
        };
        let found = rules::trust(cfg, &note);
        if found.is_empty() {
            continue;
        }
        let block = describe(&note.id, &found);
        if text.len() + block.len() > MAX_CONTEXT {
            break;
        }
        text.push_str(&block);
    }
    (!text.is_empty()).then(|| {
        format!(
            "{text}(Found by wikiknit from the note's frontmatter and the vault's wikiknit.toml.)"
        )
    })
}

/// The findings as text. Messages and fixes hold text from the note's frontmatter and the
/// vault's `wikiknit.toml` (a repository path, a `fix`): each is one line, without control
/// characters, cut at `MAX_FIELD`, so a vault cannot shape the context the agent gets beyond
/// what reading the note already shows it.
fn describe(id: &str, found: &[Finding]) -> String {
    let mut s = format!(
        "wikiknit: the vault note {} may be out of date.\n",
        one_line(id)
    );
    for f in found {
        s.push_str(&format!(
            "- {} (line {}): {}. Fix: {}\n",
            f.rule,
            f.line,
            one_line(&f.message),
            one_line(&f.fix)
        ));
    }
    s
}

/// Longest message or fix text kept, in characters.
const MAX_FIELD: usize = 400;

fn one_line(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_FIELD)
        .collect();
    if s.chars().count() > MAX_FIELD {
        out.push_str("...");
    }
    out
}

/// A Markdown file that exists, with the `wikiknit.toml` found from its folder upward (to the
/// top of its git work tree).
fn note_file(path: &Path) -> Option<(PathBuf, PathBuf)> {
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
    {
        return None;
    }
    let path = lexical(path)?;
    if !path.is_file() {
        return None;
    }
    let config = Config::find(path.parent()?)?;
    Some((config, path))
}

/// The note at `path` if it is one of the vault's notes, as `Vault::load` would see it: inside
/// the vault, not hidden, not ignored.
fn load_note(cfg: &Config, path: &Path) -> Option<Note> {
    let real = path.canonicalize().ok()?;
    if !real.starts_with(&cfg.vault) {
        return None;
    }
    // The real path is the note: the vault walk reads a symlink to a note as a file, and
    // checks the note it points to.
    let rel = real.strip_prefix(&cfg.vault).ok()?;
    let mut parts = Vec::new();
    for c in rel.components() {
        let Component::Normal(p) = c else { return None };
        let p = p.to_str()?;
        if p.starts_with('.') {
            return None;
        }
        parts.push(p);
    }
    let rel = parts.join("/");
    if cfg.is_ignored(&rel) {
        return None;
    }
    let bytes = std::fs::read(&real).ok()?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let id = &rel[..rel.len() - 3];
    Some(Note::from_text(id, real, text))
}

/// `a/./b/../c` -> `a/c` without touching the disk; `None` for a relative path.
fn lexical(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    Some(out)
}

/// The `.md` files a shell command prints: arguments of `cat`, `head`, `sed`, `grep`, ...
/// (not redirect targets, not `sed -i`), resolved from `cwd` and the `cd`s before them, with
/// globs in the last component expanded. Heredoc bodies are skipped. Words with `$` or
/// backquotes are not guessed at.
#[must_use]
pub fn printed_files(command: &str, cwd: Option<&Path>) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut cwd = cwd.map(Path::to_path_buf);
    // The folder outside each subshell `( ... )` we are in: a `cd` inside one ends with it.
    let mut outer = Vec::new();
    let mut out = Vec::new();
    for words in simple_commands(command) {
        match words.first().map(String::as_str) {
            Some(SUBSHELL_START) => {
                outer.push(cwd.clone());
                continue;
            }
            Some(SUBSHELL_END) => {
                if let Some(c) = outer.pop() {
                    cwd = c;
                }
                continue;
            }
            _ => {}
        }
        let Some((name, args)) = command_name(&words) else {
            continue;
        };
        if name == "cd" || name == "pushd" {
            cwd = match args.iter().find(|a| !a.starts_with('-') || *a == "-") {
                None => home.clone(),
                // `cd -`: the previous folder is not known.
                Some(dir) if dir == "-" => None,
                Some(dir) => expand(dir, cwd.as_deref(), home.as_deref()),
            };
            continue;
        }
        if !PRINTERS.contains(&name) {
            continue;
        }
        // `sed -i` and `awk -i inplace` write the file; `grep -l`/`-c`/`-q` print no line of it.
        let long = ["--count", "--files-with", "--quiet", "--silent"];
        let silent = match name {
            "sed" => has_flag(args, &['i'], &["--in-place"]),
            "awk" => args.iter().any(|a| a.ends_with("inplace")),
            "grep" | "egrep" => has_flag(args, &['c', 'l', 'L', 'q'], &long),
            // `rg -L` follows symlinks.
            "rg" => has_flag(args, &['c', 'l', 'q'], &long),
            _ => false,
        };
        if silent {
            continue;
        }
        // grep's first operand is its pattern, unless `-e`/`-f` give it (and their argument
        // is a pattern or a pattern file, not a file it prints).
        let searcher = matches!(name, "grep" | "egrep" | "rg");
        let mut pattern_left = searcher
            && !has_flag(args, &['e', 'f'], &["--regexp", "--file"])
            && !args.iter().any(|a| a == "-e" || a == "-f");
        let mut skip = false;
        for a in args {
            if skip {
                skip = false;
                continue;
            }
            // `> f`, `2>> f`, `&> f`: the next word is written, not read. `>f` is one word.
            // `<<< word`: the word is input text.
            let bare = a.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
            if bare == ">" || bare == ">>" || a == "<<<" || (searcher && (a == "-e" || a == "-f")) {
                skip = true;
                continue;
            }
            if bare.starts_with('>') {
                continue;
            }
            let a = a.strip_prefix('<').unwrap_or(a);
            if a.starts_with('-') {
                continue;
            }
            if pattern_left {
                pattern_left = false;
                continue;
            }
            if !a.to_ascii_lowercase().ends_with(".md") {
                continue;
            }
            if a.contains(['*', '?']) {
                out.extend(glob(a, cwd.as_deref(), home.as_deref()));
            } else if let Some(p) = expand(a, cwd.as_deref(), home.as_deref()) {
                out.push(p);
            }
        }
    }
    out
}

/// An option among `args`: a short one in a cluster (`-ni`), or a long one by prefix
/// (`--in-place=.bak`).
fn has_flag(args: &[String], short: &[char], long: &[&str]) -> bool {
    args.iter().any(|a| match a.strip_prefix("--") {
        Some(_) => long.iter().any(|f| a.starts_with(f)),
        None => a.starts_with('-') && a[1..].chars().any(|c| short.contains(&c)),
    })
}

/// The files a glob names, as the shell would expand it: `*` and `?` in the last component
/// only (`Projects/x/*.md`), hidden files only for a pattern that starts with `.`.
fn glob(word: &str, cwd: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let (dir, pattern) = word.rsplit_once('/').unwrap_or(("", word));
    if dir.contains(['*', '?']) {
        return Vec::new();
    }
    let dir = if dir.is_empty() && !word.starts_with('/') {
        cwd.map(Path::to_path_buf)
    } else {
        expand(if dir.is_empty() { "/" } else { dir }, cwd, home)
    };
    let Some(Ok(entries)) = dir.as_ref().map(std::fs::read_dir) else {
        return Vec::new();
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name: Vec<char> = e.file_name().to_string_lossy().chars().collect();
            (pattern.first() == Some(&'.') || name.first() != Some(&'.'))
                && wildcard(&pattern, &name)
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

/// `*` (any run) and `?` (one character) matching.
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some(('*', rest)) => (0..=name.len()).any(|i| wildcard(rest, &name[i..])),
        Some(('?', rest)) => !name.is_empty() && wildcard(rest, &name[1..]),
        Some((c, rest)) => name.first() == Some(c) && wildcard(rest, &name[1..]),
    }
}

/// A word as a path: `~/x` from home, `x` from `cwd`. `None` when it cannot be known.
fn expand(word: &str, cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if word.contains(['$', '`', '*', '?', '[']) {
        return None;
    }
    if word == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = word.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    if word.starts_with('~') {
        return None;
    }
    let p = Path::new(word);
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    cwd.map(|c| c.join(p))
}

/// The command name (basename, after `VAR=value` assignments and `sudo`/`command`/`env`/
/// `exec`) and its arguments.
fn command_name(words: &[String]) -> Option<(&str, &[String])> {
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        let assignment = w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if assignment || matches!(w.as_str(), "sudo" | "command" | "env" | "exec" | "time") {
            i += 1;
            continue;
        }
        let name = w.rsplit('/').next().unwrap_or(w);
        return Some((name, &words[i + 1..]));
    }
    None
}

/// Pseudo-commands `simple_commands` gives for `(` and `)`.
const SUBSHELL_START: &str = "(";
const SUBSHELL_END: &str = ")";

/// Split a shell command into simple commands (at `;`, `&&`, `||`, `|`, `&`, newlines,
/// parentheses, braces), each a list of words with quotes removed; `(` and `)` also come as
/// commands of their own (`SUBSHELL_START`, `SUBSHELL_END`), so a `cd` in a subshell ends with
/// it. Heredoc bodies and comments
/// are dropped. A best effort: no expansion, no nested quoting rules beyond `'`, `"`, `\`.
#[must_use]
pub fn simple_commands(command: &str) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // Heredoc delimiters waiting for the end of the current line: (delimiter, strip tabs).
    let mut pending: Vec<(String, bool)> = Vec::new();
    let mut heredoc_next = None::<bool>;
    let mut chars = command.chars().peekable();
    let finish_word = |word: &mut String,
                       in_word: &mut bool,
                       words: &mut Vec<String>,
                       pending: &mut Vec<(String, bool)>,
                       heredoc_next: &mut Option<bool>| {
        if !*in_word {
            return;
        }
        let w = std::mem::take(word);
        *in_word = false;
        if let Some(strip) = heredoc_next.take() {
            pending.push((w, strip));
        } else {
            words.push(w);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    in_word = true;
                    word.push(n);
                }
            }
            '#' if !in_word => {
                while chars.peek().is_some_and(|&c| c != '\n') {
                    chars.next();
                }
            }
            '<' if !in_word && chars.peek() == Some(&'<') => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    // `<<<` here-string: the next word is input text, not a file.
                    chars.next();
                    words.push("<<<".into());
                    continue;
                }
                let strip = chars.peek() == Some(&'-');
                if strip {
                    chars.next();
                }
                heredoc_next = Some(strip);
            }
            ' ' | '\t' => {
                finish_word(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut pending,
                    &mut heredoc_next,
                );
            }
            '\n' | ';' | '&' | '|' | '(' | ')' | '{' | '}'
                if !(c == '&' && (word.ends_with('>') || chars.peek() == Some(&'>'))) =>
            {
                // `{`/`}` split only as their own word (`a{b}` stays a word).
                if (c == '{' || c == '}') && in_word {
                    word.push(c);
                    continue;
                }
                finish_word(
                    &mut word,
                    &mut in_word,
                    &mut words,
                    &mut pending,
                    &mut heredoc_next,
                );
                if !words.is_empty() {
                    cmds.push(std::mem::take(&mut words));
                }
                if c == '(' {
                    cmds.push(vec![SUBSHELL_START.to_string()]);
                } else if c == ')' {
                    cmds.push(vec![SUBSHELL_END.to_string()]);
                }
                if c == '\n' && !pending.is_empty() {
                    // Skip each heredoc body, in order, up to its delimiter line.
                    for (delim, strip) in pending.drain(..) {
                        loop {
                            let mut line = String::new();
                            let mut ended = false;
                            for c in chars.by_ref() {
                                if c == '\n' {
                                    ended = true;
                                    break;
                                }
                                line.push(c);
                            }
                            let line = if strip {
                                line.trim_start_matches('\t')
                            } else {
                                &line
                            };
                            if line == delim || !ended {
                                break;
                            }
                        }
                    }
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    finish_word(
        &mut word,
        &mut in_word,
        &mut words,
        &mut pending,
        &mut heredoc_next,
    );
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(cmd: &str) -> Vec<String> {
        printed_files(cmd, Some(Path::new("/w")))
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn printed_files_follow_cd_and_skip_writes() {
        assert_eq!(
            files("cd /v && cat INDEX.md; sed -n 1,40p 'Projects/a b/overview.md' | head"),
            ["/v/INDEX.md", "/v/Projects/a b/overview.md"]
        );
        assert_eq!(
            files("head -40 x.md 2>/dev/null; grep -n foo y.md"),
            ["/w/x.md", "/w/y.md"]
        );
        assert!(files("cat > x.md <<'EOF'\ncat y.md\nEOF\necho done").is_empty());
        assert!(files("sed -i 's/a/b/' x.md; sed --in-place=.b s/a/b/ y.md").is_empty());
        assert!(
            files("grep -c a x.md; grep -rl a y.md; rg --files-with-matches a z.md").is_empty()
        );
        assert_eq!(files("grep -in a x.md"), ["/w/x.md"]);
        // grep's pattern, `-e`/`-f` arguments, and here-strings are not files it prints.
        assert!(
            files("grep -rn \"notes/old.md\" .; grep -f p.md x.txt; grep a <<< b.md").is_empty()
        );
        assert_eq!(files("grep -e old.md -n x.md y.txt"), ["/w/x.md"]);
        assert_eq!(files("rg -L foo x.md"), ["/w/x.md"]);
        assert!(files("awk -i inplace 1 x.md").is_empty());
        // A `cd` in a subshell ends with it; after `cd -` the folder is unknown.
        assert_eq!(
            files("(cd /s && cat a.md); cat b.md"),
            ["/s/a.md", "/w/b.md"]
        );
        assert!(files("cd /s; cd -; cat c.md").is_empty());
        assert!(files("cat a.md >> b.md").contains(&"/w/a.md".to_string()));
        assert!(!files("cat a.md >> b.md").contains(&"/w/b.md".to_string()));
        assert!(files("git add x.md; echo y.md; cat $D/x.md").is_empty());
        assert_eq!(
            files("python3 - <<'PY'\nopen('cat z.md')\nPY\ncat after.md"),
            ["/w/after.md"]
        );
        assert_eq!(files("cat <x.md"), ["/w/x.md"]);
        assert_eq!(files("X=1 /usr/bin/cat x.md # cat no.md"), ["/w/x.md"]);
    }

    #[test]
    fn simple_commands_split() {
        assert_eq!(
            simple_commands("a 'b c' \"d\\\"e\" && f|g; (h)"),
            vec![
                vec!["a".to_string(), "b c".into(), "d\"e".into()],
                vec!["f".into()],
                vec!["g".into()],
                vec!["(".into()],
                vec!["h".into()],
                vec![")".into()],
            ]
        );
        assert_eq!(
            simple_commands("cat x &>/dev/null"),
            vec![vec!["cat".to_string(), "x".into(), "&>/dev/null".into()]]
        );
    }
}
