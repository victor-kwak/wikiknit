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
    /// `alias NAME=VALUE` earlier in the command (expanded whether or not `expand_aliases`
    /// is on: fail closed).
    aliases: BTreeMap<String, String>,
    /// Aliases were changed in a way this parser does not follow (`BASH_ALIASES[x]=...`, an
    /// odd `alias` form): every later command is opaque.
    opaque_rest: bool,
    /// `set -a`: every later assignment is exported.
    allexport: bool,
}

/// A name `alias` can define that this parser follows.
fn is_alias_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
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
    let items = tokenize(command);
    for (k, item) in items.iter().cloned().enumerate() {
        // A function definition (`f() { ...; }`, `function f`): its body runs when called,
        // not here, and a call can change anything about this shell. The rest is opaque.
        if let Item::Cmd(c) = &item {
            let fn_def = c.words.first().is_some_and(|w| w == "function")
                || (c.words.len() == 1
                    && matches!(items.get(k + 1), Some(Item::Open))
                    && matches!(items.get(k + 2), Some(Item::Close)));
            if fn_def {
                ctx.opaque_rest = true;
            }
        }
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
        // Every command the text can reach through pipes, groups, and substitutions.
        let mut seen = std::collections::BTreeSet::new();
        let mut todo = vec![cmd];
        let mut feeds_shell = false;
        while let Some(j) = todo.pop() {
            if !seen.insert(j) {
                continue;
            }
            if commands
                .iter()
                .find(|c| c.id == j)
                .is_some_and(|c| reads_stdin_shell(&c.words))
            {
                feeds_shell = true;
                break;
            }
            todo.extend(
                commands
                    .iter()
                    .filter(|n| n.stdin_from.contains(&j))
                    .map(|n| n.id),
            );
        }
        if feeds_shell {
            collect(&text, &mut inner, out, depth + 1);
        }
    }
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
        // Only redirections or assignments: what reaches the pipe cannot be told.
        return Feed::Opaque;
    };
    // Escapes `echo`/`printf` may interpret (`\x74`): the output cannot be read.
    let unreadable = |text: &str| text.contains('\\');
    match program_name(program) {
        "echo" | "printf" => {
            let feed = output_text(program_name(program), args);
            match feed {
                Some(t) if !unreadable(&t) => Feed::Literal(t),
                _ => Feed::Opaque,
            }
        }
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

/// What `echo ARGS` / `printf ARGS` print, when it can be told. printf prints its format
/// (no `%` directives): an option other than `--` or `-v VAR` (nothing printed) is unknown.
/// echo takes leading `-n`/`-e`/`-E` clusters; other words are text. Words the shell expands
/// are unknown. Remaining backslashes are left for the caller to refuse.
fn output_text(program: &str, args: &[String]) -> Option<String> {
    if program == "printf" {
        let mut args = args;
        if args.first().is_some_and(|a| a == "--") {
            args = &args[1..];
        }
        if let Some(first) = args.first()
            && first.len() > 1
            && first.starts_with('-')
        {
            return (first == "-v").then(String::new);
        }
        let format = args.first().map_or("", String::as_str);
        if format.contains('%') || is_dynamic_word(format) {
            return None;
        }
        return Some(unescape(format));
    }
    let args: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .skip_while(|a| {
            a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| "neE".contains(c))
        })
        .collect();
    if args.iter().any(|a| is_dynamic_word(a)) {
        return None;
    }
    Some(unescape(&args.join(" ")))
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
    let mut p = strip_prefixes(words);
    if p.not_run {
        return;
    }
    // `xargs -I{} {} commit`: the replace string becomes text read from stdin.
    if let Some(rs) = p.xargs_replace.clone() {
        for w in &mut p.words {
            *w = w.replace(&rs, "$XARGS");
        }
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
    // After an alias change this parser cannot follow, nothing later can be read.
    if words.iter().any(|w| w.contains("BASH_ALIASES"))
        || redirects.iter().any(|r| r.target.contains("BASH_ALIASES"))
    {
        ctx.opaque_rest = true;
    }
    // Under xargs, an empty command gets its words from stdin.
    if p.unparsed || ctx.opaque_rest || (p.xargs && p.words.is_empty()) {
        opaque(out, ctx, &what);
        return;
    }
    let Some((program, args)) = p.words.split_first() else {
        // `exec` with only redirections: a new stdin for every later command.
        if words.iter().any(|w| w == "exec")
            && redirects.iter().any(|r| {
                let op = r.op.trim_start_matches('0');
                op.starts_with('<')
            })
        {
            ctx.opaque_rest = true;
        }
        // Only assignments: shell variables, exported only under `set -a`.
        for (k, v) in &p.assigns {
            if is_config_var(k) {
                ctx.config = true;
            }
            if ctx.allexport {
                ctx.export(k, v);
            }
            ctx.vars.insert(k.clone(), v.clone());
        }
        return;
    };
    let mut dirs = ctx.cds.clone();
    dirs.extend(p.dirs.iter().cloned());
    let name = program_name(program);
    // An alias defined earlier: run what it stands for.
    if !program.contains('/')
        && let Some(value) = ctx.aliases.get(program.as_str()).cloned()
    {
        let mut text = value;
        for a in args {
            text.push(' ');
            text.push_str(a);
        }
        let saved = ctx.aliases.remove(program.as_str()); // no self-recursion
        collect(&text, ctx, out, depth + 1);
        if let Some(v) = saved {
            ctx.aliases.insert(program.clone(), v);
        }
        return;
    }
    // Under xargs, more arguments come from stdin: a shell runs unknown script files, and
    // git with no subcommand gets one from stdin.
    let xargs_unknown = p.xargs
        && ((SHELLS.contains(&name) && !matches!(shell_run(args), ShellRun::Script(_)))
            || (name == "git" && parse_git(args, &dirs, None, None, false).is_none()));
    // A program word that is not a literal (`$G`, `$(which git)`), or a wrapper option this
    // parser does not know: it may be git, doing anything, in a repository that cannot be
    // known.
    if is_dynamic_word(program) || p.unparsed || xargs_unknown {
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
            // A nameref (`declare -n`) makes later assignments land on another name.
            if name != "export" && args.iter().any(|a| a.starts_with('-') && a.contains('n')) {
                ctx.opaque_rest = true;
            }
            for a in args.iter().filter(|a| !a.starts_with('-')) {
                let var = a.split_once('=').map_or(a.as_str(), |(k, _)| k);
                if is_dynamic_word(var.strip_suffix('+').unwrap_or(var)) {
                    ctx.opaque_rest = true;
                }
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
        // Builtins that assign variables from input: names this parser cannot follow make
        // the rest of the command opaque.
        "read" => match parse_opts(&READ_OPTS, args) {
            Some(parsed) => {
                let mut names = parsed.operands.clone();
                names.extend(parsed.values(&["a"]));
                if names.iter().any(|n| risky_name(n)) {
                    ctx.opaque_rest = true;
                }
            }
            None => ctx.opaque_rest = true,
        },
        "mapfile" | "readarray" => match parse_opts(&MAPFILE_OPTS, args) {
            Some(parsed) if !parsed.has(&["C"]) => {
                if parsed.operands.iter().any(|n| risky_name(n)) {
                    ctx.opaque_rest = true;
                }
            }
            _ => ctx.opaque_rest = true,
        },
        "printf" => {
            let args = match args.first() {
                Some(a) if a == "--" => &args[1..],
                _ => args,
            };
            // `-v NAME` and `-vNAME` assign; any other option is not modelled.
            match args.first().map(String::as_str) {
                Some("-v") => {
                    if args.get(1).is_none_or(|n| risky_name(n)) {
                        ctx.opaque_rest = true;
                    }
                }
                Some(a) if a.starts_with("-v") => {
                    if risky_name(&a[2..]) {
                        ctx.opaque_rest = true;
                    }
                }
                Some(a) if a.starts_with('-') && a.len() > 1 => ctx.opaque_rest = true,
                _ => {}
            }
        }
        "set" => {
            let mut it = args.iter();
            while let Some(a) = it.next() {
                let on = a.starts_with('-');
                if !(on || a.starts_with('+')) || a.len() < 2 || a == "--" {
                    break;
                }
                // Every letter of a cluster counts (`-ea`, `-ao allexport`).
                if a[1..].contains('a') {
                    ctx.allexport = on;
                }
                if a[1..].contains('o') && it.next().is_some_and(|o| o == "allexport") {
                    ctx.allexport = on;
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
            match shell_run(args) {
                ShellRun::Script(s) => collect(&s, &mut inner, out, depth + 1),
                ShellRun::Stdin => run_fed(feed, &inner, out, depth, &what),
                ShellRun::File(f) => run_file(&f, feed, &inner, out, depth, &what),
                ShellRun::NotRun => {}
                ShellRun::Unknown => opaque(out, &inner, &what),
            }
        }
        // `source` and `eval` run in this shell: what they change (`cd`, `export`, aliases)
        // carries on to later commands.
        "source" | "." => {
            match parse_opts(&SOURCE_OPTS, args).and_then(|p| p.operands.first().cloned()) {
                Some(f) => source_file(&f, feed, ctx, out, depth, &what),
                None => {
                    opaque(out, ctx, &what);
                    ctx.opaque_rest = true;
                }
            }
        }
        "eval" => collect(&args.join(" "), ctx, out, depth + 1),
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
            if let Some(mut call) = parse_git(args, &dirs, git_dir, work_tree, config) {
                // `git $CMD`, `git comm?t`, `git {commit,}`: the subcommand is not a literal.
                if is_dynamic_word(&call.subcommand) {
                    call.subcommand = "commit".into();
                    call.config_overridden = true;
                    call.opaque = true;
                    out.writes.push(Write {
                        what: what.clone(),
                        target: WriteTarget::Unknown,
                    });
                }
                out.calls.push(call);
            }
        }
        "alias" => {
            // `alias NAME=VALUE` is followed; any form this parser cannot follow makes the rest
            // of the command opaque.
            match parse_opts(&ALIAS_OPTS, args) {
                Some(parsed) => {
                    for a in &parsed.operands {
                        match a.split_once('=') {
                            Some((k, v)) if is_alias_name(k) => {
                                ctx.aliases.insert(k.to_string(), v.to_string());
                            }
                            None if is_alias_name(a) => {} // prints it
                            _ => ctx.opaque_rest = true,
                        }
                    }
                }
                None => ctx.opaque_rest = true,
            }
        }
        "unalias" => {
            for a in args {
                ctx.aliases.remove(a.as_str());
            }
        }
        // Commands that run a script or command argument. Each reads its options from a
        // complete table; an unknown option or a missing command is opaque.
        "trap" => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            match parse_opts(&TRAP_OPTS, args) {
                // `trap ARG SIGNAL...`; one operand, or ARG `-`, resets.
                Some(parsed) if parsed.opts.is_empty() => {
                    if parsed.operands.len() >= 2 && parsed.operands[0] != "-" {
                        collect(&parsed.operands[0], &mut inner, out, depth + 1);
                    }
                }
                Some(_) => {} // `-l`, `-p`: print
                None => opaque(out, &inner, &what),
            }
        }
        "watch" => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            match parse_opts(&WATCH_OPTS, args) {
                Some(parsed) if parsed.has(&["h", "v", "help", "version"]) => {}
                // `-x`: the words are an argv; else one string for `sh -c`.
                Some(parsed) if !parsed.operands.is_empty() => {
                    if parsed.has(&["x", "exec"]) {
                        simple(&parsed.operands, &[], feed, &mut inner, out, depth + 1);
                    } else {
                        collect(&parsed.operands.join(" "), &mut inner, out, depth + 1);
                    }
                }
                _ => opaque(out, &inner, &what),
            }
        }
        "su" | "runuser" => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            let spec = if name == "su" {
                &SU_OPTS
            } else {
                &RUNUSER_OPTS
            };
            match parse_opts(spec, args) {
                None => opaque(out, &inner, &what),
                Some(parsed) if parsed.has(&["h", "V", "help", "version"]) => {}
                // Another shell (`-s`, or `$SHELL` kept by `-m`/`-p`) runs the command with
                // rules this parser does not model; extra words after the user go to it.
                Some(parsed)
                    if !parsed.has(&["u", "user"])
                        && (parsed.has(&["s", "shell", "m", "p", "preserve-environment"])
                            || parsed.operands.iter().filter(|o| o.as_str() != "-").count()
                                > 1) =>
                {
                    opaque(out, &inner, &what);
                }
                Some(parsed) => {
                    let scripts = parsed.values(&["c", "command", "session-command"]);
                    if let Some(script) = scripts.last() {
                        collect(script, &mut inner, out, depth + 1);
                    } else if parsed.has(&["u", "user"]) {
                        // `runuser -u USER COMMAND...`
                        if parsed.operands.is_empty() {
                            opaque(out, &inner, &what);
                        } else {
                            simple(&parsed.operands, &[], feed, &mut inner, out, depth + 1);
                        }
                    } else {
                        // A shell reading stdin; words after the user go to that shell.
                        let operands = parsed.operands.iter().filter(|o| o.as_str() != "-");
                        if operands.count() > 1 {
                            opaque(out, &inner, &what);
                        } else {
                            run_fed(feed, &inner, out, depth, &what);
                        }
                    }
                }
            }
        }
        "script" => {
            let mut inner = ctx.clone();
            inner.cds = dirs.clone();
            match parse_opts(&SCRIPT_OPTS, args) {
                None => opaque(out, &inner, &what),
                Some(parsed) if parsed.has(&["h", "V", "help", "version"]) => {}
                Some(parsed) => {
                    // The typescript and log files.
                    let mut paths = parsed.operands.clone();
                    paths.extend(parsed.values(&[
                        "B",
                        "I",
                        "O",
                        "T",
                        "t",
                        "log-io",
                        "log-in",
                        "log-out",
                        "log-timing",
                        "timing",
                    ]));
                    if !paths.is_empty() {
                        write(out, &dirs, paths, false);
                    }
                    match parsed.values(&["c", "command"]).last() {
                        Some(script) => collect(script, &mut inner, out, depth + 1),
                        // A shell reading stdin.
                        None => run_fed(feed, &inner, out, depth, &what),
                    }
                }
            }
        }
        "flock" => {
            let mut inner = ctx.clone();
            inner.cds = dirs;
            match parse_opts(&FLOCK_OPTS, args) {
                None => opaque(out, &inner, &what),
                Some(parsed) if parsed.has(&["h", "V", "help", "version"]) => {}
                Some(parsed) => {
                    if let Some(script) = parsed.values(&["c", "command"]).last() {
                        collect(script, &mut inner, out, depth + 1);
                    } else {
                        // `flock FILE COMMAND...`, `flock FILE -c COMMAND` (exactly one
                        // argument after `-c`, as util-linux checks), or `flock FD`.
                        match parsed.operands.split_first() {
                            Some((_, command))
                                if command
                                    .first()
                                    .is_some_and(|c| c == "-c" || c == "--command") =>
                            {
                                if command.len() == 2 {
                                    collect(&command[1], &mut inner, out, depth + 1);
                                } else {
                                    opaque(out, &inner, &what);
                                }
                            }
                            Some((_, command)) if !command.is_empty() => {
                                simple(command, &[], feed, &mut inner, out, depth + 1);
                            }
                            Some((fd, _)) if fd.chars().all(|c| c.is_ascii_digit()) => {}
                            _ => opaque(out, &inner, &what),
                        }
                    }
                }
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
                        // `{}` is a found path: text this parser cannot see.
                        let found: Vec<String> = args[start..j]
                            .iter()
                            .map(|a| a.replace("{}", "$FOUND"))
                            .collect();
                        simple(&found, &[], &Feed::Terminal, &mut inner, out, depth + 1);
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

/// What a shell runs, from its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellRun {
    /// `-c SCRIPT`.
    Script(String),
    /// Its script from stdin (no operands, or `-s`).
    Stdin,
    /// A script file (the first operand).
    File(String),
    /// `--help`, `--version`.
    NotRun,
    /// An option not in the table, or `-c` with no script.
    Unknown,
}

fn shell_run(args: &[String]) -> ShellRun {
    let Some(p) = parse_opts(&SHELL_OPTS, args) else {
        return ShellRun::Unknown;
    };
    if p.has(&["help", "version"]) {
        return ShellRun::NotRun;
    }
    // A lone `-` ends the options, as `--` does.
    let mut operands = p.operands.clone();
    if operands.first().is_some_and(|o| o == "-") {
        operands.remove(0);
    }
    if p.has(&["c"]) {
        return operands
            .first()
            .map_or(ShellRun::Unknown, |s| ShellRun::Script(s.clone()));
    }
    if p.has(&["s"]) || operands.is_empty() {
        return ShellRun::Stdin;
    }
    ShellRun::File(operands[0].clone())
}

/// A shell that may read its script from stdin (for heredoc bodies and pipes): one that
/// reads stdin, names `/dev/stdin`, a word that can expand to nothing, or options this
/// parser does not know.
fn reads_stdin_shell(words: &[String]) -> bool {
    let p = strip_prefixes(words);
    let Some((prog, args)) = p.words.split_first() else {
        return false;
    };
    if !SHELLS.contains(&program_name(prog)) {
        return false;
    }
    match shell_run(args) {
        ShellRun::Stdin | ShellRun::Unknown => true,
        ShellRun::File(f) => f == "/dev/stdin" || f == "-" || is_dynamic_word(&f),
        ShellRun::Script(_) | ShellRun::NotRun => false,
    }
}

/// Run a script file (`bash FILE`, `source FILE`): `/dev/stdin` and process substitutions
/// are parsed; a word that expands is opaque; a file may write anything (its commits are not
/// seen, as for any program).
fn run_file(file: &str, feed: &Feed, ctx: &Ctx, out: &mut Analysis, depth: usize, what: &str) {
    if file == "/dev/stdin" || file == "-" {
        run_fed(feed, ctx, out, depth, what);
    } else if is_dynamic_word(file) {
        opaque(out, ctx, what);
    } else if let Some(inner) = process_substitution(file) {
        run_fed(&process_output(inner), ctx, out, depth, what);
    } else {
        out.writes.push(Write {
            what: what.to_string(),
            target: WriteTarget::Unknown,
        });
    }
}

/// `source FILE` / `. FILE`: the script runs in this shell. A literal script (from a process
/// substitution or `/dev/stdin`) is parsed in this shell's state; a file, a heredoc, or an
/// unreadable script can change anything about this shell, so the rest of the command is
/// opaque.
fn source_file(
    file: &str,
    feed: &Feed,
    ctx: &mut Ctx,
    out: &mut Analysis,
    depth: usize,
    what: &str,
) {
    let script = if file == "/dev/stdin" || file == "-" {
        feed.clone()
    } else if is_dynamic_word(file) {
        Feed::Opaque
    } else if let Some(inner) = process_substitution(file) {
        process_output(inner)
    } else {
        Feed::File
    };
    match script {
        Feed::Literal(text) => collect(&text, ctx, out, depth + 1),
        Feed::Terminal => {}
        Feed::File => {
            out.writes.push(Write {
                what: what.to_string(),
                target: WriteTarget::Unknown,
            });
            ctx.opaque_rest = true;
        }
        // The heredoc body is parsed where it appears, but not in this shell's state.
        Feed::Heredoc(_) | Feed::Opaque => {
            opaque(out, ctx, what);
            ctx.opaque_rest = true;
        }
    }
}

/// A variable name an assignment may set that this parser cannot follow: not a literal,
/// one that changes how git runs, or the alias table.
fn risky_name(name: &str) -> bool {
    is_dynamic_word(name)
        || is_config_var(name)
        || name == "GIT_WORK_TREE"
        || name.contains("BASH_ALIASES")
}

fn program_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// A word the shell expands at run time into something this parser cannot see: parameters
/// and substitutions, globs (`gi?`), brace expansion (`{git,x}`, `{a..b}`). The test
/// commands `[` and `[[` are words, not globs.
fn is_dynamic_word(w: &str) -> bool {
    if w.contains(['$', '`']) {
        return true;
    }
    if w == "[" || w == "[[" {
        return false;
    }
    if w.contains(['*', '?', '[']) {
        return true;
    }
    // `{a,b}` or `{a..b}`, in any group of the word
    w.contains('{') && (w.contains(',') || w.contains(".."))
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

/// One program's command-line options, as `getopt_long` reads them. Every wrapper or runner
/// this parser models has a complete table; an option not in it makes the call opaque.
struct Opts {
    /// Short options without a value.
    flags: &'static str,
    /// Short options with a value: the rest of the word, else the next word.
    args: &'static str,
    /// Short options with an optional value: only the rest of the word.
    optional: &'static str,
    long_flags: &'static [&'static str],
    /// Long options with a value: `--name=value` or `--name value`.
    long_args: &'static [&'static str],
    /// Long options with an optional value: only `--name=value`.
    long_optional: &'static [&'static str],
    /// `+x` clusters are options too (shells).
    plus: bool,
    /// Options may follow operands (GNU argument permutation); otherwise the first operand
    /// ends the options.
    permute: bool,
}

const NO_OPTS: Opts = Opts {
    flags: "",
    args: "",
    optional: "",
    long_flags: &[],
    long_args: &[],
    long_optional: &[],
    plus: false,
    permute: false,
};

/// Options read from a command line.
#[derive(Debug, Default)]
struct OptsParsed {
    /// (name, value): a short option's letter (`+x` for plus options) or a long option's name.
    opts: Vec<(String, Option<String>)>,
    operands: Vec<String>,
}

impl OptsParsed {
    fn has(&self, names: &[&str]) -> bool {
        self.opts.iter().any(|(n, _)| names.contains(&n.as_str()))
    }

    /// Values of the options named `names`, in order.
    fn values(&self, names: &[&str]) -> Vec<String> {
        self.opts
            .iter()
            .filter(|(n, _)| names.contains(&n.as_str()))
            .filter_map(|(_, v)| v.clone())
            .collect()
    }
}

/// Parse `words` against `spec`. `None` when an option is not in the table or a required
/// value is missing: the caller cannot tell what runs.
fn parse_opts(spec: &Opts, words: &[String]) -> Option<OptsParsed> {
    let mut p = OptsParsed::default();
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        i += 1;
        if w == "--" {
            p.operands.extend(words[i..].iter().cloned());
            return Some(p);
        }
        if let Some(long) = w.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if spec.long_flags.contains(&name) {
                if value.is_some() {
                    return None;
                }
                p.opts.push((name.to_string(), None));
            } else if spec.long_optional.contains(&name) {
                p.opts.push((name.to_string(), value));
            } else if spec.long_args.contains(&name) {
                let value = match value {
                    Some(v) => v,
                    None => {
                        i += 1;
                        words.get(i - 1)?.clone()
                    }
                };
                p.opts.push((name.to_string(), Some(value)));
            } else {
                return None;
            }
            continue;
        }
        let plus = spec.plus && w.starts_with('+');
        if w.len() > 1 && (w.starts_with('-') || plus) {
            let chars: Vec<char> = w[1..].chars().collect();
            let mut k = 0;
            while k < chars.len() {
                let c = chars[k];
                let name = if plus { format!("+{c}") } else { c.to_string() };
                let rest: String = chars[k + 1..].iter().collect();
                if spec.flags.contains(c) {
                    p.opts.push((name, None));
                    k += 1;
                } else if spec.optional.contains(c) {
                    p.opts.push((name, (!rest.is_empty()).then_some(rest)));
                    break;
                } else if spec.args.contains(c) {
                    let value = if rest.is_empty() {
                        i += 1;
                        words.get(i - 1)?.clone()
                    } else {
                        rest
                    };
                    p.opts.push((name, Some(value)));
                    break;
                } else {
                    return None;
                }
            }
            continue;
        }
        if !spec.permute {
            p.operands.extend(words[i - 1..].iter().cloned());
            return Some(p);
        }
        p.operands.push(w.clone());
    }
    Some(p)
}

const SUDO_OPTS: Opts = Opts {
    flags: "ABbEeHiKklNnPSsVv",
    args: "CDgpRrTtUu",
    long_flags: &[
        "askpass",
        "bell",
        "background",
        "edit",
        "set-home",
        "login",
        "remove-timestamp",
        "reset-timestamp",
        "list",
        "non-interactive",
        "preserve-groups",
        "stdin",
        "shell",
        "version",
        "validate",
        "help",
    ],
    long_args: &[
        "close-from",
        "chdir",
        "group",
        "host",
        "prompt",
        "chroot",
        "role",
        "type",
        "command-timeout",
        "other-user",
        "user",
    ],
    long_optional: &["preserve-env"],
    ..NO_OPTS
};
const DOAS_OPTS: Opts = Opts {
    flags: "Lns",
    args: "aCu",
    ..NO_OPTS
};
const TIMEOUT_OPTS: Opts = Opts {
    flags: "fpv",
    args: "ks",
    long_flags: &[
        "foreground",
        "preserve-status",
        "verbose",
        "help",
        "version",
    ],
    long_args: &["kill-after", "signal"],
    ..NO_OPTS
};
const NICE_OPTS: Opts = Opts {
    args: "n",
    long_flags: &["help", "version"],
    long_args: &["adjustment"],
    ..NO_OPTS
};
const IONICE_OPTS: Opts = Opts {
    flags: "thV",
    args: "cnpPu",
    long_flags: &["ignore", "help", "version"],
    long_args: &["class", "classdata", "pid", "pgid", "uid"],
    ..NO_OPTS
};
const XARGS_OPTS: Opts = Opts {
    flags: "0oprtx",
    args: "adEILnPs",
    optional: "eil",
    long_flags: &[
        "null",
        "interactive",
        "no-run-if-empty",
        "verbose",
        "exit",
        "open-tty",
        "show-limits",
        "help",
        "version",
    ],
    long_args: &[
        "arg-file",
        "delimiter",
        "max-args",
        "max-procs",
        "max-chars",
        "process-slot-var",
    ],
    long_optional: &["eof", "replace", "max-lines"],
    ..NO_OPTS
};
const STDBUF_OPTS: Opts = Opts {
    args: "ioe",
    long_flags: &["help", "version"],
    long_args: &["input", "output", "error"],
    ..NO_OPTS
};
const ENV_OPTS: Opts = Opts {
    flags: "i0v",
    args: "uCS",
    long_flags: &[
        "ignore-environment",
        "null",
        "debug",
        "list-signal-handling",
        "help",
        "version",
    ],
    long_args: &["unset", "chdir", "split-string"],
    long_optional: &["block-signal", "default-signal", "ignore-signal"],
    ..NO_OPTS
};
const CHRONIC_OPTS: Opts = Opts {
    flags: "ev",
    ..NO_OPTS
};
const SETSID_OPTS: Opts = Opts {
    flags: "cfwhV",
    long_flags: &["ctty", "fork", "wait", "help", "version"],
    ..NO_OPTS
};
const UNBUFFER_OPTS: Opts = Opts {
    flags: "p",
    ..NO_OPTS
};
const HELP_ONLY_OPTS: Opts = Opts {
    long_flags: &["help", "version"],
    ..NO_OPTS
};
const TIME_OPTS: Opts = Opts {
    flags: "p",
    ..NO_OPTS
};
const COMMAND_OPTS: Opts = Opts {
    flags: "pvV",
    ..NO_OPTS
};
const EXEC_OPTS: Opts = Opts {
    flags: "cl",
    args: "a",
    ..NO_OPTS
};
/// bash (and sh, dash, zsh, ksh, read with bash's table: an option bash does not have is
/// unknown).
const SHELL_OPTS: Opts = Opts {
    flags: "abBcCDeEfhHiklmnpPrstTuvx",
    args: "oO",
    long_flags: &[
        "debug",
        "debugger",
        "dump-po-strings",
        "dump-strings",
        "help",
        "login",
        "noediting",
        "noprofile",
        "norc",
        "posix",
        "pretty-print",
        "restricted",
        "verbose",
        "version",
    ],
    long_args: &["init-file", "rcfile"],
    plus: true,
    ..NO_OPTS
};
const SOURCE_OPTS: Opts = Opts {
    args: "p",
    ..NO_OPTS
};
const SU_OPTS: Opts = Opts {
    flags: "flmpPThV",
    args: "cgGsw",
    long_flags: &[
        "login",
        "preserve-environment",
        "fast",
        "pty",
        "no-pty",
        "help",
        "version",
    ],
    long_args: &[
        "command",
        "session-command",
        "group",
        "supp-group",
        "shell",
        "whitelist-environment",
    ],
    permute: true,
    ..NO_OPTS
};
const RUNUSER_OPTS: Opts = Opts {
    flags: "flmpPThV",
    args: "cgGsuw",
    long_flags: &[
        "login",
        "preserve-environment",
        "fast",
        "pty",
        "no-pty",
        "help",
        "version",
    ],
    long_args: &[
        "command",
        "session-command",
        "group",
        "supp-group",
        "shell",
        "user",
        "whitelist-environment",
    ],
    permute: true,
    ..NO_OPTS
};
const SCRIPT_OPTS: Opts = Opts {
    flags: "aefqhV",
    args: "cEBIOTmo",
    optional: "t",
    long_flags: &[
        "append", "return", "flush", "force", "quiet", "help", "version",
    ],
    long_args: &[
        "command",
        "echo",
        "log-io",
        "log-in",
        "log-out",
        "log-timing",
        "logging-format",
        "output-limit",
    ],
    long_optional: &["timing"],
    permute: true,
    ..NO_OPTS
};
const FLOCK_OPTS: Opts = Opts {
    flags: "sxenuoFhV",
    args: "wEc",
    long_flags: &[
        "shared",
        "exclusive",
        "unlock",
        "nonblock",
        "nb",
        "close",
        "no-fork",
        "verbose",
        "help",
        "version",
    ],
    long_args: &["wait", "timeout", "conflict-exit-code", "command"],
    ..NO_OPTS
};
const WATCH_OPTS: Opts = Opts {
    flags: "bcCegprtwxhv",
    args: "nqs",
    optional: "d",
    long_flags: &[
        "beep", "color", "no-color", "errexit", "chgexit", "precise", "no-rerun", "no-title",
        "no-wrap", "exec", "help", "version",
    ],
    long_args: &["interval", "equexit", "shotsdir"],
    long_optional: &["differences"],
    ..NO_OPTS
};
const TRAP_OPTS: Opts = Opts {
    flags: "lpP",
    ..NO_OPTS
};
const READ_OPTS: Opts = Opts {
    flags: "ers",
    args: "adinNptu",
    ..NO_OPTS
};
const MAPFILE_OPTS: Opts = Opts {
    flags: "t",
    args: "dnOsuCc",
    ..NO_OPTS
};
const ALIAS_OPTS: Opts = Opts {
    flags: "p",
    ..NO_OPTS
};

/// The options table of a wrapper that runs the rest of its words as a command.
fn wrapper_opts(name: &str) -> Option<&'static Opts> {
    Some(match name {
        "sudo" => &SUDO_OPTS,
        "doas" => &DOAS_OPTS,
        "timeout" => &TIMEOUT_OPTS,
        "nice" => &NICE_OPTS,
        "ionice" => &IONICE_OPTS,
        "xargs" => &XARGS_OPTS,
        "stdbuf" => &STDBUF_OPTS,
        "env" => &ENV_OPTS,
        "chronic" => &CHRONIC_OPTS,
        "setsid" => &SETSID_OPTS,
        "unbuffer" => &UNBUFFER_OPTS,
        "nohup" | "builtin" => &HELP_ONLY_OPTS,
        "time" => &TIME_OPTS,
        "command" => &COMMAND_OPTS,
        "exec" => &EXEC_OPTS,
        _ => return None,
    })
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
    /// `xargs -I STR` / `-i` / `--replace`: STR in the command is replaced by stdin text.
    xargs_replace: Option<String>,
    /// Not run at all (`command -v git`, `sudo -l`).
    not_run: bool,
    /// A wrapper option this parser does not know, or a missing command: what runs cannot
    /// be told.
    unparsed: bool,
}

/// Drop shell keywords, env assignments, and wrapper commands (with their options)
/// in front of the real program.
#[allow(clippy::too_many_lines)] // one table of wrappers
fn strip_prefixes(words: &[String]) -> Prefixed {
    let mut w: Vec<String> = words.to_vec();
    let mut p = Prefixed::default();
    loop {
        while let Some((k, v)) = w.first().and_then(|t| split_assignment(t)) {
            p.assigns.push((k.to_string(), v.to_string()));
            w.remove(0);
        }
        let Some(first) = w.first().cloned() else {
            break;
        };
        if matches!(
            first.as_str(),
            "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "{" | "}" | "!" | "coproc"
        ) {
            w.remove(0);
            continue;
        }
        let name = program_name(&first).to_string();
        let Some(spec) = wrapper_opts(&name) else {
            break;
        };
        let mut rest: Vec<String> = w[1..].to_vec();
        // `nice -10 cmd` is `nice -n 10 cmd`.
        if name == "nice"
            && let Some(first) = rest.first()
            && first.len() > 1
            && first.starts_with('-')
            && first[1..].chars().all(|c| c.is_ascii_digit())
        {
            rest[0] = format!("-n{}", &first[1..]);
        }
        let Some(parsed) = parse_opts(spec, &rest) else {
            p.unparsed = true;
            return p;
        };
        let mut ops = parsed.operands.clone();
        let help = parsed.has(&["help", "version"]);
        let mut empty_ok = false;
        match name.as_str() {
            "sudo" => {
                if parsed.has(&["e", "edit", "R", "chroot"]) {
                    p.unparsed = true;
                    return p;
                }
                if help || parsed.has(&["l", "list", "v", "validate", "K", "V"]) {
                    p.not_run = true;
                }
                p.dirs.extend(parsed.values(&["D", "chdir"]));
                if ops.is_empty() {
                    if parsed.has(&["s", "shell", "i", "login"]) {
                        ops = vec!["sh".into()]; // a shell reading stdin
                    } else if parsed.has(&["k", "remove-timestamp", "K"]) {
                        p.not_run = true;
                    }
                }
            }
            "doas" => {
                if parsed.has(&["C", "L"]) {
                    p.not_run = true;
                }
                if ops.is_empty() && parsed.has(&["s"]) {
                    ops = vec!["sh".into()];
                }
            }
            "timeout" => {
                if !help && !ops.is_empty() {
                    ops.remove(0); // DURATION
                }
            }
            "nice" | "time" | "command" | "exec" | "builtin" => {
                empty_ok = true;
                if name == "command" && parsed.has(&["v", "V"]) {
                    p.not_run = true;
                }
            }
            "ionice" => {
                empty_ok = true;
                if parsed.has(&["p", "P", "u", "pid", "pgid", "uid"]) {
                    p.not_run = true;
                }
            }
            "xargs" => {
                p.xargs = true;
                let replace = parsed
                    .opts
                    .iter()
                    .rfind(|(n, _)| matches!(n.as_str(), "I" | "i" | "replace"));
                if let Some((_, v)) = replace {
                    p.xargs_replace = Some(v.clone().unwrap_or_else(|| "{}".into()));
                }
            }
            "env" => {
                empty_ok = true; // prints the environment
                while ops.first().is_some_and(|o| o == "-") {
                    ops.remove(0); // `env -` is `env -i`
                }
                p.unsets.extend(parsed.values(&["u", "unset"]));
                p.dirs.extend(parsed.values(&["C", "chdir"]));
                let mut split = Vec::new();
                for s in parsed.values(&["S", "split-string"]) {
                    match env_split(&s) {
                        // A split string that starts with an option is parsed by env again.
                        Some(words) if !words.first().is_some_and(|f| f.starts_with('-')) => {
                            split.extend(words);
                        }
                        _ => {
                            p.unparsed = true;
                            return p;
                        }
                    }
                }
                split.extend(ops);
                ops = split;
            }
            _ => {}
        }
        if help {
            p.not_run = true;
        }
        if p.not_run {
            return p;
        }
        if ops.is_empty() {
            if !empty_ok {
                p.unparsed = true;
            }
            p.words = ops;
            return p;
        }
        w = ops;
    }
    p.words = w;
    p
}

/// Split an `env -S` string into words the way GNU env does, for the part of its rules
/// that agrees with the shell: whitespace, `'...'`, `"..."`. Anything env treats
/// differently (`${VAR}` expansion, backslash escapes such as `\_` and `\c`, `#` comments)
/// or the shell would but env does not (`;`, `|`, `&`, `<`, `>`, `(`, `)`, `` ` ``, `$(`)
/// returns `None`: the caller cannot tell what runs.
fn env_split(s: &str) -> Option<Vec<String>> {
    if s.contains(['$', '\\', '#', ';', '|', '&', '<', '>', '(', ')', '`']) {
        return None;
    }
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some(q) if q == c => break,
                        Some(ch) => word.push(ch),
                        None => return None, // unterminated: env fails
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
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

/// An open `( ... )`, `{ ...; }`, or substitution: what its commands read on stdin, and the
/// commands inside it.
struct Group {
    stdin: Vec<usize>,
    ids: Vec<usize>,
    kind: GroupKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupKind {
    Paren,
    Brace,
    /// The inside of `$(...)`, backticks, `<(...)` (the command's stdin) or `>(...)` (the
    /// command's output: unknown).
    Sub,
}

/// Producer id that is never a command: stdin that cannot be told (`>(...)`).
const UNKNOWN_STDIN: usize = usize::MAX;

/// Stdin a command inherits when nothing is piped into it: that of the nearest enclosing
/// group that has one (`echo x | { ( bash ); }`).
fn inherited_stdin(groups: &[Group]) -> Vec<usize> {
    groups
        .iter()
        .rev()
        .find(|g| !g.stdin.is_empty())
        .map(|g| g.stdin.clone())
        .unwrap_or_default()
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
            let closes_brace = words.len() == 1
                && words[0] == "}"
                && groups.last().is_some_and(|g| g.kind == GroupKind::Brace);
            let mut closed = None;
            if closes_brace {
                closed = groups.pop().map(|g| g.ids);
                words.clear();
            }
            let only_redirects = words.is_empty();
            if !words.is_empty() || !redirects.is_empty() {
                let id = cur_id.take().unwrap_or_else(|| {
                    next_id += 1;
                    next_id - 1
                });
                let mut stdin_from = std::mem::take(&mut pipe_from);
                if stdin_from.is_empty() {
                    stdin_from = inherited_stdin(&groups);
                }
                for g in &mut groups {
                    g.ids.push(id);
                }
                if words.first().is_some_and(|w| w == "{") {
                    groups.push(Group {
                        stdin: stdin_from.clone(),
                        ids: vec![id],
                        kind: GroupKind::Brace,
                    });
                }
                items.push(Item::Cmd(Cmd {
                    id,
                    words: std::mem::take(&mut words),
                    redirects: std::mem::take(&mut redirects),
                    stdin_from,
                }));
                emitted = Some(id);
                // `( ... ) 2>/dev/null | sh`: redirections of the group, not a new producer.
                if !only_redirects {
                    last_group = None;
                }
            }
            cur_id = None;
            if closed.is_some() {
                last_group = closed;
            }
        };
    }
    // Start a substitution at `start`: the current command waits until it closes.
    macro_rules! open_sub {
        ($start:expr, $output:expr) => {
            if !in_word {
                word.clear();
            }
            let sub_stdin: Vec<usize> = if $output {
                vec![UNKNOWN_STDIN]
            } else if !pipe_from.is_empty() {
                pipe_from.clone()
            } else {
                inherited_stdin(&groups)
            };
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
            if !sub_stdin.is_empty() {
                groups.push(Group {
                    stdin: sub_stdin,
                    ids: Vec::new(),
                    kind: GroupKind::Sub,
                });
            }
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
                    while groups.last().is_some_and(|g| g.kind != GroupKind::Sub) {
                        let g = groups.pop().expect("checked");
                        if g.kind == GroupKind::Paren {
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
                open_sub!(i, false);
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
                let mut stdin = std::mem::take(&mut pipe_from);
                if stdin.is_empty() {
                    stdin = inherited_stdin(&groups);
                }
                groups.push(Group {
                    stdin,
                    ids: Vec::new(),
                    kind: GroupKind::Paren,
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
                    open_sub!(i, false);
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
                open_sub!(i, c == '>');
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

    #[test]
    fn env_split_string_follows_env_not_the_shell() {
        // Plain words and quotes split the same way: parsed.
        for c in [
            "env -S 'git commit -m x'",
            "env -S 'git commit -m \"a b\"'",
            "env -S \"git commit -m 'a b'\"",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        // env expands `${VAR}`, has its own escapes and comments, and runs no shell
        // operators: anything where it differs from the shell is opaque.
        for c in [
            "env -S '${G} commit'",
            "env -S 'git\\_commit'",
            "env -S 'echo x # git commit'",
            "env -S 'git commit; rm x'",
            "env -S 'git commit | cat'",
            "env -S 'git commit && x'",
            "env -S 'git `x` commit'",
            "env -S 'git $(x) commit'",
            "env -S 'git commit > f'",
            "env -S 'git commit < f'",
            "env -S 'git (commit)'",
            "env -S 'git \\c commit'",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn redirected_groups_keep_their_pipe() {
        for c in [
            "( echo 'git commit -m x' ) 2>/dev/null | bash",
            "(echo 'git commit -m x') >&2 2>/dev/null | bash",
            "{ echo 'git commit -m x'; } 2>/dev/null | bash",
        ] {
            assert!(seen_commit(c) || opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        // A redirect-only command piped in: its output cannot be told.
        assert!(opaque_call("> f | bash"));
    }

    #[test]
    fn nested_groups_inherit_stdin() {
        for c in [
            "echo 'git commit -m x' | { ( bash ) ; }",
            "echo 'git commit -m x' | ( ( bash ) )",
            "echo 'git commit -m x' | ( { bash; } )",
            "echo 'git commit -m x' | { cd a; { bash; }; }",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn substitutions_read_the_commands_stdin() {
        // `$(...)` and `<(...)` inherit the stdin of the command they are in; `>(...)` reads
        // that command's output.
        for c in [
            "echo 'git commit -m x' | echo $(bash)",
            "echo 'git commit -m x' | cat <(bash)",
            "echo 'git commit -m x' | x=`bash` true",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        assert!(opaque_call("echo 'git commit' | tee >(bash)"));
        assert!(opaque_call("tee >(bash) < f"));
    }

    #[test]
    fn echo_and_printf_output_that_cannot_be_read_is_opaque() {
        for c in [
            "printf '%s\\n' \"$X\" | bash",
            "printf '%b' 'git commit' | bash",
            "echo -e 'gi\\x74 commit' | bash",
            "echo \"$X\" | bash",
            "printf 'gi\\164 commit' | bash",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn words_the_shell_expands_are_opaque() {
        for c in [
            "{git,x} commit",
            "git {commit,}",
            "/usr/bin/gi? commit",
            "git $CMD",
            "git comm?t",
            "git \"$C\" -m x",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(!opaque_call("git log -- '*.md'"));
    }

    #[test]
    fn xargs_and_find_replacements() {
        for c in [
            "echo commit | xargs git",
            "echo x | xargs -I{} {} commit",
            "echo x | xargs -i git {} -m x",
            "echo x | xargs --replace=Z Z commit",
            "find . -exec {} commit \\;",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(!opaque_call("ls | xargs grep x"));
        assert!(!opaque_call("git ls-files | xargs git add"));
    }

    #[test]
    fn commands_that_run_a_script_argument() {
        for c in [
            "shopt -s expand_aliases; alias g=git\ng commit -m x",
            "alias g='git commit'; g -m x",
            "trap 'git commit -m x' EXIT",
            "watch 'git commit -m x'",
            "watch -n 5 git commit -m x",
            "su -c 'git commit -m x' me",
            "flock /tmp/l git commit -m x",
            "flock -w 5 /tmp/l -c 'git commit -m x'",
            "script -qc 'git commit -m x' /dev/null",
        ] {
            assert!(seen_commit(c) || opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn xargs_options_are_read_from_a_table() {
        for c in [
            "echo x | xargs -0I{} {} commit",
            "echo x | xargs -tI {} {} commit",
            "echo x | xargs -0i git {} -m x",
            "echo x | xargs -iZ Z commit",
            "echo x | xargs --replace=Z Z commit",
            "echo x | xargs --bogus git status",
            "echo x | xargs -q git status",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(!opaque_call("ls | xargs -0 -n 1 grep x"));
        assert!(!opaque_call("ls | xargs -I{} git log -1 -- {}"));
    }

    #[test]
    fn missing_commands_are_opaque() {
        for c in [
            // Under xargs the command comes from stdin.
            "echo git commit | xargs",
            "echo git commit | xargs sudo",
            "echo git commit | xargs env",
            "echo git commit | xargs timeout 5",
            "echo git commit | xargs flock /tmp/l",
            "echo git commit | xargs watch",
            "echo git commit | xargs nohup",
            "echo x | xargs su -c",
            // A runner with no command at all.
            "flock /tmp/l",
            "watch -n 1",
            "sudo -u me",
            "timeout 5",
            "setsid",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        for c in [
            "command -v git",
            "sudo -l git",
            "env",
            "nice",
            "flock 9",
            "trap -p",
        ] {
            assert!(git_calls(c).is_empty(), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn wrapper_options_are_read_from_tables() {
        for c in [
            "bash -c -e 'git commit -m x'",
            "bash -ec 'git commit -m x'",
            "bash +e -c 'git commit -m x'",
            "sh -o errexit -c 'git commit -m x'",
            "sudo -u me -- git commit -m x",
            "sudo -iu me git commit -m x",
            "nice -10 git commit -m x",
            "nice -n 5 git commit -m x",
            "timeout -k 5 -s KILL 30 git commit -m x",
            "timeout --signal=KILL 30 git commit -m x",
            "stdbuf -oL git commit -m x",
            "ionice -c 3 git commit -m x",
            "time -p git commit -m x",
            "exec -a x git commit -m x",
            "echo 'git commit -m x' | sudo -s",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        for c in [
            "bash --bogus -c 'git commit'",
            "bash -c",
            "sudo -h host git status",
            "sudo --bogus git status",
            "timeout --bogus 5 git status",
            "nice --bogus git status",
            "stdbuf -q git status",
            "time -v git status",
            "sudo -e f",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn su_runuser_and_script_commands() {
        for c in [
            "su -c'git commit -m x' me",
            "su me -c 'git commit -m x'",
            "su -lc 'git commit -m x' me",
            "su --session-command='git commit -m x' me",
            "su --session-command 'git commit -m x' me",
            "runuser -u me git commit -m x",
            "runuser -u me -- git commit -m x",
            "runuser -l me -c 'git commit -m x'",
            "script -c'git commit -m x' /dev/null",
            "script -qec 'git commit -m x' /dev/null",
            "echo 'git commit -m x' | script -q /dev/null",
            "echo 'git commit -m x' | su me",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        for c in [
            "su --bogus me",
            "su me script.sh",
            "runuser -u me",
            "script --bogus",
            "curl x | su me",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn printf_prints_its_format() {
        for c in [
            "printf -- 'git commit -m x' | bash",
            "printf 'git commit -m x' extra words | bash",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
        assert!(opaque_call("printf -x 'git commit' | bash"));
        assert!(git_calls("printf -v v 'git commit' | bash").is_empty());
    }

    #[test]
    fn brace_expansion_in_any_group() {
        for c in ["x{y}{git,z} commit", "git co{m}{mit,}", "{a}{b..c} commit"] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn alias_changes_the_parser_cannot_follow() {
        for c in [
            "BASH_ALIASES[g]=git; g commit -m x",
            "BASH_ALIASES+=([g]=git); g commit -m x",
            "declare -A BASH_ALIASES=([g]=git); g commit -m x",
            "alias -x g=git; g commit -m x",
            "alias 'g/x=git'; g commit -m x",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(seen_commit("alias g=git; g commit -m x"));
        assert!(!opaque_call("alias -p; git status"));
    }

    #[test]
    fn watch_exec_takes_an_argv() {
        for c in [
            "watch -x bash -c 'git commit -m x'",
            "watch --exec sh -c 'echo; git commit -m x'",
            "watch -n 1 -x git commit -m x",
            "watch 'git commit -m x'",
        ] {
            assert!(seen_commit(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn eval_and_source_change_this_shell() {
        assert_eq!(dirs("eval 'cd /x'; git commit"), vec![v(&["/x"])]);
        assert_eq!(dirs("eval cd /x && git commit"), vec![v(&["/x"])]);
        assert_eq!(
            git_commits("eval 'export GIT_DIR=/x/.git'; git commit")[0]
                .git_dir
                .as_deref(),
            Some("/x/.git")
        );
        assert_eq!(dirs("source <(echo cd /x); git commit"), vec![v(&["/x"])]);
        assert!(seen_commit("eval 'alias g=git'; g commit -m x"));
        for c in [
            "eval 'BASH_ALIASES[g]=git'; g commit -m x",
            // A sourced file can change anything about this shell.
            "source ./env.sh; git commit -m x",
            ". ./env.sh && git commit -m x",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        // A subshell still does not leak.
        assert_eq!(dirs("(eval 'cd /x'); git commit"), vec![none()]);
    }

    #[test]
    fn assignments_the_parser_cannot_follow() {
        for c in [
            "declare -n X=BASH_ALIASES; X[g]=git; g commit -m x",
            "typeset -n X=BASH_ALIASES; git commit -m x",
            "export \"$V\"=../repo/.git; git commit -m x",
            "declare $N=x; git commit -m x",
            "read GIT_DIR <<< x; export GIT_DIR; git commit -m x",
            "read -r v GIT_DIR <<< 'a b'; git commit -m x",
            "printf -v GIT_DIR x; export GIT_DIR; git commit -m x",
            "mapfile -t GIT_DIR < f; git commit -m x",
            "readarray PATH < f; git commit -m x",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        // Reading into an ordinary variable is fine.
        assert!(!opaque_call("read -r line <<< x; git commit -m x"));
    }

    #[test]
    fn function_definitions_are_not_run_where_defined() {
        for c in [
            "f() { export GIT_DIR=/x/.git; }; git commit -m x",
            "function f { cd /x; }; git commit -m x",
            "f () { cd /x; }; f; git commit -m x",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
    }

    #[test]
    fn a_lone_dash_ends_shell_options() {
        assert!(seen_commit("echo 'git commit -m x' | bash -"));
        assert!(seen_commit("bash -c - 'git commit -m x'"));
        assert!(seen_commit("bash -e - <<'EOF'\ngit commit -m x\nEOF"));
        // `bash - FILE` runs FILE, not stdin.
        let c = "echo 'git commit -m x' | bash - x.sh";
        assert!(!seen_commit(c), "{c}: {:?}", git_calls(c));
        assert!(!analyze(c).writes.is_empty());
    }

    #[test]
    fn su_with_another_shell_is_opaque() {
        for c in [
            "su -s /bin/zsh me -c 'git commit -m x'",
            "su --shell=/bin/fish me -c 'git commit -m x'",
            "su -m me -c 'git commit -m x'",
            "su -p me -c 'git commit -m x'",
            "su --preserve-environment me -c 'git commit -m x'",
            "su me extra -c 'git commit -m x'",
            "runuser -s /bin/zsh -l me -c 'git commit -m x'",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        let c = "su - me -c 'git commit -m x'";
        assert!(seen_commit(c) && !opaque_call(c), "{c}: {:?}", git_calls(c));
    }

    #[test]
    fn shell_state_from_exec_and_set() {
        // `exec` with only redirections changes stdin for every later command.
        for c in [
            "exec < <(echo git commit -m x); bash",
            "exec 0<script.sh; sh",
            "exec <<< 'git commit -m x'; bash",
        ] {
            assert!(opaque_call(c), "{c}: {:?}", git_calls(c));
        }
        assert!(!opaque_call("exec 2>/dev/null; git status"));
        // Review: every stdin redirect of exec, namerefs, printf -vNAME, clustered set flags.
        for c in [
            "exec <<-EOF\n\tgit commit -m x\n\tEOF\ngit status",
            "declare -n r=GIT_DIR; r=x/.git; git status",
            "printf -vGIT_DIR %s x/.git; git status",
            "printf -x y; git status",
        ] {
            assert!(opaque_call(c), "{c}");
        }
        // `set -a` exports every later assignment.
        for c in [
            "set -a; GIT_DIR=r/.git; git commit",
            "set -ea; GIT_DIR=r/.git; git commit",
            "set -o allexport; GIT_DIR=r/.git; git commit",
            "set -ao allexport; GIT_DIR=r/.git; git commit",
            "set -oa allexport; GIT_DIR=r/.git; git commit",
        ] {
            assert_eq!(git_commits(c)[0].git_dir.as_deref(), Some("r/.git"), "{c}");
        }
        assert_eq!(
            git_commits("set -a; set +a; GIT_DIR=r/.git; git commit")[0].git_dir,
            None
        );
    }
}
