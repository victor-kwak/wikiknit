//! Just enough shell parsing to find the git commands a Bash command would run, the
//! directory and repository each one runs in, and the files the command may write.
//!
//! Data is not a command: heredoc bodies, quoted text, comments, and arithmetic
//! (`cat <<EOF ... git commit ... EOF`, `echo "git commit"`, `$((1<<2))`, `(( x = 1 << 2 ))`).
//! Commands are found behind shell keywords (`if ...; then git commit; fi`), wrappers (`sudo`,
//! `timeout`, `xargs`, `env`, `time -p`, ...), redirections (`2>/dev/null git commit`),
//! `bash -c '...'`, `eval`, `find -exec`, command substitution (also inside double quotes),
//! and scripts fed to a shell (`bash <<EOF`, `bash <<< '...'`, `echo '...' | sh`,
//! `source <(echo ...)`). A script whose text cannot be seen (`curl ... | sh`), a program
//! word that is not a literal (`$G commit`), or nesting too deep to follow is reported as
//! an *opaque* call: it may commit anywhere. `cd` and `export` inside a subshell or
//! substitution do not leak out of it.

use std::collections::BTreeMap;

/// A git invocation found in a command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitCall {
    /// Directory changes before it, in order (`cd a && cd b` -> `["a", "b"]`), then
    /// `env -C`/`sudo -D` values, then `-C` values.
    pub dirs: Vec<String>,
    /// `--git-dir` or an exported `GIT_DIR` (relative to `dirs`).
    pub git_dir: Option<String>,
    /// `--work-tree` or an exported `GIT_WORK_TREE` (relative to `dirs`).
    pub work_tree: Option<String>,
    /// The subcommand (`commit`, `add`, or an alias such as `ci`).
    pub subcommand: String,
    /// Arguments after the subcommand.
    pub args: Vec<String>,
    /// Aliases defined on the command line: `git -c alias.ci=commit ci`.
    pub inline_aliases: Vec<(String, String)>,
    /// Config or command lookup is overridden for this call (`-c` other than `alias.*`,
    /// `--config-env`, `--exec-path`, or `GIT_CONFIG*`, `GIT_DIR`, `GIT_COMMON_DIR`,
    /// `GIT_EXEC_PATH`, `HOME`, `XDG_CONFIG_HOME`, `PATH` set in the command), so an alias
    /// or external command cannot be resolved reliably.
    pub config_overridden: bool,
    /// What runs cannot be seen (`$G commit`, `curl ... | sh`): it may commit, in a
    /// repository that cannot be known.
    pub opaque: bool,
}

/// Where a command other than git may write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteTarget {
    /// These paths, as written (relative to `dirs`, the directory changes before the command).
    Paths {
        dirs: Vec<String>,
        paths: Vec<String>,
    },
    /// Cannot tell: a program wikiknit does not know, a script.
    Unknown,
}

/// A command that may write files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    /// The command, shortened, for messages.
    pub what: String,
    pub target: WriteTarget,
}

/// What a command would run and write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Every git invocation (and opaque call).
    pub calls: Vec<GitCall>,
    /// Possible file writes by commands other than git (redirections, `tee`, `cp`, unknown
    /// programs). Git's own work-tree changes are judged from `calls`.
    pub writes: Vec<Write>,
}

/// The git calls and possible writes of a Bash command.
#[must_use]
pub fn analyze(command: &str) -> Analysis {
    let mut out = Analysis::default();
    let mut ctx = Ctx::default();
    collect(command, &mut ctx, &mut out, 0);
    out
}

/// Every git invocation the command would run.
#[must_use]
pub fn git_calls(command: &str) -> Vec<GitCall> {
    analyze(command).calls
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

/// Shell state that later commands see.
#[derive(Debug, Clone, Default)]
struct Ctx {
    cds: Vec<String>,
    /// Exported `GIT_DIR` / `GIT_WORK_TREE`.
    git_dir: Option<String>,
    work_tree: Option<String>,
    /// An earlier command set a variable that changes git's config or command lookup.
    config: bool,
    /// Shell variables set without `export` (an `export NAME` later exports them).
    vars: BTreeMap<String, String>,
}

impl Ctx {
    fn export(&mut self, k: &str, v: &str) {
        match k {
            "GIT_DIR" => self.git_dir = Some(v.to_string()),
            "GIT_WORK_TREE" => self.work_tree = Some(v.to_string()),
            _ => {}
        }
        if is_config_var(k) {
            self.config = true;
        }
    }

    fn unset(&mut self, k: &str) {
        match k {
            "GIT_DIR" => self.git_dir = None,
            "GIT_WORK_TREE" => self.work_tree = None,
            _ => {}
        }
        self.vars.remove(k);
    }
}

/// Variables that change where git finds config or commands.
fn is_config_var(k: &str) -> bool {
    k.starts_with("GIT_CONFIG")
        || matches!(
            k,
            "GIT_DIR" | "GIT_COMMON_DIR" | "GIT_EXEC_PATH" | "HOME" | "XDG_CONFIG_HOME" | "PATH"
        )
}

/// `NAME=value` or `NAME+=value` -> (`NAME`, `value`).
fn split_assignment(w: &str) -> Option<(&str, &str)> {
    let (k, v) = w.split_once('=')?;
    let k = k.strip_suffix('+').unwrap_or(k);
    let ok = !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit());
    ok.then_some((k, v))
}

/// A call that cannot be seen into: it may commit anywhere and write anything.
fn opaque(out: &mut Analysis, ctx: &Ctx, what: &str) {
    out.calls.push(GitCall {
        dirs: ctx.cds.clone(),
        subcommand: "commit".into(),
        config_overridden: true,
        opaque: true,
        ..GitCall::default()
    });
    out.writes.push(Write {
        what: what.to_string(),
        target: WriteTarget::Unknown,
    });
}

fn shorten(words: &[String]) -> String {
    let s = words.join(" ");
    if s.chars().count() > 60 {
        let mut t: String = s.chars().take(57).collect();
        t.push_str("...");
        t
    } else {
        s
    }
}

fn collect(command: &str, ctx: &mut Ctx, out: &mut Analysis, depth: usize) {
    if depth > MAX_DEPTH {
        opaque(out, ctx, "commands nested too deep to follow");
        return;
    }
    // shell state at each open subshell / substitution
    let mut scopes: Vec<Ctx> = Vec::new();
    // every simple command so far (heredoc bodies and pipes refer to commands by id)
    let mut commands: Vec<Cmd> = Vec::new();
    let mut bodies: Vec<(usize, String, Ctx)> = Vec::new();
    for item in tokenize(command) {
        match item {
            Item::Open => scopes.push(ctx.clone()),
            Item::Close => {
                if let Some(saved) = scopes.pop() {
                    *ctx = saved;
                }
            }
            Item::Sub(script) => {
                let mut inner = ctx.clone();
                collect(&script, &mut inner, out, depth + 1);
            }
            // Judged once every command is known: the shell it is piped into may come later
            // (`(cat <<EOF ... EOF\n) | sh`).
            Item::Body { cmd, text } => bodies.push((cmd, text, ctx.clone())),
            Item::Cmd(cmd) => {
                let feed = stdin_feed(&commands, &cmd);
                simple(&cmd.words, &cmd.redirects, &feed, ctx, out, depth);
                commands.push(cmd);
            }
        }
    }
    // A heredoc body is a script when its command, or a command it is piped into
    // (`cat <<EOF | sh`), is a shell reading stdin.
    for (cmd, text, mut inner) in bodies {
        let mut j = cmd;
        let mut feeds_shell = false;
        while let Some(c) = commands.iter().find(|c| c.id == j) {
            if reads_stdin_shell(&c.words) {
                feeds_shell = true;
                break;
            }
            match commands.iter().find(|n| n.stdin_from.contains(&j)) {
                Some(next) => j = next.id,
                None => break,
            }
        }
        if feeds_shell {
            collect(&text, &mut inner, out, depth + 1);
        }
    }
}

fn reads_stdin_shell(words: &[String]) -> bool {
    let p = strip_prefixes(words);
    p.words
        .split_first()
        .is_some_and(|(prog, args)| SHELLS.contains(&program_name(prog)) && shell_reads_stdin(args))
}

/// What a command reads on stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Feed {
    /// Nothing visible (the agent's stdin).
    Terminal,
    /// The heredoc of the command with this id (its body is handled where it is parsed).
    Heredoc(usize),
    /// This text.
    Literal(String),
    /// A file.
    File,
    /// The output of a program that cannot be seen into.
    Opaque,
}

/// The stdin of `cmd`; `commands` are the commands before it.
fn stdin_feed(commands: &[Cmd], cmd: &Cmd) -> Feed {
    // The last redirection of fd 0 wins.
    for r in cmd.redirects.iter().rev() {
        let digits = r.op.len() - r.op.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let (fd, op) = r.op.split_at(digits);
        if !(fd.is_empty() || fd == "0") {
            continue;
        }
        match op {
            "<<" => return Feed::Heredoc(cmd.id),
            "<<<" => return Feed::Literal(r.target.clone()),
            "<" | "<>" => {
                return match process_substitution(&r.target) {
                    Some(inner) => process_output(inner),
                    None => Feed::File,
                };
            }
            _ => {}
        }
    }
    let feeds: Vec<Feed> = cmd
        .stdin_from
        .iter()
        .map(|from| {
            commands
                .iter()
                .find(|c| c.id == *from)
                .map_or(Feed::Opaque, |producer| output_of(commands, producer))
        })
        .collect();
    match feeds.len() {
        0 => Feed::Terminal,
        1 => feeds.into_iter().next().unwrap_or(Feed::Opaque),
        // A group piped in: its literal output in order; anything unreadable makes it opaque
        // (heredoc bodies are parsed where they appear).
        _ => {
            let mut text = Vec::new();
            for f in feeds {
                match f {
                    Feed::Literal(t) => text.push(t),
                    Feed::Heredoc(_) | Feed::Terminal => {}
                    Feed::File | Feed::Opaque => return Feed::Opaque,
                }
            }
            Feed::Literal(text.join("\n"))
        }
    }
}

/// What `cmd` writes to stdout.
fn output_of(commands: &[Cmd], cmd: &Cmd) -> Feed {
    let p = strip_prefixes(&cmd.words);
    let Some((program, args)) = p.words.split_first() else {
        return Feed::Terminal;
    };
    match program_name(program) {
        "echo" => {
            let args: Vec<&String> = args
                .iter()
                .skip_while(|a| {
                    a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| "neE".contains(c))
                })
                .collect();
            Feed::Literal(unescape(
                &args
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
            ))
        }
        "printf" => Feed::Literal(unescape(&args.join(" "))),
        // Pass stdin through.
        "cat" if args.iter().all(|a| a.len() > 1 && a.starts_with('-')) => {
            stdin_feed(commands, cmd)
        }
        "tee" => stdin_feed(commands, cmd),
        _ => Feed::Opaque,
    }
}

/// `\n` and `\t` as `echo -e` and `printf` print them.
fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\t", "\t")
}

/// `<(cmd)` -> `cmd`.
fn process_substitution(word: &str) -> Option<&str> {
    word.strip_prefix("<(")?.strip_suffix(')')
}

/// What the commands of a process substitution print.
fn process_output(inner: &str) -> Feed {
    let mut cmds = Vec::new();
    let mut bodies = Vec::new();
    for item in tokenize(inner) {
        match item {
            Item::Cmd(c) => cmds.push(c),
            Item::Body { cmd, text } => bodies.push((cmd, text)),
            _ => return Feed::Opaque,
        }
    }
    let Some(last) = cmds.last() else {
        return Feed::Terminal;
    };
    match output_of(&cmds, last) {
        Feed::Heredoc(j) => bodies
            .into_iter()
            .find(|(c, _)| *c == j)
            .map_or(Feed::Opaque, |(_, t)| Feed::Literal(t)),
        f => f,
    }
}

/// Run a script whose text arrives as `feed` (a shell reading stdin, `source <(...)`).
fn run_fed(feed: &Feed, ctx: &Ctx, out: &mut Analysis, depth: usize, what: &str) {
    match feed {
        Feed::Literal(script) => {
            let mut inner = ctx.clone();
            collect(script, &mut inner, out, depth + 1);
        }
        // Heredoc bodies are parsed where they appear; nothing comes from the terminal.
        Feed::Heredoc(_) | Feed::Terminal => {}
        // A script file: like `bash script.sh`, it may write anything.
        Feed::File => out.writes.push(Write {
            what: what.to_string(),
            target: WriteTarget::Unknown,
        }),
        Feed::Opaque => opaque(out, ctx, what),
    }
}

/// The file a redirection writes, if any.
fn redirect_target(r: &Redir) -> Option<&str> {
    let op = r.op.trim_start_matches(|c: char| c.is_ascii_digit());
    match op {
        ">" | ">>" | ">|" | "&>" | "&>>" | "<>" => {}
        // `>&2`, `>&-` duplicate a descriptor; `>&file` writes the file.
        ">&" if r.target == "-" || r.target.chars().all(|c| c.is_ascii_digit()) => return None,
        ">&" => {}
        _ => return None,
    }
    // `>(cmd)`: its commands are parsed where they appear.
    if r.target.starts_with(">(") {
        return None;
    }
    Some(&r.target)
}

/// Programs that do not write files (beyond their redirections).
const READ_ONLY: &[&str] = &[
    "[",
    "[[",
    ":",
    "ag",
    "b2sum",
    "basename",
    "case",
    "cat",
    "cd",
    "cksum",
    "cmp",
    "column",
    "comm",
    "cut",
    "date",
    "declare",
    "df",
    "diff",
    "dirname",
    "done",
    "du",
    "echo",
    "egrep",
    "esac",
    "exit",
    "export",
    "false",
    "fgrep",
    "fi",
    "file",
    "fold",
    "for",
    "free",
    "function",
    "git",
    "grep",
    "hash",
    "head",
    "hexdump",
    "hostname",
    "id",
    "in",
    "jq",
    "less",
    "let",
    "local",
    "ls",
    "md5sum",
    "nl",
    "nproc",
    "od",
    "paste",
    "pgrep",
    "popd",
    "printenv",
    "printf",
    "ps",
    "pushd",
    "pwd",
    "read",
    "readlink",
    "readonly",
    "realpath",
    "return",
    "rg",
    "select",
    "seq",
    "set",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "shift",
    "shopt",
    "sleep",
    "stat",
    "tail",
    "test",
    "tr",
    "tree",
    "true",
    "type",
    "typeset",
    "uname",
    "uniq",
    "unset",
    "wait",
    "wc",
    "which",
    "whoami",
    "wikiknit",
    "yes",
    "}",
];

/// Arguments that are not options.
fn operands(args: &[String]) -> Vec<String> {
    args.iter()
        .filter(|a| !a.starts_with('-') || a.as_str() == "-")
        .cloned()
        .collect()
}

/// Where a program other than git may write: `None` for no files, `Some(paths)` for these
/// paths, `Some([])` with `unknown` set when it cannot be told.
enum Effect {
    None,
    Paths(Vec<String>),
    Unknown,
}

fn program_effect(name: &str, args: &[String]) -> Effect {
    if READ_ONLY.contains(&name) {
        return Effect::None;
    }
    match name {
        "sed" => {
            let in_place = args.iter().any(|a| {
                a.starts_with("--in-place")
                    || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
            });
            if in_place {
                Effect::Paths(operands(args))
            } else {
                Effect::None
            }
        }
        "awk" | "gawk" | "mawk" | "nawk" => {
            let writes = args.iter().any(|a| {
                a.contains('>') || a.contains("system") || a.contains("inplace") || a == "-i"
            });
            if writes {
                Effect::Unknown
            } else {
                Effect::None
            }
        }
        "sort" => {
            let mut paths = Vec::new();
            let mut it = args.iter();
            while let Some(a) = it.next() {
                if a == "-o" || a == "--output" {
                    paths.extend(it.next().cloned());
                } else if let Some(p) = a.strip_prefix("--output=") {
                    paths.push(p.to_string());
                } else if let Some(p) = a.strip_prefix("-o") {
                    paths.push(p.to_string());
                }
            }
            if paths.is_empty() {
                Effect::None
            } else {
                Effect::Paths(paths)
            }
        }
        "dd" => Effect::Paths(
            args.iter()
                .filter_map(|a| a.strip_prefix("of="))
                .map(str::to_string)
                .collect(),
        ),
        "tee" | "cp" | "mv" | "install" | "ln" | "link" | "rm" | "unlink" | "rmdir" | "mkdir"
        | "touch" | "truncate" | "shred" | "mktemp" => {
            let mut paths = operands(args);
            paths.extend(
                args.iter()
                    .filter_map(|a| a.strip_prefix("--target-directory="))
                    .map(str::to_string),
            );
            Effect::Paths(paths)
        }
        _ => Effect::Unknown,
    }
}

/// Handle one simple command.
#[allow(clippy::too_many_lines)] // one dispatch over the programs that matter
fn simple(
    words: &[String],
    redirects: &[Redir],
    feed: &Feed,
    ctx: &mut Ctx,
    out: &mut Analysis,
    depth: usize,
) {
    let p = strip_prefixes(words);
    if p.not_run {
        return;
    }
    let what = shorten(words);
    let write = |out: &mut Analysis, dirs: &[String], paths: Vec<String>, xargs: bool| {
        out.writes.push(Write {
            what: what.clone(),
            target: if xargs {
                WriteTarget::Unknown
            } else {
                WriteTarget::Paths {
                    dirs: dirs.to_vec(),
                    paths,
                }
            },
        });
    };
    // Redirections (in `[[ ... ]]`, `<` and `>` compare strings).
    if p.words.first().map(String::as_str) != Some("[[") {
        let paths: Vec<String> = redirects
            .iter()
            .filter_map(redirect_target)
            .map(str::to_string)
            .collect();
        if !paths.is_empty() {
            write(out, &ctx.cds, paths, false);
        }
    }
    let Some((program, args)) = p.words.split_first() else {
        // Only assignments: shell variables, not exported.
        for (k, v) in &p.assigns {
            if is_config_var(k) {
                ctx.config = true;
            }
            ctx.vars.insert(k.clone(), v.clone());
        }
        return;
    };
    let mut dirs = ctx.cds.clone();
    dirs.extend(p.dirs.iter().cloned());
    let name = program_name(program);
    // A program word that is not a literal (`$G`, `$(which git)`), or a wrapper option this
    // parser does not know: it may be git, doing anything, in a repository that cannot be
    // known.
    if is_dynamic_word(program) || p.unparsed {
        let mut call = parse_git(args, &dirs, None, None, true).unwrap_or_else(|| GitCall {
            dirs: dirs.clone(),
            ..GitCall::default()
        });
        call.subcommand = "commit".into();
        call.config_overridden = true;
        call.opaque = true;
        out.calls.push(call);
        out.writes.push(Write {
            what,
            target: WriteTarget::Unknown,
        });
        return;
    }
    match name {
        "export" | "declare" | "typeset" | "readonly" | "local" => {
            let exported =
                name == "export" || args.iter().any(|a| a.starts_with('-') && a.contains('x'));
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                if let Some((k, v)) = split_assignment(a) {
                    if is_config_var(k) {
                        ctx.config = true;
                    }
                    if exported {
                        ctx.export(k, v);
                    } else {
                        ctx.vars.insert(k.to_string(), v.to_string());
                    }
                } else if exported && let Some(v) = ctx.vars.get(a.as_str()).cloned() {
                    ctx.export(a, &v);
                }
            }
        }
        "unset" => {
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                ctx.unset(a);
            }
        }
        "cd" | "pushd" => {
            if let Some(dir) = args.iter().find(|a| !a.starts_with('-')) {
                ctx.cds.push(dir.clone());
            }
        }
        "popd" => {
            ctx.cds.pop();
        }
        name if SHELLS.contains(&name) => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            if let Some(script) = shell_c_script(args) {
                collect(script, &mut inner, out, depth + 1);
            } else if shell_reads_stdin(args) {
                // `bash $S`: the word may be a script file, or expand to nothing.
                if script_operand(args).is_some_and(is_dynamic_word) {
                    opaque(out, &inner, &what);
                }
                run_fed(feed, &inner, out, depth, &what);
            } else {
                run_script_arg(args, feed, &inner, out, depth, &what);
            }
        }
        "source" | "." => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            run_script_arg(args, feed, &inner, out, depth, &what);
            // A sourced script runs in this shell: its `cd` and `export` are unknown.
        }
        "eval" => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            collect(&args.join(" "), &mut inner, out, depth + 1);
        }
        "git" => {
            let mut config = ctx.config;
            let mut git_dir = ctx.git_dir.clone();
            let mut work_tree = ctx.work_tree.clone();
            for u in &p.unsets {
                match u.as_str() {
                    "GIT_DIR" => git_dir = None,
                    "GIT_WORK_TREE" => work_tree = None,
                    _ => {}
                }
            }
            for (k, v) in &p.assigns {
                config |= is_config_var(k);
                match k.as_str() {
                    "GIT_DIR" => git_dir = Some(v.clone()),
                    "GIT_WORK_TREE" => work_tree = Some(v.clone()),
                    _ => {}
                }
            }
            if let Some(call) = parse_git(args, &dirs, git_dir, work_tree, config) {
                out.calls.push(call);
            }
        }
        "find" => {
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "-exec" | "-execdir" | "-ok" | "-okdir" => {
                        let start = i + 1;
                        let mut j = start;
                        while j < args.len() && args[j] != ";" && args[j] != "+" {
                            j += 1;
                        }
                        let mut inner = ctx.clone();
                        inner.cds = dirs.clone();
                        simple(
                            &args[start..j],
                            &[],
                            &Feed::Terminal,
                            &mut inner,
                            out,
                            depth + 1,
                        );
                        i = j + 1;
                    }
                    "-delete" | "-fprint" | "-fprint0" | "-fprintf" | "-fls" => {
                        out.writes.push(Write {
                            what: what.clone(),
                            target: WriteTarget::Unknown,
                        });
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
        }
        _ => match program_effect(name, args) {
            Effect::None => {}
            Effect::Paths(paths) => write(out, &dirs, paths, p.xargs),
            Effect::Unknown => out.writes.push(Write {
                what,
                target: WriteTarget::Unknown,
            }),
        },
    }
}

/// `bash FILE` / `source FILE`: a process substitution or `/dev/stdin` is parsed; a script
/// file may write anything (its commits are not seen, as for any program).
fn run_script_arg(
    args: &[String],
    feed: &Feed,
    ctx: &Ctx,
    out: &mut Analysis,
    depth: usize,
    what: &str,
) {
    let mut it = args.iter();
    let mut file = None;
    while let Some(a) = it.next() {
        if shell_opt_takes_arg(a) {
            it.next();
            continue;
        }
        if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            continue;
        }
        file = Some(a.as_str());
        break;
    }
    match file {
        Some("/dev/stdin" | "-") => run_fed(feed, ctx, out, depth, what),
        Some(f) if is_dynamic_word(f) => opaque(out, ctx, what),
        Some(f) => match process_substitution(f) {
            Some(inner) => run_fed(&process_output(inner), ctx, out, depth, what),
            None => out.writes.push(Write {
                what: what.to_string(),
                target: WriteTarget::Unknown,
            }),
        },
        None => {}
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

/// `bash` with only options reads its script from stdin (a heredoc, a pipe).
/// `bash` with only options, or with `-s` (later words are arguments), reads its script from
/// stdin (a heredoc, a pipe). So may `bash $X`: the word can expand to nothing.
fn shell_reads_stdin(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let a = a.as_str();
        if shell_opt_takes_arg(a) {
            it.next();
            continue;
        }
        if a.starts_with('-') || a.starts_with('+') {
            let short = a.len() > 1 && !a.starts_with("--");
            if short && a[1..].contains('c') {
                return false;
            }
            if short && a.starts_with('-') && a[1..].contains('s') {
                return true;
            }
            if short && a[1..].contains(['o', 'O']) {
                it.next();
            }
            continue;
        }
        return is_dynamic_word(a);
    }
    true
}

/// The first word of a shell's arguments that is not an option (its script file).
fn script_operand(args: &[String]) -> Option<&str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if shell_opt_takes_arg(a) {
            it.next();
            continue;
        }
        if a.len() > 1 && (a.starts_with('-') || a.starts_with('+')) {
            continue;
        }
        return Some(a);
    }
    None
}

/// A word the shell expands at run time into something this parser cannot see.
fn is_dynamic_word(w: &str) -> bool {
    w.contains(['$', '`'])
}

fn parse_git(
    args: &[String],
    dirs: &[String],
    git_dir: Option<String>,
    work_tree: Option<String>,
    config_overridden: bool,
) -> Option<GitCall> {
    let mut call = GitCall {
        dirs: dirs.to_vec(),
        git_dir,
        work_tree,
        config_overridden,
        ..GitCall::default()
    };
    let mut it = args.iter();
    while let Some(t) = it.next() {
        match t.as_str() {
            "-C" => call.dirs.push(it.next()?.clone()),
            "--git-dir" => call.git_dir = Some(it.next()?.clone()),
            "--work-tree" => call.work_tree = Some(it.next()?.clone()),
            "-c" => {
                let kv = it.next()?;
                // Config keys are case-insensitive: `-c ALIAS.X=commit` defines alias `x`.
                let (k, v) = kv.split_once('=').unwrap_or((kv.as_str(), "true"));
                let k = k.to_ascii_lowercase();
                if let Some(name) = k.strip_prefix("alias.") {
                    call.inline_aliases.push((name.to_string(), v.to_string()));
                } else {
                    call.config_overridden = true; // include.path, includeIf, ...
                }
            }
            "--config-env" | "--exec-path" => {
                it.next();
                call.config_overridden = true;
            }
            s if s.starts_with("--config-env=") || s.starts_with("--exec-path") => {
                call.config_overridden = true;
            }
            "--namespace" | "--super-prefix" | "--list-cmds" => {
                it.next();
            }
            s if s.starts_with("--work-tree=") => {
                call.work_tree = Some(s["--work-tree=".len()..].to_string());
            }
            s if s.starts_with("--git-dir=") => {
                call.git_dir = Some(s["--git-dir=".len()..].to_string());
            }
            s if s.starts_with('-') => {}
            sub => {
                call.subcommand = sub.to_string();
                call.args = it.cloned().collect();
                return Some(call);
            }
        }
    }
    None
}

/// Options (of a wrapper) that take a separate argument.
fn wrapper_arg_opts(wrapper: &str) -> &'static [&'static str] {
    match wrapper {
        "sudo" => &["-u", "-g", "-h", "-p", "-C", "-r", "-t", "-U", "-T", "-R"],
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
        "doas" => &["-u", "-C"],
        _ => &[],
    }
}

/// A command with the prefixes in front of its program taken off.
#[derive(Debug, Default)]
struct Prefixed {
    /// The program and its arguments.
    words: Vec<String>,
    /// `NAME=value` in front of it (also after `env`).
    assigns: Vec<(String, String)>,
    /// `env -u NAME`.
    unsets: Vec<String>,
    /// `env -C DIR`, `sudo -D DIR`.
    dirs: Vec<String>,
    /// Run by `xargs`: more arguments come from stdin.
    xargs: bool,
    /// Not run at all (`command -v git`).
    not_run: bool,
    /// A wrapper option this parser does not know: what runs cannot be told.
    unparsed: bool,
}

/// Drop shell keywords, env assignments, and wrapper commands (with their options)
/// in front of the real program.
fn strip_prefixes(words: &[String]) -> Prefixed {
    let mut w: Vec<String> = words.to_vec();
    let mut p = Prefixed::default();
    let mut i = 0;
    while i < w.len() {
        let t = w[i].clone();
        if let Some((k, v)) = split_assignment(&t) {
            p.assigns.push((k.to_string(), v.to_string()));
            i += 1;
            continue;
        }
        match t.as_str() {
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "{" | "}" | "!"
            | "coproc" | "nohup" | "builtin" => {
                i += 1;
                continue;
            }
            "time" => {
                i += 1;
                while i < w.len() && w[i].starts_with('-') {
                    i += 1;
                    if w[i - 1] == "--" {
                        break;
                    }
                }
                continue;
            }
            "command" => {
                i += 1;
                while i < w.len() && w[i].starts_with('-') {
                    if w[i].contains(['v', 'V']) {
                        p.not_run = true;
                    }
                    i += 1;
                }
                continue;
            }
            "exec" => {
                i += 1;
                while i < w.len() && w[i].starts_with('-') {
                    i += if w[i] == "-a" { 2 } else { 1 };
                }
                continue;
            }
            _ => {}
        }
        let name = program_name(&t).to_string();
        if !matches!(
            name.as_str(),
            "sudo"
                | "doas"
                | "timeout"
                | "nice"
                | "ionice"
                | "xargs"
                | "stdbuf"
                | "env"
                | "chronic"
                | "setsid"
                | "unbuffer"
        ) {
            break;
        }
        p.xargs |= name == "xargs";
        let arg_opts = wrapper_arg_opts(&name);
        i += 1;
        if name == "env" {
            i = env_options(&mut w, i, &mut p);
            continue;
        }
        while i < w.len() && w[i].starts_with('-') {
            let opt = w[i].clone();
            i += 1;
            if opt == "--" {
                break;
            }
            let value = |i: &mut usize| {
                let v = w.get(*i).cloned();
                *i += 1;
                v
            };
            match (name.as_str(), opt.as_str()) {
                ("sudo", "-D" | "--chdir") => p.dirs.extend(value(&mut i)),
                ("sudo", o) if o.starts_with("--chdir=") => {
                    p.dirs.push(o["--chdir=".len()..].to_string());
                }
                _ if arg_opts.contains(&opt.as_str()) => i += 1,
                _ => {}
            }
        }
        // timeout DURATION
        if name == "timeout" && i < w.len() {
            i += 1;
        }
    }
    p.words = w.split_off(i.min(w.len()));
    p
}

/// Parse `env`'s options from `w[i]` on (GNU coreutils `env`); returns the index after them.
/// `-S STRING` splits STRING into words in place. An unknown option sets `unparsed`.
fn env_options(w: &mut Vec<String>, mut i: usize, p: &mut Prefixed) -> usize {
    while i < w.len() && w[i].starts_with('-') {
        let opt = w[i].clone();
        i += 1;
        if opt == "--" {
            break;
        }
        if opt == "-" {
            continue; // same as -i
        }
        // (option, value) for the options that take one
        let mut take: Option<(char, String)> = None;
        if let Some(long) = opt.strip_prefix("--") {
            let (k, v) = match long.split_once('=') {
                Some((k, v)) => (k, Some(v.to_string())),
                None => (long, None),
            };
            let short = match k {
                "ignore-environment"
                | "null"
                | "debug"
                | "block-signal"
                | "default-signal"
                | "ignore-signal"
                | "list-signal-handling" => continue,
                "help" | "version" => {
                    p.not_run = true;
                    continue;
                }
                "unset" => 'u',
                "chdir" => 'C',
                "split-string" => 'S',
                _ => {
                    p.unparsed = true;
                    return i;
                }
            };
            let v = match v {
                Some(v) => v,
                None => {
                    i += 1;
                    w.get(i - 1).cloned().unwrap_or_default()
                }
            };
            take = Some((short, v));
        } else {
            let chars: Vec<char> = opt[1..].chars().collect();
            for (k, c) in chars.iter().enumerate() {
                match c {
                    'i' | '0' | 'v' => {}
                    'u' | 'C' | 'S' => {
                        let rest: String = chars[k + 1..].iter().collect();
                        let v = if rest.is_empty() {
                            i += 1;
                            w.get(i - 1).cloned().unwrap_or_default()
                        } else {
                            rest
                        };
                        take = Some((*c, v));
                        break;
                    }
                    _ => {
                        p.unparsed = true;
                        return i;
                    }
                }
            }
        }
        match take {
            Some(('u', v)) => p.unsets.push(v),
            Some(('C', v)) => p.dirs.push(v),
            Some((_, v)) => {
                // `-S 'git commit -m x'`: split as the shell would split words.
                let split: Vec<String> = tokenize(&v)
                    .into_iter()
                    .flat_map(|item| match item {
                        Item::Cmd(c) => c.words,
                        _ => Vec::new(),
                    })
                    .collect();
                w.splice(i..i, split);
            }
            None => {}
        }
    }
    i
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Redir {
    /// `>`, `>>`, `2>`, `&>`, `>&`, `<`, `<<`, `<<<`, ...
    op: String,
    /// The word after it (the delimiter for `<<`).
    target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Cmd {
    /// Unique in one `tokenize`, given when the command starts (before any substitution
    /// inside it runs), so heredocs and pipes point at the right command.
    id: usize,
    /// Words with quotes removed; redirections taken out.
    words: Vec<String>,
    redirects: Vec<Redir>,
    /// Commands whose output is piped into this one (several for `( ... ) |`, `{ ...; } |`).
    stdin_from: Vec<usize>,
}

/// An open `( ... )` or `{ ...; }`: what is piped into it, and the commands inside it.
struct Group {
    stdin: Vec<usize>,
    ids: Vec<usize>,
    paren: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    /// A simple command.
    Cmd(Cmd),
    /// Start of a subshell or command substitution.
    Open,
    /// End of one.
    Close,
    /// A command substitution found inside double quotes.
    Sub(String),
    /// A heredoc body, fed to the command with this id.
    Body { cmd: usize, text: String },
}

struct Heredoc {
    delim: String,
    strip_tabs: bool,
    /// The delimiter was quoted (`<<'EOF'`): the body is literal. Unquoted bodies expand
    /// `$(...)` and backticks, which run commands.
    quoted: bool,
    cmd: usize,
}

/// The command being built when a `$(`, `<(`, `>(`, or backtick opened; it resumes at the
/// matching close, with the substitution's text as part of the current word.
struct Saved {
    words: Vec<String>,
    word: String,
    redirects: Vec<Redir>,
    pending: Option<String>,
    cur_id: Option<usize>,
    pipe_from: Vec<usize>,
    groups: Vec<Group>,
    start: usize,
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

#[allow(clippy::too_many_lines, unused_assignments)] // one state machine; splitting it hides the state
fn tokenize(command: &str) -> Vec<Item> {
    let chars: Vec<char> = command.chars().collect();
    let mut items = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut redirects: Vec<Redir> = Vec::new();
    // A redirection operator waiting for its target word.
    let mut pending: Option<String> = None;
    let mut word = String::new();
    let mut in_word = false;
    // Ids: the command being built has one once a heredoc names it (else at its end).
    let mut next_id = 0usize;
    let mut cur_id: Option<usize> = None;
    // Producers piped into the next command; groups open at this level; the commands of the
    // group that just closed (a pipe after `)` or `}` reads all of them).
    let mut pipe_from: Vec<usize> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut last_group: Option<Vec<usize>> = None;
    let mut emitted: Option<usize> = None;
    let mut heredocs: Vec<Heredoc> = Vec::new();
    let mut in_backtick = false;
    let mut i = 0;

    // Each open `(` (None) or `$(`/`<(`/`>(`/backtick (the command it interrupts), and the
    // nesting depth at each open `case`: inside a `case`, a `)` at the same depth ends a
    // pattern (`a) ...;;`), not a subshell.
    let mut opens: Vec<Option<Saved>> = Vec::new();
    let mut cases: Vec<usize> = Vec::new();
    macro_rules! end_word {
        () => {
            if in_word {
                let w = std::mem::take(&mut word);
                in_word = false;
                if let Some(op) = pending.take() {
                    redirects.push(Redir { op, target: w });
                } else {
                    if words.is_empty() && !w.is_empty() {
                        match w.as_str() {
                            "case" => cases.push(opens.len()),
                            "esac" => {
                                cases.pop();
                            }
                            _ => {}
                        }
                    }
                    words.push(w);
                }
            }
        };
    }
    macro_rules! end_cmd {
        () => {
            end_word!();
            pending = None;
            emitted = None;
            let closes_brace =
                words.len() == 1 && words[0] == "}" && groups.last().is_some_and(|g| !g.paren);
            let mut closed = None;
            if closes_brace {
                closed = groups.pop().map(|g| g.ids);
                words.clear();
            }
            if !words.is_empty() || !redirects.is_empty() {
                let id = cur_id.take().unwrap_or_else(|| {
                    next_id += 1;
                    next_id - 1
                });
                let mut stdin_from = std::mem::take(&mut pipe_from);
                if stdin_from.is_empty()
                    && let Some(g) = groups.last()
                {
                    stdin_from = g.stdin.clone();
                }
                for g in &mut groups {
                    g.ids.push(id);
                }
                if words.first().is_some_and(|w| w == "{") {
                    groups.push(Group {
                        stdin: stdin_from.clone(),
                        ids: vec![id],
                        paren: false,
                    });
                }
                items.push(Item::Cmd(Cmd {
                    id,
                    words: std::mem::take(&mut words),
                    redirects: std::mem::take(&mut redirects),
                    stdin_from,
                }));
                emitted = Some(id);
                last_group = None;
            }
            cur_id = None;
            if closed.is_some() {
                last_group = closed;
            }
        };
    }
    // Start a substitution at `start`: the current command waits until it closes.
    macro_rules! open_sub {
        ($start:expr) => {
            if !in_word {
                word.clear();
            }
            opens.push(Some(Saved {
                words: std::mem::take(&mut words),
                word: std::mem::take(&mut word),
                redirects: std::mem::take(&mut redirects),
                pending: pending.take(),
                cur_id: cur_id.take(),
                pipe_from: std::mem::take(&mut pipe_from),
                groups: std::mem::take(&mut groups),
                start: $start,
            }));
            in_word = false;
            items.push(Item::Open);
        };
    }
    // Close the innermost `(` or substitution at `end` (the closing char).
    macro_rules! close {
        ($end:expr) => {
            end_cmd!();
            items.push(Item::Close);
            match opens.pop() {
                Some(Some(s)) => {
                    words = s.words;
                    word = s.word;
                    redirects = s.redirects;
                    pending = s.pending;
                    cur_id = s.cur_id;
                    pipe_from = s.pipe_from;
                    groups = s.groups;
                    last_group = None;
                    in_word = true;
                    let end = ($end).min(chars.len().saturating_sub(1));
                    word.extend(chars[s.start..=end].iter());
                }
                Some(None) => {
                    // A subshell: a pipe after it reads every command in it.
                    while let Some(g) = groups.pop() {
                        if g.paren {
                            last_group = Some(g.ids);
                            break;
                        }
                    }
                }
                None => {}
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
                            word.extend(chars[i..end].iter());
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
                            let j = j.min(chars.len());
                            items.push(Item::Sub(chars[start..j].iter().collect()));
                            word.extend(chars[i..(j + 1).min(chars.len())].iter());
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
                open_sub!(i);
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
                groups.push(Group {
                    stdin: std::mem::take(&mut pipe_from),
                    ids: Vec::new(),
                    paren: true,
                });
                items.push(Item::Open);
                opens.push(None);
            }
            ')' => {
                if cases.last() == Some(&opens.len()) {
                    end_cmd!(); // end of a `case` pattern
                } else {
                    close!(i);
                }
            }
            '`' => {
                if in_backtick {
                    close!(i);
                } else {
                    open_sub!(i);
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
                end_word!();
                if chars.get(i + 2) == Some(&'<') {
                    // `<<<` here-string: the next word is its text.
                    pending = Some("<<<".into());
                    i += 3;
                    continue;
                }
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
                    let id = *cur_id.get_or_insert_with(|| {
                        next_id += 1;
                        next_id - 1
                    });
                    redirects.push(Redir {
                        op: "<<".into(),
                        target: delim.clone(),
                    });
                    heredocs.push(Heredoc {
                        delim,
                        strip_tabs,
                        quoted,
                        cmd: id,
                    });
                }
                continue;
            }
            '<' | '>' if chars.get(i + 1) == Some(&'(') => {
                // Process substitution `<(cmd)` / `>(cmd)`: commands, and a word.
                open_sub!(i);
                i += 2;
                continue;
            }
            '<' | '>' => {
                // A redirection; a number right before it is its file descriptor (`2>`).
                let mut op = String::new();
                if in_word
                    && pending.is_none()
                    && !word.is_empty()
                    && word.chars().all(|d| d.is_ascii_digit())
                {
                    op = std::mem::take(&mut word);
                    in_word = false;
                } else {
                    end_word!();
                }
                op.push(c);
                i += 1;
                if c == '>' {
                    if matches!(chars.get(i), Some('>' | '|')) {
                        op.push(chars[i]);
                        i += 1;
                    }
                    if chars.get(i) == Some(&'&') {
                        op.push('&');
                        i += 1;
                    }
                } else if matches!(chars.get(i), Some('&' | '>')) {
                    op.push(chars[i]);
                    i += 1;
                }
                pending = Some(op);
                continue;
            }
            '&' if chars.get(i + 1) == Some(&'>') => {
                // `&>file`, `&>>file`: stdout and stderr.
                end_word!();
                let mut op = String::from("&>");
                i += 2;
                if chars.get(i) == Some(&'>') {
                    op.push('>');
                    i += 1;
                }
                pending = Some(op);
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
                last_group = None;
                i += 2;
                continue;
            }
            '|' => {
                // `|` or `|&`: a pipe into the next command.
                end_cmd!();
                pipe_from = match (last_group.take(), emitted) {
                    (Some(g), _) => g,
                    (None, Some(id)) => vec![id],
                    (None, None) => Vec::new(),
                };
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
    // Unclosed substitutions: close them so the commands they interrupt are kept.
    while !opens.is_empty() {
        close!(chars.len());
    }
    end_cmd!();
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
        assert_eq!(dirs("git --work-tree /v commit"), vec![none()]);
        assert_eq!(
            git_commits("git --work-tree /v --git-dir=/v/.git commit")[0].git_dir,
            Some("/v/.git".into())
        );
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
            "PATH+=:/tmp git x",
            "export PATH+=:/tmp; git x",
            "f() { local GIT_DIR=/x; git x; }",
        ] {
            assert!(git_calls(c)[0].config_overridden, "{c}");
        }
    }

    fn writes(c: &str) -> Vec<WriteTarget> {
        analyze(c).writes.into_iter().map(|w| w.target).collect()
    }

    fn paths(c: &str) -> Vec<String> {
        writes(c)
            .into_iter()
            .flat_map(|t| match t {
                WriteTarget::Paths { paths, .. } => paths,
                WriteTarget::Unknown => vec!["?".into()],
            })
            .collect()
    }

    #[test]
    fn redirections_are_writes_except_descriptors() {
        assert_eq!(paths("echo x > f"), vec!["f"]);
        assert_eq!(paths("echo x >> f; echo y 2>e"), vec!["f", "e"]);
        assert_eq!(paths("echo x &> f"), vec!["f"]);
        assert_eq!(paths("echo x >f 2>&1"), vec!["f"]);
        assert_eq!(paths("echo a>b"), vec!["b"]);
        assert_eq!(paths("> f"), vec!["f"]);
        assert_eq!(paths("cat > f <<'EOF'\nx\nEOF"), vec!["f"]);
        assert_eq!(paths("cat <<EOF >f\nx\nEOF"), vec!["f"]);
        assert_eq!(paths("git commit -m x 2>/dev/null"), vec!["/dev/null"]);
        assert!(paths("git commit -m x >&2").is_empty());
        assert!(paths("echo x 1>&2 2>&-").is_empty());
        assert!(paths("[[ a > b ]]").is_empty());
        assert!(paths("cat < f").is_empty());
        assert!(paths("echo \"a > b\" 'c > d'").is_empty());
        assert_eq!(paths("cd v && echo x > c.md"), vec!["c.md"]);
        match &writes("cd v && echo x > c.md")[0] {
            WriteTarget::Paths { dirs, .. } => assert_eq!(dirs, &v(&["v"])),
            WriteTarget::Unknown => panic!(),
        }
    }

    #[test]
    fn programs_that_may_write() {
        assert_eq!(paths("echo x | tee -a f g"), vec!["f", "g"]);
        assert_eq!(paths("cp a b"), vec!["a", "b"]);
        assert_eq!(paths("sed -i s/a/b/ f"), vec!["s/a/b/", "f"]);
        assert!(paths("sed s/a/b/ f").is_empty());
        assert_eq!(paths("dd if=/dev/zero of=f"), vec!["f"]);
        assert_eq!(paths("python3 -c 'x'"), vec!["?"]);
        assert_eq!(paths("./gen.sh"), vec!["?"]);
        assert_eq!(paths("bash gen.sh"), vec!["?"]);
        assert_eq!(paths("find . -delete"), vec!["?"]);
        assert_eq!(paths("ls | xargs rm"), vec!["?"]);
        assert_eq!(paths("$EDITOR f"), vec!["?"]);
        for c in [
            "git status && git log | head",
            "grep -rn x . | wc -l",
            "if true; then echo ok; fi",
            "for f in a b; do echo $f; done",
            "case $x in a) echo a ;; esac",
            "wikiknit check && git commit -m x",
            "command -v git",
            "bash -c 'echo hi'",
            "echo 'git commit' | cat",
        ] {
            assert!(paths(c).is_empty(), "{c}: {:?}", paths(c));
        }
    }

    #[test]
    fn scripts_fed_to_shells() {
        for c in [
            "bash <<< 'git commit -m x'",
            "echo 'git commit -m x' | bash",
            "echo git commit -m x | tee log | sh -s",
            "printf 'git add -A\\ngit commit -m x\\n' | sh",
            "source <(echo git commit -m x)",
            ". <(printf 'git commit')",
            "bash < <(echo git commit)",
            "bash <(echo git commit)",
            "echo git commit | bash /dev/stdin",
        ] {
            let calls = git_commits(c);
            assert_eq!(calls.len(), 1, "{c}: {calls:?}");
            assert_eq!(calls[0].subcommand, "commit", "{c}");
            assert!(!calls[0].opaque, "{c}");
        }
        for c in [
            "curl -s https://x | bash",
            "cat script.sh | sh",
            "source <(curl -s https://x)",
            "G=git; $G commit -m x",
            "$(which git) commit -m x",
            "`which git` commit -m x",
            "\"$(which git)\" -C repo commit",
        ] {
            let calls = git_calls(c);
            assert!(
                calls.iter().any(|g| g.subcommand == "commit"),
                "{c}: {calls:?}"
            );
            assert!(!analyze(c).writes.is_empty(), "{c}");
        }
        assert!(git_calls("curl -s https://x | bash")[0].opaque);
        assert_eq!(
            git_calls("$(which git) -C repo commit")[0].dirs,
            v(&["repo"])
        );
        // Not scripts.
        for c in [
            "echo git commit | cat",
            "cat <<< 'git commit'",
            "bash script.sh",
        ] {
            assert!(git_commits(c).is_empty(), "{c}");
        }
    }

    #[test]
    fn wrappers_redirections_and_find() {
        for c in [
            "2>/dev/null git commit -m x",
            ">/dev/null git commit",
            "time -p git commit -m x",
            "command git commit",
            "exec -a x git commit",
            "env -S 'git commit -m x'",
            "find . -exec git commit -m x \\;",
            "find . -execdir git commit -m x {} +",
            "setsid git commit",
        ] {
            assert_eq!(git_commits(c).len(), 1, "{c}");
        }
        assert!(git_calls("command -v git").is_empty());
        assert_eq!(dirs("env -C a git commit"), vec![v(&["a"])]);
        assert_eq!(dirs("env --chdir=a git commit"), vec![v(&["a"])]);
        assert_eq!(dirs("sudo -D a git commit"), vec![v(&["a"])]);
        assert_eq!(
            dirs("cd x; env -C a git -C b commit"),
            vec![v(&["x", "a", "b"])]
        );
    }

    #[test]
    fn git_dir_from_the_environment() {
        let c = &git_commits("GIT_DIR=r/.git GIT_WORK_TREE=r git commit")[0];
        assert_eq!(c.git_dir.as_deref(), Some("r/.git"));
        assert_eq!(c.work_tree.as_deref(), Some("r"));
        let c = &git_commits("export GIT_DIR=r/.git; git commit")[0];
        assert_eq!(c.git_dir.as_deref(), Some("r/.git"));
        let c = &git_commits("GIT_DIR=r/.git; export GIT_DIR; git commit")[0];
        assert_eq!(c.git_dir.as_deref(), Some("r/.git"));
        // Not exported: git does not see it.
        assert_eq!(git_commits("GIT_DIR=r/.git; git commit")[0].git_dir, None);
        // Does not leak out of a subshell.
        assert_eq!(
            git_commits("(export GIT_DIR=r/.git); git commit")[0].git_dir,
            None
        );
        assert_eq!(
            git_commits("export GIT_DIR=r/.git; env -u GIT_DIR git commit")[0].git_dir,
            None
        );
        assert_eq!(
            git_commits("GIT_DIR=a git --git-dir=b commit")[0]
                .git_dir
                .as_deref(),
            Some("b")
        );
        // Variables that do not change config lookup.
        assert!(!git_calls("GIT_PAGER=cat git lg")[0].config_overridden);
        assert!(!git_calls("GIT_EDITOR=true GIT_WORK_TREE=x git lg")[0].config_overridden);
        assert!(git_calls("GIT_CONFIG_GLOBAL=/x git lg")[0].config_overridden);
        assert!(git_calls("GIT_DIR=/x git lg")[0].config_overridden);
    }

    #[test]
    fn subcommand_args_are_kept() {
        let c = &git_calls("git -C r rebase --continue")[0];
        assert_eq!(c.subcommand, "rebase");
        assert_eq!(c.args, v(&["--continue"]));
    }

    #[test]
    fn too_deep_is_opaque() {
        let mut c = "git commit".to_string();
        for _ in 0..10 {
            c = format!("eval {c}");
        }
        let calls = git_calls(&c);
        assert!(calls.iter().any(|g| g.opaque), "{calls:?}");
    }

    /// Whether `c` is found to run `git commit` that the parser can see (not opaque).
    fn seen_commit(c: &str) -> bool {
        git_calls(c)
            .iter()
            .any(|g| g.subcommand == "commit" && !g.opaque)
    }

    fn opaque_call(c: &str) -> bool {
        git_calls(c).iter().any(|g| g.opaque)
    }

    #[test]
    fn substitutions_do_not_shift_pipes_or_heredocs() {
        for c in [
            "echo 'git commit -m x' | x=$(true) bash",
            "echo 'git commit -m x' | x=`true` bash",
            "cat <<'EOF' | x=$(true) sh\ngit commit -m x\nEOF",
            "bash -s <<'EOF' $(true)\ngit commit -m x\nEOF",
            "x=$(true) bash <<'EOF'\ngit commit -m x\nEOF",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn groups_piped_into_a_shell() {
        for c in [
            "(echo 'git commit -m x') | bash",
            "{ echo 'git commit -m x'; } | bash",
            "(echo hi; echo 'git commit') | bash",
            "echo 'git commit -m x' | (bash)",
            "echo 'git commit -m x' | { cd a; bash; }",
            "(cat <<'EOF'\ngit commit\nEOF\n) | bash",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        // Any unreadable part of a group makes the script opaque.
        for c in [
            "(curl -s x; echo hi) | bash",
            "{ curl -s x; echo hi; } | bash",
            "for f in a; do cat $f; done | bash",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(git_calls("(echo 'git commit') | cat").is_empty());
    }

    #[test]
    fn non_literal_program_words_are_opaque() {
        for c in [
            "$G status",
            "\"$(which git)\" st",
            "`which git` log",
            "$EDITOR f",
        ] {
            assert!(opaque_call(c), "{c}");
        }
        assert!(!opaque_call("x=$(true) git status"));
        assert!(opaque_call("bash $S"));
        assert!(opaque_call("source $F"));
    }

    #[test]
    fn env_options() {
        for c in [
            "env -S'git commit -m x'",
            "env -iS 'git commit -m x'",
            "env -vS'git commit'",
            "env --split-string='git commit -m \"a b\"'",
            "env -u X -C a -i git commit",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        assert_eq!(dirs("env -Ca git commit"), vec![v(&["a"])]);
        assert_eq!(dirs("env -iCa git commit"), vec![v(&["a"])]);
        assert_eq!(
            git_commits("export GIT_DIR=r; env -uGIT_DIR git commit")[0].git_dir,
            None
        );
        for c in [
            "env --bogus git status",
            "env -q git status",
            "env -iq git status",
        ] {
            assert!(opaque_call(c), "{c}");
        }
        assert!(git_calls("env --help").is_empty());
    }
}
