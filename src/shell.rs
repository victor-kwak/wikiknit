//! Just enough shell parsing to find the git commands a Bash command would run,
//! and the directory each one runs in.
//!
//! Data is not a command: heredoc bodies, quoted text, comments, and arithmetic
//! (`cat <<EOF ... git commit ... EOF`, `echo "git commit"`, `$((1<<2))`, `(( x = 1 << 2 ))`).
//! Commands are found behind shell keywords (`if ...; then git commit; fi`), wrappers (`sudo`,
//! `timeout`, `xargs`, `env`, ...), `bash -c '...'`, `eval`, command substitution (also inside
//! double quotes), and heredocs fed to a shell (`bash <<EOF`). `cd` inside a subshell or
//! substitution does not leak out of it.

/// A git invocation found in a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCall {
    /// Directory changes before it, in order (`cd a && cd b` -> `["a", "b"]`), then `-C` values.
    pub dirs: Vec<String>,
    /// The subcommand (`commit`, `add`, or an alias such as `ci`).
    pub subcommand: String,
    /// Aliases defined on the command line: `git -c alias.ci=commit ci`.
    pub inline_aliases: Vec<(String, String)>,
    /// Config or command lookup is overridden for this call (`-c` other than `alias.*`,
    /// `--config-env`, `--exec-path`, or `GIT_*`, `HOME`, `XDG_CONFIG_HOME`, `PATH` set in
    /// the command), so an alias or external command cannot be resolved reliably.
    pub config_overridden: bool,
}

/// Every git invocation the command would run.
#[must_use]
pub fn git_calls(command: &str) -> Vec<GitCall> {
    let mut out = Vec::new();
    let mut cds = Vec::new();
    collect(command, &mut cds, &mut out, 0);
    out
}

/// Every `git commit` the command would run (aliases not resolved).
#[must_use]
pub fn git_commits(command: &str) -> Vec<GitCall> {
    git_calls(command)
        .into_iter()
        .filter(|g| g.subcommand == "commit")
        .collect()
}

const MAX_DEPTH: usize = 6;
const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh"];

fn collect(command: &str, cds: &mut Vec<String>, out: &mut Vec<GitCall>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    // cds length at each open subshell / substitution
    let mut scopes: Vec<usize> = Vec::new();
    // every simple command so far, by index (heredoc bodies refer back to their command)
    let mut commands: Vec<Vec<String>> = Vec::new();
    // piped[i]: command i's output is piped into command i + 1
    let mut piped: Vec<bool> = Vec::new();
    let reads_stdin_shell = |words: &[String]| {
        let words = strip_prefixes(words);
        words
            .split_first()
            .is_some_and(|(p, args)| SHELLS.contains(&program_name(p)) && shell_reads_stdin(args))
    };
    // `export GIT_CONFIG_COUNT=...` (or a bare assignment) earlier in the command.
    let mut env_config = false;
    for item in tokenize(command) {
        match item {
            Item::Pipe => {
                if let Some(last) = piped.last_mut() {
                    *last = true;
                }
            }
            Item::Open => scopes.push(cds.len()),
            Item::Close => {
                if let Some(n) = scopes.pop() {
                    cds.truncate(n);
                }
            }
            Item::Sub(script) => {
                let mut inner = cds.clone();
                collect(&script, &mut inner, out, depth + 1);
            }
            Item::Body { cmd, text } => {
                // The body is a script when its command, or a command it is piped into
                // (`cat <<EOF | sh`), is a shell reading stdin.
                let mut j = cmd;
                let mut feeds_shell = false;
                while let Some(words) = commands.get(j) {
                    if reads_stdin_shell(words) {
                        feeds_shell = true;
                        break;
                    }
                    if !piped.get(j).copied().unwrap_or(false) {
                        break;
                    }
                    j += 1;
                }
                if feeds_shell {
                    let mut inner = cds.clone();
                    collect(&text, &mut inner, out, depth + 1);
                }
            }
            Item::Cmd(words) => {
                commands.push(words.clone());
                piped.push(false);
                // Variables that change where git finds config or commands.
                let sets_git_config = words.iter().any(|w| {
                    w.split_once('=').is_some_and(|(k, _)| {
                        k.starts_with("GIT_") || matches!(k, "HOME" | "XDG_CONFIG_HOME" | "PATH")
                    })
                });
                let first = words.first().map(String::as_str);
                if sets_git_config
                    && (matches!(first, Some("export" | "declare" | "typeset" | "readonly"))
                        || words.iter().all(|w| w.contains('=')))
                {
                    env_config = true;
                }
                let words = strip_prefixes(&words);
                let Some((program, args)) = words.split_first() else {
                    continue;
                };
                match program_name(program) {
                    "cd" | "pushd" => {
                        if let Some(dir) = args.iter().find(|a| !a.starts_with('-')) {
                            cds.push(dir.clone());
                        }
                    }
                    "popd" => {
                        cds.pop();
                    }
                    name if SHELLS.contains(&name) => {
                        if let Some(script) = shell_c_script(args) {
                            let mut inner = cds.clone();
                            collect(script, &mut inner, out, depth + 1);
                        }
                    }
                    "eval" => {
                        let mut inner = cds.clone();
                        collect(&args.join(" "), &mut inner, out, depth + 1);
                    }
                    "git" => {
                        if let Some(call) = parse_git(args, cds, env_config || sets_git_config) {
                            out.push(call);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn program_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Shell options that take a separate argument.
fn shell_opt_takes_arg(opt: &str) -> bool {
    matches!(opt, "-o" | "+o" | "-O" | "+O" | "--rcfile" | "--init-file")
}

/// The script of `bash -c SCRIPT` (also `-lc`, `-ec`, `-o pipefail -c`, `-euo pipefail -c`).
fn shell_c_script(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if a == "--" {
            return None;
        }
        if shell_opt_takes_arg(a) {
            it.next();
            continue;
        }
        if a.starts_with("--") {
            continue;
        }
        if (a.starts_with('-') || a.starts_with('+')) && a.len() > 1 {
            let cluster = &a[1..];
            if cluster.contains(['o', 'O']) {
                it.next(); // `-euo pipefail`
            }
            if a.starts_with('-') && cluster.contains('c') {
                return it.next().map(String::as_str);
            }
            continue;
        }
        return None; // a script file
    }
    None
}

/// `bash` with only options reads its script from stdin (a heredoc).
fn shell_reads_stdin(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if shell_opt_takes_arg(a) {
            it.next();
            continue;
        }
        if a.starts_with('-') || a.starts_with('+') {
            if a.len() > 1 && !a.starts_with("--") && a[1..].contains('c') {
                return false;
            }
            if a.len() > 1 && !a.starts_with("--") && a[1..].contains(['o', 'O']) {
                it.next();
            }
            continue;
        }
        return false;
    }
    true
}

fn parse_git(args: &[String], cds: &[String], config_overridden: bool) -> Option<GitCall> {
    let mut dirs = cds.to_vec();
    let mut inline_aliases = Vec::new();
    let mut config_overridden = config_overridden;
    let mut it = args.iter();
    while let Some(t) = it.next() {
        match t.as_str() {
            "-C" | "--work-tree" | "--git-dir" => dirs.push(it.next()?.clone()),
            "-c" => {
                let kv = it.next()?;
                // Config keys are case-insensitive: `-c ALIAS.X=commit` defines alias `x`.
                let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), "true"));
                let k = k.to_ascii_lowercase();
                if let Some(name) = k.strip_prefix("alias.") {
                    inline_aliases.push((name.to_string(), v.to_string()));
                } else {
                    config_overridden = true; // include.path, includeIf, ...
                }
            }
            "--config-env" | "--exec-path" => {
                it.next();
                config_overridden = true;
            }
            s if s.starts_with("--config-env=") || s.starts_with("--exec-path") => {
                config_overridden = true;
            }
            "--namespace" => {
                it.next();
            }
            s if s.starts_with("--work-tree=") => {
                dirs.push(s["--work-tree=".len()..].to_string());
            }
            s if s.starts_with("--git-dir=") => dirs.push(s["--git-dir=".len()..].to_string()),
            s if s.starts_with('-') => {}
            sub => {
                return Some(GitCall {
                    dirs,
                    subcommand: sub.to_string(),
                    inline_aliases,
                    config_overridden,
                });
            }
        }
    }
    None
}

/// Options (of a wrapper) that take a separate argument.
fn wrapper_arg_opts(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "sudo" => &["-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U", "-T"],
        "timeout" => &["-s", "-k", "--signal", "--kill-after"],
        "nice" => &["-n", "--adjustment"],
        "ionice" => &["-c", "-n", "-p", "-P", "-u"],
        "xargs" => &[
            "-I",
            "-i",
            "-n",
            "-P",
            "-L",
            "-l",
            "-d",
            "-E",
            "-e",
            "-s",
            "-a",
            "--max-args",
            "--max-procs",
            "--max-lines",
            "--max-chars",
            "--delimiter",
            "--arg-file",
            "--eof",
            "--process-slot-var",
        ],
        "stdbuf" => &["-i", "-o", "-e"],
        "env" => &["-u", "-C", "-S", "--unset", "--chdir"],
        "doas" => &["-u", "-C"],
        _ => &[],
    }
}

/// Drop shell keywords, env assignments, and wrapper commands (with their options)
/// in front of the real program.
fn strip_prefixes(words: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < words.len() {
        let t = words[i].as_str();
        let assignment = t.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty()
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !k.starts_with(|c: char| c.is_ascii_digit())
        });
        if assignment
            || matches!(
                t,
                "if" | "then"
                    | "else"
                    | "elif"
                    | "do"
                    | "while"
                    | "until"
                    | "{"
                    | "}"
                    | "!"
                    | "time"
                    | "coproc"
                    | "command"
                    | "exec"
                    | "nohup"
                    | "builtin"
            )
        {
            i += 1;
            continue;
        }
        let name = program_name(t);
        if matches!(
            name,
            "sudo"
                | "doas"
                | "timeout"
                | "nice"
                | "ionice"
                | "xargs"
                | "stdbuf"
                | "env"
                | "chronic"
        ) {
            let arg_opts = wrapper_arg_opts(name);
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                let opt = words[i].as_str();
                i += 1;
                if opt == "--" {
                    break;
                }
                if arg_opts.contains(&opt) {
                    i += 1;
                }
            }
            // timeout DURATION
            if name == "timeout" && i < words.len() {
                i += 1;
            }
            continue;
        }
        break;
    }
    words[i..].to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A simple command, words with quotes removed.
    Cmd(Vec<String>),
    /// Start of a subshell or command substitution.
    Open,
    /// End of one.
    Close,
    /// A command substitution found inside double quotes.
    Sub(String),
    /// A heredoc body, fed to the command with this index (in order of `Cmd` items).
    Body { cmd: usize, text: String },
    /// The previous command's output is piped into the next one (`|`, `|&`).
    Pipe,
}

struct Heredoc {
    delim: String,
    strip_tabs: bool,
    /// The delimiter was quoted (`<<'EOF'`): the body is literal. Unquoted bodies expand
    /// `$(...)` and backticks, which run commands.
    quoted: bool,
    cmd: usize,
}

/// Command substitutions (`$(...)`, backticks) in an unquoted heredoc body.
fn body_substitutions(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut subs = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '$' if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) != Some(&'(') => {
                let end = balanced_close(&chars, i + 1);
                subs.push(
                    chars[i + 2..end.saturating_sub(1).max(i + 2)]
                        .iter()
                        .collect(),
                );
                i = end;
            }
            '`' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != '`' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                subs.push(chars[start..j.min(chars.len())].iter().collect());
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    subs
}

/// Index just past the `)` that closes the `(` at `open` (or the end), skipping quotes.
fn balanced_close(chars: &[char], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '\'' => {
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

#[allow(clippy::too_many_lines)] // one state machine; splitting it hides the state
fn tokenize(command: &str) -> Vec<Item> {
    let chars: Vec<char> = command.chars().collect();
    let mut items = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut cmd_count = 0usize;
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut in_backtick = false;
    let mut i = 0;

    // Nesting depth of `(`/`$(`/backticks, and that depth at each open `case`: inside a
    // `case`, a `)` at the same depth ends a pattern (`a) ...;;`), not a subshell.
    let mut open_depth = 0usize;
    let mut cases: Vec<usize> = Vec::new();
    macro_rules! end_word {
        () => {
            if in_word {
                if words.is_empty() && !word.is_empty() {
                    match word.as_str() {
                        "case" => cases.push(open_depth),
                        "esac" => {
                            cases.pop();
                        }
                        _ => {}
                    }
                }
                words.push(std::mem::take(&mut word));
                in_word = false;
            }
        };
    }
    macro_rules! end_cmd {
        () => {
            end_word!();
            if !words.is_empty() {
                items.push(Item::Cmd(std::mem::take(&mut words)));
                cmd_count += 1;
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() => {
                            i += 1;
                            word.push(chars[i]);
                        }
                        '$' if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) != Some(&'(') => {
                            // "$(...)": a command inside the string.
                            let end = balanced_close(&chars, i + 1);
                            let inner: String = chars[i + 2..end.saturating_sub(1).max(i + 2)]
                                .iter()
                                .collect();
                            items.push(Item::Sub(inner));
                            i = end;
                            continue;
                        }
                        '`' => {
                            let start = i + 1;
                            let mut j = start;
                            while j < chars.len() && chars[j] != '`' {
                                if chars[j] == '\\' {
                                    j += 1;
                                }
                                j += 1;
                            }
                            items
                                .push(Item::Sub(chars[start..j.min(chars.len())].iter().collect()));
                            i = j + 1;
                            continue;
                        }
                        ch => word.push(ch),
                    }
                    i += 1;
                }
            }
            '\\' => {
                // A backslash-newline joins lines; any other escaped char is literal.
                if i + 1 < chars.len() && chars[i + 1] != '\n' {
                    in_word = true;
                    word.push(chars[i + 1]);
                }
                i += 1;
            }
            '$' if chars.get(i + 1) == Some(&'(') && chars.get(i + 2) == Some(&'(') => {
                // Arithmetic `$((...))`: data, kept as part of the word.
                in_word = true;
                i = balanced_close(&chars, i + 1);
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                // Command substitution: its contents are commands, in their own scope.
                end_cmd!();
                items.push(Item::Open);
                open_depth += 1;
                i += 2;
                continue;
            }
            '(' if !in_word && chars.get(i + 1) == Some(&'(') => {
                // Arithmetic command `(( ... ))`: data.
                end_cmd!();
                i = balanced_close(&chars, i);
                continue;
            }
            '(' => {
                end_cmd!();
                items.push(Item::Open);
                open_depth += 1;
            }
            ')' => {
                end_cmd!();
                if cases.last() == Some(&open_depth) {
                    // end of a `case` pattern
                } else {
                    items.push(Item::Close);
                    open_depth = open_depth.saturating_sub(1);
                }
            }
            '`' => {
                end_cmd!();
                if in_backtick {
                    items.push(Item::Close);
                    open_depth = open_depth.saturating_sub(1);
                } else {
                    items.push(Item::Open);
                    open_depth += 1;
                }
                in_backtick = !in_backtick;
            }
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue; // the newline ends the command below
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                if chars.get(i + 2) == Some(&'<') {
                    // `<<<` here-string: the next word is data, read as a normal word.
                    end_word!();
                    i += 3;
                    continue;
                }
                end_word!();
                i += 2;
                let strip_tabs = chars.get(i) == Some(&'-');
                if strip_tabs {
                    i += 1;
                }
                while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                    i += 1;
                }
                let mut delim = String::new();
                let mut quoted = false;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && !matches!(chars[i], ';' | '&' | '|' | ')' | '<' | '>')
                {
                    if chars[i] == '\'' || chars[i] == '"' || chars[i] == '\\' {
                        quoted = true;
                    } else {
                        delim.push(chars[i]);
                    }
                    i += 1;
                }
                if !delim.is_empty() {
                    heredocs.push(Heredoc {
                        delim,
                        strip_tabs,
                        quoted,
                        cmd: cmd_count,
                    });
                }
                continue;
            }
            '\n' => {
                end_cmd!();
                i += 1;
                // Heredoc bodies start on the next line; the terminator is the exact word
                // (leading tabs allowed with `<<-`).
                for h in std::mem::take(&mut heredocs) {
                    let mut body = String::new();
                    while i < chars.len() {
                        let start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[start..i].iter().collect();
                        i += 1;
                        let line_cmp = line.trim_end_matches('\r');
                        let line_cmp = if h.strip_tabs {
                            line_cmp.trim_start_matches('\t')
                        } else {
                            line_cmp
                        };
                        if line_cmp == h.delim {
                            break;
                        }
                        body.push_str(&line);
                        body.push('\n');
                    }
                    if !h.quoted {
                        items.extend(body_substitutions(&body).into_iter().map(Item::Sub));
                    }
                    items.push(Item::Body {
                        cmd: h.cmd,
                        text: body,
                    });
                }
                continue;
            }
            '|' if chars.get(i + 1) == Some(&'|') => {
                end_cmd!();
                i += 2;
                continue;
            }
            '|' => {
                // `|` or `|&`: a pipe into the next command.
                end_cmd!();
                items.push(Item::Pipe);
                if chars.get(i + 1) == Some(&'&') {
                    i += 1;
                }
            }
            ';' | '&' => {
                end_cmd!();
            }
            c if c.is_whitespace() => {
                end_word!();
            }
            _ => {
                in_word = true;
                word.push(c);
            }
        }
        i += 1;
    }
    if in_word {
        words.push(word);
    }
    if !words.is_empty() {
        items.push(Item::Cmd(words));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(c: &str) -> Vec<Vec<String>> {
        git_commits(c).into_iter().map(|g| g.dirs).collect()
    }

    fn none() -> Vec<String> {
        Vec::new()
    }

    fn v(dirs: &[&str]) -> Vec<String> {
        dirs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn finds_commits() {
        assert_eq!(dirs("git commit -m x"), vec![none()]);
        assert_eq!(dirs("cd a && git commit -q -F f"), vec![v(&["a"])]);
        assert_eq!(dirs("git -C x commit"), vec![v(&["x"])]);
        assert_eq!(dirs("git --work-tree /v commit"), vec![v(&["/v"])]);
        assert_eq!(dirs("git add . && git -c user.name=v commit"), vec![none()]);
        assert_eq!(dirs("FOO=1 git commit"), vec![none()]);
        assert_eq!(
            dirs("cd ~/r && git add a\ngit commit -m 'msg; with git commit'").len(),
            1
        );
        assert_eq!(dirs("git commit --amend --no-edit"), vec![none()]);
        assert_eq!(dirs("/usr/bin/git commit"), vec![none()]);
        assert_eq!(
            dirs("git commit -m \"$(cat <<'EOF'\nmsg git commit\nEOF\n)\""),
            vec![none()]
        );
    }

    #[test]
    fn finds_commits_behind_keywords_and_wrappers() {
        for c in [
            "if true; then git commit -m x; fi",
            "git diff --quiet || { git commit -m x; }",
            "! git commit",
            "for f in a; do git commit; done",
            "while false; do :; done; git commit",
            "timeout 30 git commit",
            "timeout -s KILL 30s git commit",
            "sudo -u me git commit",
            "sudo git commit",
            "xargs git commit",
            "xargs -I{} git commit -m {}",
            "xargs --max-args 1 git commit",
            "env -u X FOO=1 git commit",
            "nice -n 5 git commit",
            "bash -c \"git commit -m x\"",
            "sh -c 'cd a && git commit'",
            "bash -lc 'git commit'",
            "bash -o pipefail -c 'git commit'",
            "bash -euo pipefail -c 'git commit'",
            "eval git commit -m x",
            "echo $(git commit -m x)",
            "echo `git commit -m x`",
            "out=\"$(git commit -m x)\"",
            "echo \"`git commit -m x`\"",
            "x=$((1<<2))\ngit commit -m z",
            "(( x = 1 << 2 ))\ngit commit -m z",
            "bash <<EOF\ngit commit -m x\nEOF",
            "cat <<EOF | sh\ngit commit -m x\nEOF",
            "cat <<EOF | tee log | bash\ngit commit -m x\nEOF",
            "cat > f <<EOF\n$(git commit -m x)\nEOF",
            "cat > f <<EOF\n`git commit -m x`\nEOF",
            "bash -s <<'EOF'\ngit commit -m x\nEOF",
        ] {
            assert_eq!(git_commits(c).len(), 1, "{c}");
        }
        assert_eq!(dirs("sh -c 'cd a && git commit'"), vec![v(&["a"])]);
    }

    #[test]
    fn cd_does_not_leak_out_of_subshells() {
        assert_eq!(dirs("(cd /other && ls); git commit"), vec![none()]);
        assert_eq!(dirs("X=$(cd /other && pwd); git commit"), vec![none()]);
        assert_eq!(dirs("pushd /other; popd; git commit"), vec![none()]);
        assert_eq!(dirs("(cd /other && git commit)"), vec![v(&["/other"])]);
        assert_eq!(
            dirs("(case x in a) ;; esac; cd /o); git commit"),
            vec![none()]
        );
        assert_eq!(
            dirs("case $x in (a) cd /o ;; esac; git commit"),
            vec![v(&["/o"])]
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
            "cat <<-EOF\n\tgit commit\n\tEOF",
            "cat <<EOF\n  EOF\ngit commit\nEOF",
            "grep -n 'git commit' file",
            "git log --grep commit",
            "# git commit -m x",
            "echo hi # git commit",
            "cat <<< 'git commit'",
            "bash -c 'echo git commit'",
            "bash script.sh <<EOF\ngit commit\nEOF",
            "cat > f <<'EOF'\n$(git commit -m x)\nEOF",
            "cat <<EOF | grep x\ngit commit\nEOF",
            "false || echo git commit",
        ] {
            assert!(git_commits(c).is_empty(), "{c}");
        }
    }

    #[test]
    fn quoted_heredoc_marker_is_not_a_heredoc() {
        assert_eq!(git_commits("echo \"x << y\"\ngit commit -m z").len(), 1);
        assert_eq!(git_commits("echo 'a <<EOF'\ngit commit").len(), 1);
    }

    #[test]
    fn commit_after_heredoc_counts() {
        let c = "cat > f <<'EOF'\ntext\nEOF\ngit commit -m done";
        assert_eq!(git_commits(c).len(), 1);
        let two = "cat <<A <<B\na\nA\nb\nB\ngit commit";
        assert_eq!(git_commits(two).len(), 1);
    }

    #[test]
    fn reports_aliases_as_subcommands() {
        let calls = git_calls("git ci -m x");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].subcommand, "ci");
        let inline = git_calls("git -c alias.x=commit x");
        assert_eq!(
            inline[0].inline_aliases,
            vec![("x".into(), "commit".into())]
        );
        assert!(!inline[0].config_overridden);
        let upper = git_calls("git -c ALIAS.X=commit x");
        assert_eq!(upper[0].inline_aliases, vec![("x".into(), "commit".into())]);
        for c in [
            "GIT_CONFIG_COUNT=1 git x",
            "export GIT_CONFIG_COUNT=1; git x",
            "git --config-env=alias.x=V x",
            "git -c include.path=/tmp/c x",
            "HOME=/tmp git x",
            "XDG_CONFIG_HOME=/tmp git x",
            "PATH=/tmp:$PATH git x",
            "GIT_EXEC_PATH=/tmp git x",
            "git --exec-path=/tmp x",
        ] {
            assert!(git_calls(c)[0].config_overridden, "{c}");
        }
    }
}
