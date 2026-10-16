//! Just enough shell parsing to find `git commit` invocations in a Bash command,
//! and the directory each one runs in.
//!
//! Heredoc bodies and quoted text are data, not commands: `cat <<EOF ... git commit ... EOF`
//! and `echo "git commit"` are not commits.

/// A `git ... commit` found in a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    /// Directory changes before it, in order (`cd a && cd b` -> `["a", "b"]`), then `-C` values.
    pub dirs: Vec<String>,
}

/// Every `git commit` the command would run.
#[must_use]
pub fn git_commits(command: &str) -> Vec<GitCommit> {
    let mut out = Vec::new();
    let mut cds: Vec<String> = Vec::new();
    for words in simple_commands(&strip_heredocs(command)) {
        let mut w = words.iter().map(String::as_str).peekable();
        // Skip env assignments and wrappers.
        while let Some(&t) = w.peek() {
            let assignment = t.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            });
            if assignment || matches!(t, "sudo" | "command" | "exec" | "env" | "time" | "nice") {
                w.next();
            } else {
                break;
            }
        }
        let Some(program) = w.next() else { continue };
        if program == "cd" || program == "pushd" {
            if let Some(dir) = w.next() {
                cds.push(dir.to_string());
            }
            continue;
        }
        if program != "git" && !program.ends_with("/git") {
            continue;
        }
        let mut dirs = cds.clone();
        while let Some(t) = w.next() {
            match t {
                "-C" => {
                    if let Some(d) = w.next() {
                        dirs.push(d.to_string());
                    }
                }
                "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--exec-path" => {
                    w.next();
                }
                _ if t.starts_with("--work-tree=") => {
                    dirs.push(t["--work-tree=".len()..].to_string());
                }
                _ if t.starts_with('-') => {}
                "commit" => {
                    out.push(GitCommit { dirs });
                    break;
                }
                _ => break, // another subcommand
            }
        }
    }
    out
}

/// Drop heredoc bodies (the lines after `<<WORD` up to the line `WORD`).
fn strip_heredocs(command: &str) -> String {
    let mut out = String::new();
    let mut pending: Vec<String> = Vec::new();
    for line in command.lines() {
        if let Some(delim) = pending.first() {
            if line.trim() == delim {
                pending.remove(0);
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        pending.extend(heredoc_delims(line));
    }
    out
}

fn heredoc_delims(line: &str) -> Vec<String> {
    let mut delims = Vec::new();
    let mut rest = line;
    while let Some(i) = rest.find("<<") {
        let after = &rest[i + 2..];
        if let Some(here_string) = after.strip_prefix('<') {
            // `<<<` here-string: no body.
            rest = here_string;
            continue;
        }
        let after = after.trim_start_matches('-').trim_start();
        let word: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '&' | '|' | ')' | '<' | '>'))
            .collect();
        let word = word.trim_matches(|c| c == '\'' || c == '"').to_string();
        if !word.is_empty() {
            delims.push(word);
        }
        rest = after;
    }
    delims
}

/// Split into simple commands (by `;`, `&&`, `||`, `|`, `&`, newlines, parens),
/// each as a list of words with quotes removed.
fn simple_commands(command: &str) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                // A backslash-newline joins lines; any other escaped char is literal.
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    in_word = true;
                    word.push(n);
                }
            }
            '#' if !in_word => {
                // Comment to end of line.
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                end_word(&mut word, &mut in_word, &mut words);
                cmds.push(std::mem::take(&mut words));
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '`' => {
                end_word(&mut word, &mut in_word, &mut words);
                if !words.is_empty() {
                    cmds.push(std::mem::take(&mut words));
                }
            }
            c if c.is_whitespace() => end_word(&mut word, &mut in_word, &mut words),
            _ => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut words);
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds.retain(|c| !c.is_empty());
    cmds
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(c: &str) -> Vec<Vec<String>> {
        git_commits(c).into_iter().map(|g| g.dirs).collect()
    }

    #[test]
    fn finds_commits() {
        assert_eq!(dirs("git commit -m x"), vec![Vec::<String>::new()]);
        assert_eq!(
            dirs("cd a && git commit -q -F f"),
            vec![vec!["a".to_string()]]
        );
        assert_eq!(dirs("git -C x commit"), vec![vec!["x".to_string()]]);
        assert_eq!(
            dirs("git add . && git -c user.name=v commit"),
            vec![Vec::<String>::new()]
        );
        assert_eq!(dirs("FOO=1 git commit"), vec![Vec::<String>::new()]);
        assert_eq!(
            dirs("cd ~/r && git add a\ngit commit -m 'msg; with git commit'").len(),
            1
        );
    }

    #[test]
    fn ignores_data() {
        for c in [
            "git status",
            "echo commit",
            "git log; echo commit",
            "echo 'git commit'",
            "echo \"cd x && git commit\"",
            "cat > f <<'EOF'\nrun `git commit` later\ngit commit -m x\nEOF\necho done",
            "python3 - <<EOF\nprint('git commit')\nEOF",
            "grep -n 'git commit' file",
            "git log --grep commit",
        ] {
            assert!(git_commits(c).is_empty(), "{c}");
        }
    }

    #[test]
    fn commit_after_heredoc_counts() {
        let c = "cat > f <<'EOF'\ntext\nEOF\ngit commit -m done";
        assert_eq!(git_commits(c).len(), 1);
    }
}
