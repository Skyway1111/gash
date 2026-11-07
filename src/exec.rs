//! Command execution.

use crate::ast::*;
use crate::io::Handle;
use crate::parser::{ParseError, Parser};
use crate::path;
use crate::shell::*;
use crate::vars::*;
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

pub fn exit_code(st: ExitStatus) -> i32 {
    match st.code() {
        Some(c) => {
            let u = c as u32;
            if u == 0xC000_013A {
                130
            } else if u == 0xC000_0005 || u == 0xC000_00FD {
                139
            } else if u > 255 {
                (u & 0xff) as i32
            } else {
                c
            }
        }
        None => 1,
    }
}

/// Exit status of an MSYS2/Cygwin child: a process killed by a signal exits with
/// Windows code `signal << 8` (plus 0x80 if it dumped core), which bash reports as 128+signal.
pub fn msys_exit_code(st: ExitStatus) -> i32 {
    if let Some(c) = st.code() {
        let u = c as u32;
        let sig = u >> 8;
        if u > 255 && (u & 0x7f) == 0 && (1..65).contains(&sig) {
            return 128 + sig as i32;
        }
    }
    exit_code(st)
}

pub fn io_err_msg(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        NotFound => "No such file or directory".into(),
        PermissionDenied => "Permission denied".into(),
        AlreadyExists => "File exists".into(),
        IsADirectory => "Is a directory".into(),
        NotADirectory => "Not a directory".into(),
        DirectoryNotEmpty => "Directory not empty".into(),
        BrokenPipe => "Broken pipe".into(),
        InvalidInput => "Invalid argument".into(),
        _ => match e.raw_os_error() {
            Some(267) => "Not a directory".into(),
            Some(5) => "Permission denied".into(),
            Some(32) => "Device or resource busy".into(),
            Some(123) => "No such file or directory".into(),
            _ => {
                let s = e.to_string();
                match s.find(" (os error") {
                    Some(i) => s[..i].to_string(),
                    None => s,
                }
            }
        },
    }
}

pub enum RedirErr {
    Failed,
    Flow(Flow),
}

#[derive(Default)]
pub struct Saved(pub Vec<(i32, Option<Handle>)>);

enum Kind {
    Func(Arc<FuncDef>),
    Builtin,
    Ext(Resolved),
    NotFound,
}

enum Ctl {
    Next(i32),
    Break,
}

fn has_path_sep(name: &str) -> bool {
    name.contains('/') || name.contains('\\') || path::is_windows_abs(name)
}

/// Windows caps a command line at 32,767 characters. Past this length (program and arguments,
/// counted in UTF-8 bytes, which is never less than the UTF-16 length) arguments for an MSYS
/// program go in a response file instead.
const MSYS_CMDLINE_MAX: usize = 30_000;

/// Add `args`, already quoted with `msys_quote`, to an MSYS program's command line. When they
/// would make it longer than Windows allows, they go in a response file and the program gets
/// `@"<file>"`, which the MSYS runtime replaces with the file's contents, parsed like a
/// command line. (Git Bash hands arguments from one MSYS process to the next without the
/// Windows command line, so it has no such limit.) Returns that file: keep it until the
/// program has read it.
fn add_msys_args(cmd: &mut Proc, program: &Path, args: Vec<String>) -> Option<std::fs::File> {
    let len = program.as_os_str().len() + args.iter().map(|a| a.len() + 1).sum::<usize>();
    if len > MSYS_CMDLINE_MAX {
        if let Ok((f, file)) = crate::io::response_file(args.join(" ").as_bytes()) {
            // The quote goes after the @: a quoted @ is taken literally.
            cmd.raw_arg(format!("@\"{}\"", file.display()));
            return Some(f);
        }
    }
    for a in args {
        cmd.raw_arg(a);
    }
    None
}

/// The MSYS runtime globs each quoted argument it gets from a native parent through an
/// 8,192-character buffer (glob.cc's MAXPATHLEN), so longer ones arrive cut short.
const MSYS_GLOB_MAX: usize = 8_000;

/// Command for MSYS program `prog` with `args`, run as `typed` (its argv[0]) if given. Also
/// returns `add_msys_args`'s response file and the `MSYS` value to give the command, when it
/// isn't `msys`, the shell's. An argument too long for the runtime's globbing goes through Git's
/// MSYS `env` with `noglob`, which takes its command line intact, restores `msys` and runs the
/// program, which gets its arguments from an MSYS parent unparsed.
fn msys_cmd(typed: Option<&str>, prog: &Path, args: &[String], msys: Option<String>) -> (Proc, Option<std::fs::File>, Option<String>) {
    if args.iter().any(|a| a.len() > MSYS_GLOB_MAX) {
        let env = path::mounts().root.join(r"usr\bin\env.exe");
        if env.is_file() {
            let mut a = match &msys {
                Some(v) => vec![format!("MSYS={v}")],
                None => vec!["-u".into(), "MSYS".into()],
            };
            a.push(typed.map_or_else(|| path::to_posix(prog), str::to_string));
            a.extend_from_slice(args);
            let mut cmd = Proc::new(&env);
            // Without globbing, `"` and `'` each delimit a literal run, with no escapes.
            let rsp = add_msys_args(&mut cmd, &env, a.iter().map(|s| format!("\"{}\"", s.replace('"', "\"'\"'\""))).collect());
            return (cmd, rsp, Some(msys.map_or("noglob".into(), |v| format!("{v} noglob"))));
        }
    }
    let mut cmd = match typed {
        Some(t) => Proc::new(t),
        None => Proc::new(prog),
    };
    let rsp = add_msys_args(&mut cmd, prog, args.iter().map(|a| msys_quote(a)).collect());
    (cmd, rsp, None)
}

/// Quote an argument for an MSYS2/Cygwin program started from a native process
/// (whose runtime re-parses the command line with globbing).
pub fn msys_quote(a: &str) -> String {
    let needs = a.is_empty()
        || a.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '"' | '\'' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '\\'))
        || a.starts_with('~')
        || a.starts_with('@');
    if !needs {
        return a.to_string();
    }
    let b = a.as_bytes();
    let dos = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    if dos && !a.contains('"') && !a.chars().any(|c| matches!(c, ' ' | '\t' | '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '\'')) {
        return a.to_string();
    }
    let mut s = String::with_capacity(a.len() + 2);
    s.push('"');
    for c in a.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' if !dos => s.push_str("\\\\"),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

pub fn read_script_file(p: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(p)?;
    let mut s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    if s.starts_with('\u{feff}') {
        s.remove(0);
    }
    if s.contains("\r\n") {
        s = s.replace("\r\n", "\n");
    }
    Ok(s)
}

impl Shell {
    // ---------- running source text ----------

    pub fn run_source(&mut self, src: &str, fatal_syntax: bool) -> R {
        self.run_source_top(src, fatal_syntax, false, false)
    }

    /// `top`: this is a top-level reader (main script); `Abort` discards only the current command.
    /// `reader`: bash's reader_loop (a script or stdin, not `-c`), which clears
    /// `stdin_redir` before each command it reads.
    pub fn run_source_top(&mut self, src: &str, fatal_syntax: bool, top: bool, reader: bool) -> R {
        let mut p = Parser::new(src);
        if let Some(l) = self.parse_line_base.take() {
            p.line = l;
        }
        let mut st = 0;
        loop {
            p.aliases = if self.sh[sho::expand_aliases()] { Some(self.aliases.clone()) } else { None };
            match p.parse_next() {
                Ok(None) => break,
                Ok(Some(list)) => {
                    if self.o[O_VERBOSE] {
                        for item in &list {
                            self.write_fd(2, format!("{}\n", item.text).as_bytes());
                        }
                    }
                    if self.o[O_NOEXEC] {
                        continue;
                    }
                    if reader {
                        self.stdin_redir = false;
                    }
                    st = match self.exec_list(&list) {
                        Ok(s) => s,
                        Err(Flow::Abort(s)) if top => {
                            self.status = s;
                            s
                        }
                        Err(e) => return Err(e),
                    };
                }
                Err(e) => {
                    let (msg, line) = match e {
                        ParseError::Incomplete => ("syntax error: unexpected end of file".to_string(), p.line),
                        ParseError::Syntax { msg, line } => (msg, line),
                    };
                    let m = format!("{}: line {}: {}\n", self.source_name, line, msg);
                    self.write_fd(2, m.as_bytes());
                    self.status = 2;
                    if fatal_syntax && !self.interactive {
                        return Err(Flow::Fatal(2));
                    }
                    return Ok(2);
                }
            }
        }
        Ok(st)
    }

    /// Run a whole script (or stdin) as the main program of this shell; returns exit status.
    pub fn run_main(&mut self, src: &str) -> i32 {
        let r = self.run_source_top(src, true, true, true);
        self.finish_main(r)
    }

    /// Run a `-c` command string as the main program of this shell.
    pub fn run_command_string(&mut self, src: &str) -> i32 {
        let r = self.run_source_top(src, true, true, false);
        self.finish_main(r)
    }

    pub fn finish_main(&mut self, r: R) -> i32 {
        let st = match r {
            Ok(s) => s,
            Err(Flow::Exit(s)) | Err(Flow::Fatal(s)) | Err(Flow::Return(s)) | Err(Flow::Abort(s)) => s,
            Err(_) => self.status,
        };
        self.status = st;
        self.run_exit_trap();
        self.join_ps_threads();
        self.status
    }

    /// Close our ends of > >(cmd) pipes and wait for the consumers to finish.
    pub fn join_ps_threads(&mut self) {
        if self.ps_threads.is_empty() {
            return;
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.fds = crate::io::Fds { v: Vec::new() };
        for h in std::mem::take(&mut self.ps_threads) {
            let _ = h.join();
        }
    }

    pub fn finish_subshell(&mut self, r: R) -> i32 {
        let st = match r {
            Ok(s) => s,
            Err(Flow::Exit(s)) | Err(Flow::Fatal(s)) | Err(Flow::Return(s)) | Err(Flow::Abort(s)) => s,
            Err(_) => self.status,
        };
        self.status = st;
        self.run_exit_trap();
        self.join_ps_threads();
        self.status
    }

    pub fn run_exit_trap(&mut self) {
        if self.exit_trap_done {
            return;
        }
        self.exit_trap_done = true;
        if let Some(t) = self.traps.get("EXIT").cloned() {
            if !t.is_empty() {
                let saved = self.status;
                self.in_trap = true;
                let r = self.run_source(&t, false);
                self.in_trap = false;
                match r {
                    Err(Flow::Exit(s)) => self.status = s,
                    _ => self.status = saved,
                }
            }
        }
    }

    /// Run text like `eval`/`source` do: an Abort ends just this text.
    pub fn run_nested(&mut self, src: &str) -> R {
        match self.run_source(src, false) {
            Err(Flow::Abort(s)) => {
                self.status = s;
                Ok(s)
            }
            r => r,
        }
    }

    pub fn run_trap(&mut self, sig: &str) -> Result<(), Flow> {
        if self.in_trap {
            return Ok(());
        }
        if let Some(t) = self.traps.get(sig).cloned() {
            if t.is_empty() {
                return Ok(());
            }
            let saved = self.status;
            self.in_trap = true;
            let saved_cond = self.cond_depth;
            let saved_line = self.lineno;
            self.parse_line_base = Some(self.lineno);
            let r = self.run_source(&t, false);
            self.lineno = saved_line;
            self.cond_depth = saved_cond;
            self.in_trap = false;
            self.status = saved;
            match r {
                Err(Flow::Exit(s)) => return Err(Flow::Exit(s)),
                Err(Flow::Fatal(s)) => return Err(Flow::Fatal(s)),
                _ => {}
            }
        }
        Ok(())
    }

    pub fn check_int(&mut self) -> Result<(), Flow> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Flow::Exit(143));
        }
        if crate::sys::interrupted() {
            if let Some(t) = self.traps.get("INT").cloned() {
                crate::sys::take_interrupt();
                if !t.is_empty() {
                    self.run_trap("INT")?;
                }
                return Ok(());
            }
            return Err(Flow::Fatal(130));
        }
        Ok(())
    }

    // ---------- lists ----------

    pub fn exec_list(&mut self, list: &List) -> R {
        let mut st = self.status;
        for item in list {
            self.check_int()?;
            if item.background {
                st = self.exec_background(item)?;
            } else {
                st = self.exec_and_or(&item.and_or)?;
            }
        }
        Ok(st)
    }

    fn in_cond<T>(&mut self, f: impl FnOnce(&mut Shell) -> T) -> T {
        self.cond_depth += 1;
        let r = f(self);
        self.cond_depth -= 1;
        r
    }

    pub fn exec_and_or(&mut self, ao: &AndOr) -> R {
        let n = ao.rest.len();
        let mut st = if n > 0 { self.in_cond(|s| s.exec_pipeline(&ao.first))? } else { self.exec_pipeline(&ao.first)? };
        for (i, (op, p)) in ao.rest.iter().enumerate() {
            let run = match op {
                AndOrOp::And => st == 0,
                AndOrOp::Or => st != 0,
            };
            if run {
                st = if i + 1 == n { self.exec_pipeline(p)? } else { self.in_cond(|s| s.exec_pipeline(p))? };
            }
        }
        Ok(st)
    }

    pub fn exec_pipeline(&mut self, p: &Pipeline) -> R {
        let timing = if p.time { Some((Instant::now(), crate::sys::total_times())) } else { None };
        let st = if p.cmds.len() <= 1 {
            if p.negate {
                self.cond_depth += 1;
            }
            let r = if p.cmds.is_empty() { Ok(0) } else { self.exec_command(&p.cmds[0].0) };
            if p.negate {
                self.cond_depth -= 1;
            }
            let s = r?;
            self.set_pipestatus(&[s]);
            s
        } else {
            if p.negate {
                self.cond_depth += 1;
            }
            let r = self.exec_multi(p);
            if p.negate {
                self.cond_depth -= 1;
            }
            r?
        };
        let st = if p.negate { (st == 0) as i32 } else { st };
        self.status = st;
        if let Some((t0, (u0, s0))) = timing {
            let real = t0.elapsed().as_secs_f64();
            let (u1, s1) = crate::sys::total_times();
            self.print_times(real, u1 - u0, s1 - s0, p.time_posix);
        }
        if st != 0 && !p.negate && self.cond_depth == 0 && !self.in_trap {
            if self.func_depth == 0 || self.o[O_ERRTRACE] {
                self.run_trap("ERR")?;
            }
            if self.o[O_ERREXIT] {
                return Err(Flow::Exit(st));
            }
        }
        Ok(st)
    }

    fn print_times(&mut self, real: f64, user: f64, sys: f64, posix: bool) {
        let fmt_l = |t: f64| format!("{}m{:.3}s", (t / 60.0) as u64, t % 60.0);
        let s = if posix {
            format!("real {:.2}\nuser {:.2}\nsys {:.2}\n", real, user, sys)
        } else if let Some(tf) = self.get_str("TIMEFORMAT") {
            let mut out = String::new();
            let mut it = tf.chars().peekable();
            while let Some(c) = it.next() {
                if c == '%' {
                    let mut prec = 3usize;
                    let mut long = false;
                    if let Some(d) = it.peek().and_then(|c| c.to_digit(10)) {
                        prec = d as usize;
                        it.next();
                    }
                    if it.peek() == Some(&'l') {
                        long = true;
                        it.next();
                    }
                    let v = match it.next() {
                        Some('R') => real,
                        Some('U') => user,
                        Some('S') => sys,
                        Some('P') => {
                            out.push_str(&format!("{:.*}", prec, if real > 0.0 { (user + sys) / real * 100.0 } else { 0.0 }));
                            continue;
                        }
                        Some('%') => {
                            out.push('%');
                            continue;
                        }
                        _ => continue,
                    };
                    if long {
                        out.push_str(&format!("{}m{:.*}s", (v / 60.0) as u64, prec, v % 60.0));
                    } else {
                        out.push_str(&format!("{:.*}", prec, v));
                    }
                } else if c == '\\' && it.peek() == Some(&'n') {
                    it.next();
                    out.push('\n');
                } else if c == '\\' && it.peek() == Some(&'t') {
                    it.next();
                    out.push('\t');
                } else {
                    out.push(c);
                }
            }
            out.push('\n');
            out
        } else {
            format!("\nreal\t{}\nuser\t{}\nsys\t{}\n", fmt_l(real), fmt_l(user), fmt_l(sys))
        };
        self.write_fd(2, s.as_bytes());
    }

    fn set_pipestatus(&mut self, sts: &[i32]) {
        let m: std::collections::BTreeMap<i64, String> = sts.iter().enumerate().map(|(i, s)| (i as i64, s.to_string())).collect();
        let v = self.vars.global_mut("PIPESTATUS");
        v.value = Some(Value::Arr(m));
    }

    fn exec_multi(&mut self, p: &Pipeline) -> R {
        let n = p.cmds.len();
        let lastpipe = self.sh[sho::lastpipe()] && !self.o[O_MONITOR];
        let mut threads: Vec<std::thread::JoinHandle<i32>> = Vec::new();
        let mut prev: Option<Handle> = None;
        let mut last_status: Option<i32> = None;
        for (i, (cmd, pipe_err)) in p.cmds.iter().enumerate() {
            let last = i + 1 == n;
            let (r, w) = if !last {
                match std::io::pipe() {
                    Ok((r, w)) => (Some(Handle::PipeR(Arc::new(r))), Some(Handle::PipeW(Arc::new(w)))),
                    Err(e) => {
                        self.eprint(&format!("pipe error: {}", e));
                        return Ok(1);
                    }
                }
            } else {
                (None, None)
            };
            if last && lastpipe {
                let saved0 = self.fds.set(0, prev.take());
                let r = self.exec_command(cmd);
                self.fds.set(0, saved0);
                last_status = Some(match r {
                    Ok(s) => s,
                    Err(e) => {
                        // wait for others before propagating
                        for t in threads.drain(..) {
                            let _ = t.join();
                        }
                        return Err(e);
                    }
                });
                break;
            }
            let mut sub = self.subshell_clone();
            sub.forked = Some(prev.is_some());
            if let Some(pr) = prev.take() {
                sub.fds.set(0, Some(pr));
            }
            if let Some(wh) = &w {
                sub.fds.set(1, Some(wh.clone()));
                if *pipe_err {
                    sub.fds.set(2, Some(wh.clone()));
                }
            }
            drop(w);
            let cmd = cmd.clone();
            let h = std::thread::Builder::new()
                .stack_size(64 * 1024 * 1024)
                .spawn(move || {
                    let r = sub.exec_command(&cmd);
                    sub.finish_subshell(r)
                })
                .expect("spawn pipeline thread");
            threads.push(h);
            prev = r;
        }
        drop(prev);
        let mut sts: Vec<i32> = threads.into_iter().map(|t| t.join().unwrap_or(1)).collect();
        if let Some(s) = last_status {
            sts.push(s);
        }
        self.set_pipestatus(&sts);
        let st = if self.o[O_PIPEFAIL] {
            sts.iter().rev().find(|s| **s != 0).copied().unwrap_or(0)
        } else {
            *sts.last().unwrap_or(&0)
        };
        Ok(st)
    }

    fn exec_background(&mut self, item: &ListItem) -> R {
        let mut sub = self.subshell_clone();
        // bash: without job control, `cmd &` reads /dev/null unless stdin was redirected
        // (see Shell::stdin_redir); its own `<` redirections still apply afterwards.
        if !self.interactive && !self.stdin_redir {
            sub.fds.set(0, Some(Handle::Null));
        }
        let id = self.jobs.list.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        sub.cancel = cancel.clone();
        let child_pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        sub.child_pid = child_pid.clone();
        // Fast path: a single external command gets a real process (real $!).
        let ao = &item.and_or;
        if ao.rest.is_empty() && ao.first.cmds.len() == 1 && !ao.first.negate && !ao.first.time {
            if let Command::Simple(sc) = &ao.first.cmds[0].0 {
                if let Some(first) = sc.words.first().and_then(|w| w.as_plain()) {
                    if !self.funcs.contains_key(&first) && !crate::builtins::is_builtin(&first) {
                        sub.spawn_only = true;
                        let r = sub.exec_command(&ao.first.cmds[0].0);
                        if let Some(child) = sub.spawned.take() {
                            let pid = child.id();
                            self.last_bg = Some(pid);
                            if self.interactive {
                                self.write_fd(2, format!("[{}] {}\n", id, pid).as_bytes());
                            }
                            self.jobs.list.push(Job {
                                id,
                                pid,
                                text: item.text.clone(),
                                kind: JobKind::Proc(child),
                                status: None,
                                cancel,
                                child_pid,
                                notified: false,
                            });
                            return Ok(0);
                        }
                        // not found / redirection error etc.: already reported
                        let st = sub.finish_subshell(r);
                        let pid = fake_pid();
                        self.last_bg = Some(pid);
                        self.jobs.list.push(Job {
                            id,
                            pid,
                            text: item.text.clone(),
                            kind: JobKind::Thread(None),
                            status: Some(st),
                            cancel,
                            child_pid,
                            notified: !self.interactive,
                        });
                        return Ok(0);
                    }
                }
            }
        }
        if ao.rest.is_empty() && ao.first.cmds.len() == 1 {
            sub.forked = Some(false);
        }
        let ao = item.and_or.clone();
        let h = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let r = sub.exec_and_or(&ao);
                sub.finish_subshell(r)
            })
            .expect("spawn background thread");
        let pid = fake_pid();
        self.last_bg = Some(pid);
        if self.interactive {
            self.write_fd(2, format!("[{}] {}\n", id, pid).as_bytes());
        }
        self.jobs.list.push(Job {
            id,
            pid,
            text: item.text.clone(),
            kind: JobKind::Thread(Some(h)),
            status: None,
            cancel,
            child_pid,
            notified: false,
        });
        Ok(0)
    }

    // ---------- commands ----------

    pub fn exec_command(&mut self, cmd: &Command) -> R {
        // bash's stdin_redir bookkeeping; see Shell::stdin_redir.
        let forked = self.forked.take();
        let piped = forked == Some(true);
        match cmd {
            Command::Simple(sc) => {
                if piped {
                    // A function or builtin reading a pipe (bash forks it with pipe_in set).
                    self.stdin_redir = true;
                }
                self.exec_simple(sc)
            }
            Command::Compound { kind, redirs, line } => {
                self.lineno = *line;
                let redir = piped || stdin_redirects(redirs);
                if let CompoundKind::Subshell(_) = &**kind {
                    // `( )` decides for itself and leaves the parent's flag alone.
                    let outer = std::mem::replace(&mut self.stdin_redir, redir);
                    let r = self.exec_compound_redirected(kind, redirs);
                    self.stdin_redir = outer;
                    return r;
                }
                if piped {
                    self.stdin_redir = true;
                } else if forked.is_none() && !redirs.is_empty() {
                    // Sticks after the command ends, like bash's global. A forked
                    // compound command's own redirections don't count in bash.
                    self.stdin_redir = redir;
                }
                self.exec_compound_redirected(kind, redirs)
            }
            Command::FuncDef(f) => {
                if crate::builtins::is_special_builtin(&f.name) && self.o[O_POSIX] {
                    self.eprint(&format!("`{}': is a special builtin", f.name));
                    return Ok(1);
                }
                // Remember the file it is defined in, for BASH_SOURCE and error messages.
                let def = FuncDef { name: f.name.clone(), body: f.body.clone(), source: f.source.clone(), file: self.source_name.clone(), line: f.line };
                self.define_function(Arc::new(def));
                Ok(0)
            }
        }
    }

    fn exec_compound_redirected(&mut self, kind: &CompoundKind, redirs: &[Redir]) -> R {
        if redirs.is_empty() {
            return self.exec_compound(kind);
        }
        let saved = match self.apply_redirs(redirs) {
            Ok(s) => s,
            Err(RedirErr::Failed) => return Ok(1),
            Err(RedirErr::Flow(f)) => return Err(f),
        };
        let r = self.exec_compound(kind);
        self.restore_fds(saved);
        r
    }

    fn loop_body(&mut self, body: &List) -> Result<Ctl, Flow> {
        match self.exec_list(body) {
            Ok(s) => Ok(Ctl::Next(s)),
            Err(Flow::Break(n)) => {
                if n > 1 {
                    Err(Flow::Break(n - 1))
                } else {
                    Ok(Ctl::Break)
                }
            }
            Err(Flow::Continue(n)) => {
                if n > 1 {
                    Err(Flow::Continue(n - 1))
                } else {
                    Ok(Ctl::Next(0))
                }
            }
            Err(e) => Err(e),
        }
    }

    fn exec_compound(&mut self, kind: &CompoundKind) -> R {
        match kind {
            CompoundKind::Subshell(list) => {
                let mut sub = self.subshell_clone();
                let r = sub.exec_list(list);
                let st = sub.finish_subshell(r);
                Ok(st)
            }
            CompoundKind::Group(list) => self.exec_list(list),
            CompoundKind::If { conds, else_body } => {
                for (c, b) in conds {
                    let cs = self.in_cond(|s| s.exec_list(c))?;
                    if cs == 0 {
                        return self.exec_list(b);
                    }
                }
                match else_body {
                    Some(b) => self.exec_list(b),
                    None => Ok(0),
                }
            }
            CompoundKind::While { cond, body, until } => {
                self.loop_depth += 1;
                let r = self.run_while(cond, body, *until);
                self.loop_depth -= 1;
                r
            }
            CompoundKind::For { var, words, body } => {
                let items = match words {
                    Some(ws) => self.expand_words(ws)?,
                    None => self.positional.clone(),
                };
                self.loop_depth += 1;
                let r = self.run_for(var, items, body);
                self.loop_depth -= 1;
                r
            }
            CompoundKind::ArithFor { init, cond, step, body } => {
                self.loop_depth += 1;
                let r = self.run_arith_for(init.as_ref(), cond.as_ref(), step.as_ref(), body);
                self.loop_depth -= 1;
                r
            }
            CompoundKind::Select { var, words, body } => {
                let items = match words {
                    Some(ws) => self.expand_words(ws)?,
                    None => self.positional.clone(),
                };
                self.loop_depth += 1;
                let r = self.run_select(var, items, body);
                self.loop_depth -= 1;
                r
            }
            CompoundKind::Case { word, arms } => {
                let w = self.expand_word_str(word)?;
                let nocase = self.sh[sho::nocasematch()];
                let ext = self.sh[sho::extglob()];
                let mut st = 0;
                let mut i = 0;
                let mut fall = false;
                while i < arms.len() {
                    let arm = &arms[i];
                    let mut matched = fall;
                    if !matched {
                        for p in &arm.patterns {
                            let pat = self.expand_word_pat(p)?;
                            if crate::pattern::pattern_match_ext(&pat, &w, nocase, ext) {
                                matched = true;
                                break;
                            }
                        }
                    }
                    if matched {
                        st = self.exec_list(&arm.body)?;
                        match arm.term {
                            CaseTerm::Break => return Ok(st),
                            CaseTerm::FallThrough => fall = true,
                            CaseTerm::Continue => fall = false,
                        }
                    }
                    i += 1;
                }
                Ok(st)
            }
            CompoundKind::Cond(e) => {
                if self.o[O_XTRACE] {
                    let t = self.cond_trace(e);
                    self.trace_line(&format!("[[ {} ]]", t));
                }
                let r = self.eval_cond(e)?;
                Ok(r)
            }
            CompoundKind::Arith(w) => {
                let s = self.expand_word_str(w)?;
                if self.o[O_XTRACE] {
                    self.trace_line(&format!("(( {} ))", s));
                }
                match crate::arith::eval(&s, self) {
                    Ok(v) => Ok(if v != 0 { 0 } else { 1 }),
                    Err(e) => {
                        self.eprint(&format!("{}: {}", s.trim_start(), e));
                        Ok(1)
                    }
                }
            }
        }
    }

    fn run_while(&mut self, cond: &List, body: &List, until: bool) -> R {
        let mut st = 0;
        loop {
            self.check_int()?;
            let cs = match self.in_cond(|s| s.exec_list(cond)) {
                Ok(c) => c,
                Err(Flow::Break(n)) => {
                    if n > 1 {
                        return Err(Flow::Break(n - 1));
                    }
                    break;
                }
                Err(Flow::Continue(n)) => {
                    if n > 1 {
                        return Err(Flow::Continue(n - 1));
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            if (cs == 0) == until {
                break;
            }
            match self.loop_body(body)? {
                Ctl::Next(s) => st = s,
                Ctl::Break => break,
            }
        }
        Ok(st)
    }

    fn run_for(&mut self, var: &str, items: Vec<String>, body: &List) -> R {
        let mut st = 0;
        let trace = if self.o[O_XTRACE] {
            Some(format!("for {} in {}", var, items.iter().map(|s| crate::expand::shell_quote(s)).collect::<Vec<_>>().join(" ")))
        } else {
            None
        };
        for it in items {
            self.check_int()?;
            if let Some(t) = &trace {
                if self.o[O_XTRACE] {
                    self.trace_line(t);
                }
            }
            if let Err(e) = self.set_var(var, &it, false) {
                self.eprint(&e);
                return Err(Flow::Abort(1));
            }
            match self.loop_body(body)? {
                Ctl::Next(s) => st = s,
                Ctl::Break => break,
            }
        }
        Ok(st)
    }

    fn arith_status(&mut self, w: &Word) -> Result<i64, Flow> {
        let s = self.expand_word_str(w)?;
        match crate::arith::eval(&s, self) {
            Ok(v) => Ok(v),
            Err(e) => {
                self.eprint(&format!("{}: {}", s.trim_start(), e));
                Err(Flow::Abort(1))
            }
        }
    }

    fn run_arith_for(&mut self, init: Option<&Word>, cond: Option<&Word>, step: Option<&Word>, body: &List) -> R {
        let mut st = 0;
        if let Some(w) = init {
            self.arith_status(w)?;
        }
        loop {
            self.check_int()?;
            if let Some(w) = cond {
                if self.arith_status(w)? == 0 {
                    break;
                }
            }
            match self.loop_body(body)? {
                Ctl::Next(s) => st = s,
                Ctl::Break => break,
            }
            if let Some(w) = step {
                self.arith_status(w)?;
            }
        }
        Ok(st)
    }

    fn run_select(&mut self, var: &str, items: Vec<String>, body: &List) -> R {
        let mut st = 0;
        let mut show = true;
        loop {
            self.check_int()?;
            if show {
                let mut menu = String::new();
                for (i, it) in items.iter().enumerate() {
                    menu.push_str(&format!("{}) {}\n", i + 1, it));
                }
                self.write_fd(2, menu.as_bytes());
            }
            let ps3 = self.get_str("PS3").unwrap_or_else(|| "#? ".into());
            self.write_fd(2, ps3.as_bytes());
            let h = match self.fds.get(0) {
                Some(h) => h.clone(),
                None => return Ok(1),
            };
            let (line, found) = h.read_until(b'\n', None).unwrap_or_default();
            if !found && line.is_empty() {
                self.write_fd(2, b"\n");
                return Ok(1);
            }
            let line = String::from_utf8_lossy(&line).trim_end_matches('\r').to_string();
            let _ = self.set_var("REPLY", &line, false);
            if line.trim().is_empty() {
                show = true;
                continue;
            }
            show = false;
            let choice = line.trim().parse::<usize>().ok().and_then(|n| if n >= 1 { items.get(n - 1).cloned() } else { None });
            let _ = self.set_var(var, &choice.unwrap_or_default(), false);
            match self.loop_body(body)? {
                Ctl::Next(s) => st = s,
                Ctl::Break => break,
            }
        }
        Ok(st)
    }

    // ---------- [[ ]] ----------

    /// Render a [[ ]] expression for xtrace (expanding words that have no side effects).
    fn cond_trace(&mut self, e: &CondExpr) -> String {
        let word = |s: &mut Shell, w: &Word| -> String {
            let pure = !w.parts.iter().any(|p| {
                matches!(p, WordPart::CmdSubst(_) | WordPart::Backtick(_) | WordPart::ProcSubst { .. } | WordPart::Arith(_))
                    || matches!(p, WordPart::DQuoted(inner) if inner.iter().any(|q| matches!(q, WordPart::CmdSubst(_) | WordPart::Backtick(_) | WordPart::Arith(_))))
                    || matches!(p, WordPart::Param(pe) if !matches!(pe.op, ParamOp::None | ParamOp::Length))
            });
            if pure {
                let saved = s.status;
                let v = s.expand_word_str(w).unwrap_or_else(|_| w.to_source());
                s.status = saved;
                crate::expand::shell_quote(&v)
            } else {
                w.to_source()
            }
        };
        match e {
            CondExpr::And(a, b) => format!("{} && {}", self.cond_trace(a), self.cond_trace(b)),
            CondExpr::Or(a, b) => format!("{} || {}", self.cond_trace(a), self.cond_trace(b)),
            CondExpr::Not(a) => format!("! {}", self.cond_trace(a)),
            CondExpr::Word(w) => word(self, w),
            CondExpr::Unary(op, w) => format!("{} {}", op, word(self, w)),
            CondExpr::Binary(l, op, r) => {
                let ls = word(self, l);
                let rs = if op == "==" || op == "!=" || op == "=" || op == "=~" { r.to_source() } else { word(self, r) };
                let rs = if r.as_plain().is_some() || op == "=~" { rs } else { word(self, r) };
                format!("{} {} {}", ls, op, rs)
            }
        }
    }

    pub fn eval_cond(&mut self, e: &CondExpr) -> R {
        match e {
            CondExpr::And(a, b) => {
                let r = self.eval_cond(a)?;
                if r != 0 {
                    return Ok(r);
                }
                self.eval_cond(b)
            }
            CondExpr::Or(a, b) => {
                let r = self.eval_cond(a)?;
                if r == 0 {
                    return Ok(0);
                }
                self.eval_cond(b)
            }
            CondExpr::Not(a) => {
                let r = self.eval_cond(a)?;
                Ok(if r == 0 { 1 } else if r == 1 { 0 } else { r })
            }
            CondExpr::Word(w) => {
                let s = self.expand_word_str(w)?;
                Ok(if s.is_empty() { 1 } else { 0 })
            }
            CondExpr::Unary(op, w) => {
                let s = self.expand_word_str(w)?;
                Ok(if crate::builtins::test_unary(self, op, &s) { 0 } else { 1 })
            }
            CondExpr::Binary(l, op, r) => {
                let ls = self.expand_word_str(l)?;
                let nocase = self.sh[sho::nocasematch()];
                match op.as_str() {
                    "==" | "=" | "!=" => {
                        let p = self.expand_word_pat(r)?;
                        let m = crate::pattern::pattern_match(&p, &ls, nocase);
                        Ok(if m != (op == "!=") { 0 } else { 1 })
                    }
                    "=~" => {
                        let re = self.expand_word_regex(r)?;
                        match crate::regex::Regex::new(&re, nocase) {
                            Ok(rx) => match rx.captures(&ls) {
                                Some(caps) => {
                                    let m: std::collections::BTreeMap<i64, String> =
                                        caps.into_iter().enumerate().map(|(i, c)| (i as i64, c.unwrap_or_default())).collect();
                                    let v = self.vars.global_mut("BASH_REMATCH");
                                    v.value = Some(Value::Arr(m));
                                    v.attrs |= ARRAY;
                                    Ok(0)
                                }
                                None => {
                                    let v = self.vars.global_mut("BASH_REMATCH");
                                    v.value = Some(Value::Arr(Default::default()));
                                    Ok(1)
                                }
                            },
                            Err(_) => Ok(2),
                        }
                    }
                    _ => {
                        let rs = self.expand_word_str(r)?;
                        match crate::builtins::test_binary(self, &ls, op, &rs, true) {
                            Ok(b) => Ok(if b { 0 } else { 1 }),
                            Err(m) => {
                                self.eprint(&m);
                                Ok(2)
                            }
                        }
                    }
                }
            }
        }
    }

    // ---------- simple commands ----------

    fn expand_decl_words(&mut self, words: &[Word], arrays: &mut HashMap<usize, Vec<(Option<String>, String)>>) -> Result<Vec<String>, Flow> {
        let mut out = Vec::new();
        for w in words {
            if let [WordPart::DeclAssign(a)] = w.parts.as_slice() {
                let lhs = match &a.index {
                    Some(i) => format!("{}[{}]", a.name, self.expand_word_str(i)?),
                    None => a.name.clone(),
                };
                let op = if a.append { "+=" } else { "=" };
                match &a.value {
                    AssignValue::Scalar(v) => {
                        let s = self.expand_word_str(v)?;
                        out.push(format!("{}{}{}", lhs, op, s));
                    }
                    AssignValue::Array(elems) => {
                        let ev = self.expand_array_elems(elems)?;
                        arrays.insert(out.len(), ev);
                        out.push(format!("{}{}(...)", lhs, op));
                    }
                }
            } else {
                self.expand_word_into(w, &mut out)?;
            }
        }
        Ok(out)
    }

    pub fn expand_array_elems(&mut self, elems: &[(Option<Word>, Word)]) -> Result<Vec<(Option<String>, String)>, Flow> {
        let mut out = Vec::new();
        for (k, v) in elems {
            match k {
                Some(k) => {
                    let ks = self.expand_word_str(k)?;
                    let vs = self.expand_word_str(v)?;
                    out.push((Some(ks), vs));
                }
                None => {
                    let mut vals = Vec::new();
                    self.expand_word_into(v, &mut vals)?;
                    for x in vals {
                        out.push((None, x));
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn do_assign(&mut self, a: &Assign) -> Result<(), Flow> {
        let r = match &a.value {
            AssignValue::Scalar(w) => {
                let v = self.expand_word_str(w)?;
                match &a.index {
                    Some(iw) => {
                        let k = self.expand_word_str(iw)?;
                        self.set_elem(&a.name, &k, &v, a.append)
                    }
                    None => self.set_var(&a.name, &v, a.append),
                }
            }
            AssignValue::Array(elems) => {
                let ev = self.expand_array_elems(elems)?;
                self.set_array(&a.name, ev, a.append)
            }
        };
        if let Err(e) = r {
            self.eprint(&e);
            self.status = 1;
            return Err(Flow::Abort(1));
        }
        Ok(())
    }

    pub fn trace_line(&mut self, s: &str) {
        let ps4 = self.get_str("PS4").unwrap_or_else(|| "+ ".into());
        let saved = self.o[O_XTRACE];
        self.o[O_XTRACE] = false;
        let ps4 = self.expand_string(&ps4).unwrap_or(ps4);
        self.o[O_XTRACE] = saved;
        let first = ps4.chars().next().map(|c| c.to_string()).unwrap_or_default();
        let lvl = first.repeat(self.subshell.min(10));
        self.write_fd(2, format!("{}{}{}\n", lvl, ps4, s).as_bytes());
    }

    fn exec_simple(&mut self, sc: &SimpleCommand) -> R {
        self.lineno = sc.line;
        self.cmdsubst_status = None;
        let has_decl = sc.words.iter().any(|w| matches!(w.parts.as_slice(), [WordPart::DeclAssign(_)]));
        let mut decl_arrays = HashMap::new();
        let argv = if has_decl { self.expand_decl_words(&sc.words, &mut decl_arrays)? } else { self.expand_words(&sc.words)? };

        if argv.is_empty() {
            for a in &sc.assigns {
                if let Err(e) = self.do_assign(a) {
                    self.finish_procsubst();
                    return Err(e);
                }
                if self.o[O_XTRACE] {
                    let v = self.get_str(&a.name).unwrap_or_default();
                    self.trace_line(&format!("{}={}", a.name, crate::expand::shell_quote(&v)));
                }
            }
            let st = self.cmdsubst_status.unwrap_or(0);
            let saved = match self.apply_redirs(&sc.redirs) {
                Ok(s) => s,
                Err(RedirErr::Failed) => {
                    self.finish_procsubst();
                    return Ok(1);
                }
                Err(RedirErr::Flow(f)) => return Err(f),
            };
            if self.keep_redirs {
                self.keep_redirs = false;
            } else {
                self.restore_fds(saved);
            }
            self.finish_procsubst();
            return Ok(st);
        }

        let mut env_assigns: Vec<(String, String)> = Vec::new();
        for a in &sc.assigns {
            if let AssignValue::Scalar(w) = &a.value {
                let mut v = self.expand_word_str(w)?;
                if a.append {
                    v = self.get_str(&a.name).unwrap_or_default() + &v;
                }
                env_assigns.push((a.name.clone(), v));
            }
        }
        if self.o[O_XTRACE] {
            let mut parts: Vec<String> = env_assigns.iter().map(|(k, v)| format!("{}={}", k, crate::expand::shell_quote(v))).collect();
            parts.extend(argv.iter().map(|a| crate::expand::shell_quote(a)));
            self.trace_line(&parts.join(" "));
        }
        let name = argv[0].clone();
        let kind = self.classify(&name, &env_assigns);
        let saved = match self.apply_redirs(&sc.redirs) {
            Ok(s) => s,
            Err(RedirErr::Failed) => {
                self.finish_procsubst();
                return Ok(1);
            }
            Err(RedirErr::Flow(f)) => return Err(f),
        };
        let r = match kind {
            Kind::Func(f) => self.with_temp_assigns(&env_assigns, |s| s.run_function(&f, &argv[1..])),
            Kind::Builtin => {
                let saved_decl = std::mem::replace(&mut self.decl_arrays, decl_arrays);
                let r = self.with_temp_assigns(&env_assigns, |s| crate::builtins::run(s, &argv));
                self.decl_arrays = saved_decl;
                r
            }
            Kind::Ext(res) => self.run_external(&res, &argv, &env_assigns),
            Kind::NotFound => self.command_not_found(&argv),
        };
        if self.keep_redirs {
            self.keep_redirs = false;
        } else {
            self.restore_fds(saved);
        }
        self.finish_procsubst();
        if let Some(l) = argv.last() {
            self.last_arg = l.clone();
        }
        r
    }

    pub fn with_temp_assigns(&mut self, a: &[(String, String)], f: impl FnOnce(&mut Shell) -> R) -> R {
        if a.is_empty() {
            return f(self);
        }
        let mut scope = HashMap::new();
        for (k, v) in a {
            let attrs = self.vars.get(k).map(|v| v.attrs & !READONLY).unwrap_or(0);
            scope.insert(k.clone(), Var::new(Value::Str(v.clone()), attrs | EXPORT));
            if k == "PATH" {
                self.hash.clear();
            }
        }
        self.vars.push_frame_with(scope);
        let depth = self.vars.frames.len();
        let r = f(self);
        if self.vars.frames.len() >= depth {
            self.vars.frames.remove(depth - 1);
        }
        if a.iter().any(|(k, _)| k == "PATH") {
            self.hash.clear();
        }
        r
    }

    fn classify(&mut self, name: &str, assigns: &[(String, String)]) -> Kind {
        if !has_path_sep(name) {
            if let Some(f) = self.funcs.get(name) {
                return Kind::Func(f.clone());
            }
            if crate::builtins::is_builtin(name) && !self.disabled_builtins.contains(name) {
                return Kind::Builtin;
            }
        }
        let r = match assigns.iter().find(|(k, _)| k == "PATH") {
            Some((_, p)) => {
                let p = p.clone();
                self.find_in_path(name, &p)
            }
            None => self.find_command(name),
        };
        match r {
            Some(r) => Kind::Ext(r),
            None => Kind::NotFound,
        }
    }

    fn command_not_found(&mut self, argv: &[String]) -> R {
        if let Some(f) = self.funcs.get("command_not_found_handle").cloned() {
            let mut sub = self.subshell_clone();
            let r = sub.run_function(&f, argv);
            return Ok(sub.finish_subshell(r));
        }
        let name = &argv[0];
        if has_path_sep(name) {
            let wp = path::to_win(name, &self.cwd);
            if wp.is_dir() {
                self.eprint(&format!("{}: Is a directory", name));
                return Ok(126);
            }
            self.eprint(&format!("{}: No such file or directory", name));
        } else {
            self.eprint(&format!("{}: command not found", name));
        }
        Ok(127)
    }

    pub fn run_function(&mut self, f: &Arc<FuncDef>, args: &[String]) -> R {
        if self.func_depth >= 4000 {
            self.eprint(&format!("{}: maximum function nesting level exceeded ({})", f.name, self.func_depth));
            return Err(Flow::Fatal(1));
        }
        let saved_pos = std::mem::replace(&mut self.positional, args.to_vec());
        self.vars.push_frame();
        self.call_stack.push(Frame { func: f.name.clone(), source: self.source_name.clone(), call_line: self.lineno });
        // While it runs, the code is from the function's own file (BASH_SOURCE[0], error messages).
        let saved_source = if f.file.is_empty() { None } else { Some(std::mem::replace(&mut self.source_name, f.file.clone())) };
        self.func_depth += 1;
        let saved_loop = self.loop_depth;
        self.loop_depth = 0;
        let had_return_trap = self.traps.get("RETURN").cloned();
        let mut r = self.exec_command(&f.body);
        if let Some(t) = self.traps.get("RETURN").cloned() {
            if had_return_trap.as_deref() != Some(t.as_str()) || self.o[crate::shell::set_opt_index("functrace").unwrap()] {
                let st = match &r {
                    Ok(s) => *s,
                    Err(Flow::Return(s)) => *s,
                    _ => self.status,
                };
                self.status = st;
                if let Err(e) = self.run_trap("RETURN") {
                    r = Err(e);
                }
            }
        }
        self.loop_depth = saved_loop;
        self.func_depth -= 1;
        if let Some(s) = saved_source {
            self.source_name = s;
        }
        self.call_stack.pop();
        self.vars.pop_frame();
        self.positional = saved_pos;
        match r {
            Ok(s) => Ok(s),
            Err(Flow::Return(s)) => {
                self.status = s;
                Ok(s)
            }
            Err(e) => Err(e),
        }
    }

    // ---------- command lookup ----------

    pub fn find_command(&mut self, name: &str) -> Option<Resolved> {
        if has_path_sep(name) {
            return self.resolve_path_cmd(name);
        }
        let p = self.get_str("PATH").unwrap_or_default();
        if self.hash_path != p {
            self.hash.clear();
            self.hash_path = p.clone();
        }
        if let Some(r) = self.hash.get(name) {
            return Some(r.clone());
        }
        let r = self.find_in_path(name, &p);
        if let Some(r) = &r {
            if self.o[O_HASHALL] {
                self.hash.insert(name.to_string(), r.clone());
            }
        }
        r
    }

    pub fn find_in_path(&mut self, name: &str, pathvar: &str) -> Option<Resolved> {
        if has_path_sep(name) {
            return self.resolve_path_cmd(name);
        }
        let dirs = path::split_posix_list(pathvar);
        for d in dirs {
            let d = if d.is_empty() { ".".to_string() } else { d };
            let wd = path::to_win(&d, &self.cwd);
            if let Some(r) = self.probe(&wd, name, true) {
                return Some(prefer_native_coreutils(r));
            }
        }
        None
    }

    fn resolve_path_cmd(&mut self, name: &str) -> Option<Resolved> {
        let wp = path::to_win(name, &self.cwd);
        if let Some(r) = self.classify_file(&wp, false) {
            return Some(r);
        }
        let lower = name.to_lowercase();
        if ![".exe", ".com", ".bat", ".cmd"].iter().any(|e| lower.ends_with(e)) {
            for ext in [".exe", ".com", ".bat", ".cmd"] {
                let mut s = wp.clone().into_os_string();
                s.push(ext);
                let p = PathBuf::from(s);
                if let Some(r) = self.classify_file(&p, false) {
                    return Some(r);
                }
            }
        }
        None
    }

    fn probe(&mut self, dir: &Path, name: &str, strict: bool) -> Option<Resolved> {
        let lower = name.to_lowercase();
        let has_ext = [".exe", ".com", ".bat", ".cmd"].iter().any(|e| lower.ends_with(e));
        if !has_ext {
            let p = dir.join(format!("{}.exe", name));
            if p.is_file() {
                return self.classify_file(&p, strict);
            }
        }
        let p = dir.join(name);
        if let Some(r) = self.classify_file(&p, strict) {
            return Some(r);
        }
        if !has_ext {
            for ext in [".com", ".bat", ".cmd"] {
                let p = dir.join(format!("{}{}", name, ext));
                if p.is_file() {
                    return self.classify_file(&p, strict);
                }
            }
        }
        None
    }

    /// Determine how to execute a file. `strict`: require an executable marker (PATH search).
    pub fn classify_file(&mut self, p: &Path, strict: bool) -> Option<Resolved> {
        let md = std::fs::metadata(p).ok()?;
        if !md.is_file() {
            return None;
        }
        let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "exe" | "com" => {
                if let Some(util) = crate::coreutils::get().and_then(|cu| cu.util_of(p)) {
                    return Some(Resolved { path: p.to_path_buf(), kind: ExeKind::Coreutils, interp: None, util: Some(util), opts: Vec::new() });
                }
                let kind = if path::is_msys_binary(p) || is_msys_dir(p) { ExeKind::Msys } else { ExeKind::Native };
                return Some(Resolved { path: p.to_path_buf(), kind, interp: None, util: None, opts: Vec::new() });
            }
            "bat" | "cmd" => return Some(Resolved { path: p.to_path_buf(), kind: ExeKind::Batch, interp: None, util: None, opts: Vec::new() }),
            _ => {}
        }
        let mut f = std::fs::File::open(p).ok()?;
        let mut buf = [0u8; 512];
        let n = f.read(&mut buf).unwrap_or(0);
        let head = &buf[..n];
        if head.starts_with(b"MZ") {
            let kind = if is_msys_dir(p) { ExeKind::Msys } else { ExeKind::Native };
            return Some(Resolved { path: p.to_path_buf(), kind, interp: None, util: None, opts: Vec::new() });
        }
        if head.starts_with(b"#!") {
            let line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
            let line = String::from_utf8_lossy(&head[2..line_end]).trim().trim_end_matches('\r').to_string();
            let mut parts = line.split_whitespace();
            let interp = parts.next().unwrap_or("").to_string();
            let rest: Vec<String> = parts.map(|s| s.to_string()).collect();
            let base = interp.rsplit(['/', '\\']).next().unwrap_or("").trim_end_matches(".exe").to_string();
            // `opts`: what a shell interpreter gets as options. As on Linux, the rest of the line
            // is a single argument; after `env` it is split into words, as `env -S` does.
            let (prog, arg, opts) = if base == "env" {
                let mut r = rest.clone();
                if r.first().map(|s| s == "-S").unwrap_or(false) {
                    r.remove(0);
                }
                let prog = if r.is_empty() { String::new() } else { r.remove(0) };
                (prog, if r.is_empty() { None } else { Some(r.join(" ")) }, r)
            } else {
                let arg = if rest.is_empty() { None } else { Some(rest.join(" ")) };
                (interp.clone(), arg.clone(), arg.into_iter().collect())
            };
            let pbase = prog.rsplit(['/', '\\']).next().unwrap_or("").trim_end_matches(".exe").to_string();
            if matches!(pbase.as_str(), "sh" | "bash" | "dash" | "ash" | "gash") && arg.as_deref().map(|a| !a.contains("-c")).unwrap_or(true) {
                return Some(Resolved { path: p.to_path_buf(), kind: ExeKind::Script, interp: None, util: None, opts });
            }
            // locate interpreter
            let ip = if prog.contains('/') || prog.contains('\\') {
                let w = path::to_win(&prog, &self.cwd);
                let found = if w.is_file() {
                    Some(w.clone())
                } else {
                    let mut s = w.clone().into_os_string();
                    s.push(".exe");
                    let e = PathBuf::from(s);
                    if e.is_file() {
                        Some(e)
                    } else {
                        None
                    }
                };
                match found {
                    Some(f) => Some(f),
                    None => {
                        let pv = self.get_str("PATH").unwrap_or_default();
                        self.find_in_path(&pbase, &pv).map(|r| r.path)
                    }
                }
            } else {
                let pv = self.get_str("PATH").unwrap_or_default();
                self.find_in_path(&prog, &pv).map(|r| r.path)
            };
            let ip = ip?;
            let ik = if path::is_msys_binary(&ip) || is_msys_dir(&ip) { ExeKind::Msys } else { ExeKind::Native };
            return Some(Resolved { path: p.to_path_buf(), kind: ExeKind::Interp, interp: Some((ip, ik, arg)), util: None, opts: Vec::new() });
        }
        if strict && ext != "sh" {
            return None;
        }
        Some(Resolved { path: p.to_path_buf(), kind: ExeKind::Script, interp: None, util: None, opts: Vec::new() })
    }

    // ---------- external commands ----------

    pub fn run_external(&mut self, res: &Resolved, argv: &[String], assigns: &[(String, String)]) -> R {
        if res.kind == ExeKind::Script && std::env::var_os("GASH_NO_INPROC").is_none() && !self.spawn_only {
            if let Some(r) = self.run_script_inproc(res, argv, assigns) {
                return r;
            }
        }
        // Some command lines run Git's GNU tool after all (see `coreutils::keep_gnu`).
        let gnu;
        let res = match res.util {
            Some(u) if res.kind == ExeKind::Coreutils && crate::coreutils::keep_gnu(u, &argv[1..]) => {
                let p = path::mounts().root.join(r"usr\bin").join(format!("{u}.exe"));
                if p.is_file() {
                    gnu = Resolved { path: p, kind: ExeKind::Msys, interp: None, util: None, opts: Vec::new() };
                    &gnu
                } else {
                    res
                }
            }
            _ => res,
        };
        // The arguments of an MSYS program, when they are too long for a command line.
        let mut rsp: Option<std::fs::File> = None;
        let mut msys_env: Option<String> = None;
        let mut cmd;
        let native;
        let mut msys_child = false;
        // As in Git Bash, these count where the MSYS runtime would see them: in the environment
        // the program starts with, so when exported or given as a prefix (`MSYS_NO_PATHCONV=1 cmd`).
        let env_var = |name: &str| -> Option<String> {
            match assigns.iter().rev().find(|(k, _)| k == name) {
                Some((_, v)) => Some(v.clone()),
                None => match self.vars.get(name) {
                    Some(v) if v.has(EXPORT) => match &v.value {
                        Some(Value::Str(s)) => Some(s.clone()),
                        _ => None,
                    },
                    _ => None,
                },
            }
        };
        let no_conv = env_var("MSYS_NO_PATHCONV").map(|v| !v.is_empty()).unwrap_or(false);
        let excl: Vec<String> = env_var("MSYS2_ARG_CONV_EXCL")
            .map(|v| v.split(';').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
            .unwrap_or_default();
        let conv = |a: &str| -> String {
            if no_conv || excl.iter().any(|e| e == "*" || a.starts_with(e.as_str())) {
                a.to_string()
            } else {
                path::convert_arg(a).into_owned()
            }
        };
        match res.kind {
            ExeKind::Native => {
                cmd = Proc::new(&res.path);
                for a in &argv[1..] {
                    let a = conv(a);
                    // Quote wildcard arguments so programs that glob their own command line
                    // (MSVC setargv, Rust `wild`) don't expand what the shell left literal.
                    if crate::coreutils::has_glob_chars(&a) {
                        cmd.raw_arg(crate::coreutils::msvc_quote(&a));
                    } else {
                        cmd.arg(a);
                    }
                }
                native = true;
            }
            ExeKind::Batch => {
                cmd = Proc::new(&res.path);
                for a in &argv[1..] {
                    cmd.arg(conv(a));
                }
                native = true;
            }
            ExeKind::Coreutils => {
                // Standing in for an MSYS tool: always translate MSYS paths (per-utility policy).
                // Started as `coreutils <util>`, which names itself `util` in messages (its
                // per-utility links say `util.exe`).
                let (util, policy) = res.util.and_then(crate::coreutils::util).unwrap_or(("", crate::coreutils::ArgPolicy::Files));
                cmd = Proc::new(&crate::coreutils::get().map_or(res.path.clone(), |cu| cu.exe.clone()));
                cmd.raw_arg(util);
                for a in crate::coreutils::translate_args(policy, &argv[1..]) {
                    cmd.raw_arg(crate::coreutils::msvc_quote(&a));
                }
                native = true;
            }
            ExeKind::Msys => {
                // Let MSYS programs see the command name as typed in argv[0] (affects their messages).
                // Rust resolves a bare name against the child's PATH, which we set to the same search order.
                let typed = &argv[0];
                let fname = res.path.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
                let same = !has_path_sep(typed) && (fname == typed.to_lowercase() || fname == format!("{}.exe", typed.to_lowercase()));
                let typed = (same && !assigns.iter().any(|(k, _)| k == "PATH")).then_some(typed.as_str());
                (cmd, rsp, msys_env) = msys_cmd(typed, &res.path, &argv[1..], env_var("MSYS"));
                native = false;
                msys_child = true;
            }
            ExeKind::Script => {
                cmd = Proc::new(&crate::global().exe);
                for o in &res.opts {
                    cmd.arg(o);
                }
                cmd.arg(res.path.as_os_str());
                for a in &argv[1..] {
                    cmd.arg(a);
                }
                native = false;
            }
            ExeKind::Interp => {
                let (ip, ik, iarg) = res.interp.clone().unwrap();
                let msys = ik == ExeKind::Msys;
                let mut args: Vec<String> = Vec::new();
                if let Some(a) = iarg {
                    args.push(a);
                }
                args.push(if msys { path::to_posix(&res.path) } else { res.path.to_string_lossy().into_owned() });
                args.extend(argv[1..].iter().cloned());
                if msys {
                    (cmd, rsp, msys_env) = msys_cmd(None, &ip, &args, env_var("MSYS"));
                } else {
                    cmd = Proc::new(&ip);
                    for a in &args {
                        cmd.arg(conv(a));
                    }
                }
                native = !msys;
                msys_child = msys;
            }
        }
        cmd.current_dir(&self.cwd);
        cmd.env_clear();
        for (k, v) in self.child_env(assigns, native) {
            cmd.env(k, v);
        }
        if let Some(v) = msys_env {
            cmd.env("MSYS", v);
        }
        let mut pumps = Vec::new();
        // Here-doc input lent to the child as a file (see `MemReader::lend_file`).
        let mut lent = Vec::new();
        // `>>` handles given to the program; they stay shared while it runs (io::shared_append).
        let mut appends = Vec::new();
        for fd in 0..3 {
            if let Some(Handle::Append(a)) = self.fds.get(fd) {
                appends.push(a.clone());
            }
            if msys_child && names_std_fd(&argv[1..], fd) {
                if let Some(Handle::Mem(m)) = self.fds.get(fd) {
                    if let Ok(Some(f)) = m.lock().unwrap().lend_file() {
                        if let Ok(c) = f.try_clone() {
                            lent.push((m.clone(), f));
                            match fd {
                                0 => cmd.stdin(c),
                                1 => cmd.stdout(c),
                                _ => cmd.stderr(c),
                            };
                            continue;
                        }
                    }
                }
            }
            let st = match self.fds.get(fd) {
                Some(h) => match h.to_stdio(fd) {
                    Ok((s, p)) => {
                        if let Some(p) = p {
                            pumps.push(p);
                        }
                        s
                    }
                    Err(_) => Stdio::null(),
                },
                None => {
                    let mut oo = std::fs::OpenOptions::new();
                    if fd == 0 {
                        oo.write(true);
                    } else {
                        oo.read(true);
                    }
                    oo.open("NUL").map(Stdio::from).unwrap_or_else(|_| Stdio::null())
                }
            };
            match fd {
                0 => cmd.stdin(st),
                1 => cmd.stdout(st),
                _ => cmd.stderr(st),
            };
        }
        let mut flags = 0;
        if self.spawn_only {
            flags |= crate::sys::CREATE_NEW_PROCESS_GROUP;
        }
        if !crate::sys::has_console() {
            // No console to inherit: without this each console child pops its own window.
            // (Ignored by GUI programs, which still open normally.)
            flags |= crate::sys::CREATE_NO_WINDOW;
        }
        if flags != 0 {
            cmd.creation_flags(flags);
        }
        let spawned = cmd.spawn();
        drop(cmd);
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                let code = if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
                self.eprint(&format!("{}: {}", argv[0], io_err_msg(&e)));
                return Ok(code);
            }
        };
        if !appends.is_empty() {
            crate::io::append_started(&appends, &child);
        }
        if self.spawn_only {
            if let Some(f) = rsp.take() {
                // The program reads its response file as it starts; keep the file until it exits.
                if let Ok(h) = std::os::windows::io::AsHandle::as_handle(&child).try_clone_to_owned() {
                    std::thread::spawn(move || {
                        let raw = std::os::windows::io::AsRawHandle::as_raw_handle(&h) as crate::sys::HANDLE;
                        unsafe { crate::sys::WaitForSingleObject(raw, u32::MAX) }; // INFINITE
                        drop(f);
                    });
                }
            }
            self.spawned = Some(child);
            return Ok(0);
        }
        self.child_pid.store(child.id(), Ordering::SeqCst);
        let st = child.wait();
        self.child_pid.store(0, Ordering::SeqCst);
        {
            use std::os::windows::io::AsRawHandle;
            let (u, s) = crate::sys::handle_times(child.as_raw_handle() as _);
            crate::sys::add_child_times(u, s);
        }
        for p in pumps {
            let _ = p.join();
        }
        for (m, f) in lent {
            m.lock().unwrap().return_file(&f);
        }
        let raw = st.as_ref().ok().and_then(|s| s.code()).map(|c| c as u32);
        let died_of_sigint = raw == Some(0xC000_013A) || (!native && raw == Some(2 << 8));
        let code = match st {
            Ok(s) => {
                if native {
                    exit_code(s)
                } else {
                    msys_exit_code(s)
                }
            }
            Err(_) => 127,
        };
        if died_of_sigint {
            // The child died from Ctrl+C (MSYS programs read ^C themselves, so our console
            // handler may not have fired): behave as if we were interrupted too.
            crate::sys::INTERRUPTED.store(true, Ordering::SeqCst);
        } else if crate::sys::interrupted() {
            // the child handled Ctrl+C itself; don't abort the script
            crate::sys::take_interrupt();
        }
        Ok(code)
    }

    /// `None` when the options on the script's `#!` line can't be applied in-process (see
    /// apply_shebang_opts): the caller then starts the script as a separate process instead.
    fn run_script_inproc(&mut self, res: &Resolved, argv: &[String], assigns: &[(String, String)]) -> Option<R> {
        let src = match read_script_file(&res.path) {
            Ok(s) => s,
            Err(e) => {
                self.eprint(&format!("{}: {}", argv[0], io_err_msg(&e)));
                return Some(Ok(126));
            }
        };
        let mut child = self.child_shell(assigns);
        if !res.opts.is_empty() && !apply_shebang_opts(&mut child, &res.opts, &argv[0]) {
            return None;
        }
        child.arg0 = argv[0].clone();
        child.positional = argv[1..].to_vec();
        child.source_name = argv[0].clone();
        let st = child.run_main(&src);
        Some(Ok(st))
    }

    // ---------- redirections ----------

    pub fn restore_fds(&mut self, saved: Saved) {
        for (fd, old) in saved.0.into_iter().rev() {
            self.fds.set(fd, old);
        }
        crate::io::prune_append_files();
    }

    fn set_fd_saved(&mut self, fd: i32, h: Option<Handle>, saved: &mut Saved, persist: bool) {
        let old = self.fds.set(fd, h);
        if !persist && !saved.0.iter().any(|(f, _)| *f == fd) {
            saved.0.push((fd, old));
        }
    }

    pub fn apply_redirs(&mut self, redirs: &[Redir]) -> Result<Saved, RedirErr> {
        let mut saved = Saved::default();
        for r in redirs {
            if let Err(e) = self.apply_redir(r, &mut saved) {
                self.restore_fds(saved);
                return Err(e);
            }
        }
        Ok(saved)
    }

    pub fn open_path(&self, p: &str, read: bool, write: bool, append: bool, truncate: bool) -> std::io::Result<Handle> {
        match p {
            "/dev/null" => return Ok(Handle::Null),
            "/dev/stdin" => return self.fds.get(0).cloned().ok_or_else(|| std::io::Error::other("Bad file descriptor")),
            "/dev/stdout" => return self.fds.get(1).cloned().ok_or_else(|| std::io::Error::other("Bad file descriptor")),
            "/dev/stderr" => return self.fds.get(2).cloned().ok_or_else(|| std::io::Error::other("Bad file descriptor")),
            _ => {}
        }
        if let Some(n) = p.strip_prefix("/dev/fd/") {
            if let Ok(fd) = n.parse::<i32>() {
                return self.fds.get(fd).cloned().ok_or_else(|| std::io::Error::other("Bad file descriptor"));
            }
        }
        let wp = if p == "/dev/tty" {
            PathBuf::from(if read && !write { "CONIN$" } else { "CONOUT$" })
        } else {
            path::to_win(p, &self.cwd)
        };
        let mut oo = std::fs::OpenOptions::new();
        oo.read(read);
        if write {
            // Not `OpenOptions::append`: that gives an append-only handle, which MSYS
            // programs can't write to (see `Handle::Append`).
            oo.write(true).create(true);
            if truncate && !append {
                oo.truncate(true);
            }
        }
        if p == "/dev/tty" {
            oo.create(false);
        }
        let f = oo.open(&wp)?;
        if append && write && crate::sys::is_disk(std::os::windows::io::AsRawHandle::as_raw_handle(&f) as _) {
            (&f).seek(std::io::SeekFrom::End(0))?;
            return Ok(Handle::Append(crate::io::shared_append(f)));
        }
        Ok(Handle::File(Arc::new(f)))
    }

    fn redir_fail(&self, target: &str, e: &std::io::Error) -> RedirErr {
        self.eprint(&format!("{}: {}", target, io_err_msg(e)));
        RedirErr::Failed
    }

    fn apply_redir(&mut self, r: &Redir, saved: &mut Saved) -> Result<(), RedirErr> {
        let default_fd = match r.op {
            RedirOp::In | RedirOp::ReadWrite | RedirOp::DupIn | RedirOp::HereDoc | RedirOp::HereString => 0,
            _ => 1,
        };
        let mut fd = r.fd.unwrap_or(default_fd);
        let persist = r.fd_var.is_some();
        if let Some(var) = &r.fd_var {
            let closing = matches!(r.op, RedirOp::DupIn | RedirOp::DupOut)
                && matches!(&r.target, RedirTarget::Word(w) if w.as_plain().as_deref() == Some("-"));
            if closing {
                fd = self.get_str(var).and_then(|v| v.parse().ok()).unwrap_or(-1);
                if fd >= 0 {
                    self.fds.set(fd, None);
                }
                return Ok(());
            }
            fd = self.fds.free_fd(10);
            let _ = self.set_var(var, &fd.to_string(), false);
        }
        match r.op {
            RedirOp::HereDoc => {
                let body = match &r.target {
                    RedirTarget::HereDoc(cell) => cell.get(),
                    _ => None,
                };
                let content = match body {
                    Some(b) => {
                        if b.quoted {
                            match b.word.parts.first() {
                                Some(WordPart::SQuoted(s)) => s.clone(),
                                _ => String::new(),
                            }
                        } else {
                            self.expand_word_str(&b.word).map_err(RedirErr::Flow)?
                        }
                    }
                    None => String::new(),
                };
                let h = Handle::Mem(Arc::new(std::sync::Mutex::new(crate::io::MemReader::new(content.into_bytes()))));
                self.set_fd_saved(fd, Some(h), saved, persist);
                return Ok(());
            }
            RedirOp::HereString => {
                let w = match &r.target {
                    RedirTarget::Word(w) => w,
                    _ => return Ok(()),
                };
                let mut s = self.expand_word_str(w).map_err(RedirErr::Flow)?;
                s.push('\n');
                let h = Handle::Mem(Arc::new(std::sync::Mutex::new(crate::io::MemReader::new(s.into_bytes()))));
                self.set_fd_saved(fd, Some(h), saved, persist);
                return Ok(());
            }
            _ => {}
        }
        let w = match &r.target {
            RedirTarget::Word(w) => w,
            _ => return Ok(()),
        };
        // < <(cmd) and > >(cmd): stream through a pipe to a worker thread
        if let [WordPart::ProcSubst { input, prog }] = w.parts.as_slice() {
            let reading = matches!(r.op, RedirOp::In);
            let writing = matches!(r.op, RedirOp::Out | RedirOp::Append | RedirOp::Clobber);
            if (reading && *input) || (writing && !*input) {
                let (pr, pw) = match std::io::pipe() {
                    Ok(p) => p,
                    Err(e) => {
                        self.eprint(&format!("pipe error: {}", e));
                        return Err(RedirErr::Failed);
                    }
                };
                let mut sub = self.subshell_clone();
                let mine = if reading {
                    sub.fds.set(1, Some(Handle::PipeW(Arc::new(pw))));
                    Handle::PipeR(Arc::new(pr))
                } else {
                    sub.fds.set(0, Some(Handle::PipeR(Arc::new(pr))));
                    Handle::PipeW(Arc::new(pw))
                };
                let prog = prog.clone();
                let h = std::thread::Builder::new()
                    .stack_size(64 * 1024 * 1024)
                    .spawn(move || {
                        let r = sub.exec_list(&prog.items);
                        sub.finish_subshell(r)
                    })
                    .expect("spawn process substitution");
                if writing {
                    self.ps_threads.push(h);
                }
                self.set_fd_saved(fd, Some(mine), saved, persist);
                return Ok(());
            }
        }
        let target = {
            let mut v = Vec::new();
            if w.parts.iter().any(|p| matches!(p, WordPart::Lit(_))) && !self.o[O_NOGLOB] {
                self.expand_word_into(w, &mut v).map_err(RedirErr::Flow)?;
                if v.len() > 1 {
                    self.eprint(&format!("{}: ambiguous redirect", w.to_source()));
                    return Err(RedirErr::Failed);
                }
                v.pop().unwrap_or_default()
            } else {
                self.expand_word_str(w).map_err(RedirErr::Flow)?
            }
        };
        if target.is_empty() && !matches!(r.op, RedirOp::DupIn | RedirOp::DupOut) {
            self.eprint(&format!("{}: ambiguous redirect", w.to_source()));
            return Err(RedirErr::Failed);
        }
        match r.op {
            RedirOp::In => {
                let h = self.open_path(&target, true, false, false, false).map_err(|e| self.redir_fail(&target, &e))?;
                self.set_fd_saved(fd, Some(h), saved, persist);
            }
            RedirOp::Out | RedirOp::Clobber => {
                if r.op == RedirOp::Out && self.o[O_NOCLOBBER] && !target.starts_with("/dev/") {
                    let wp = path::to_win(&target, &self.cwd);
                    if wp.is_file() {
                        self.eprint(&format!("{}: cannot overwrite existing file", target));
                        return Err(RedirErr::Failed);
                    }
                }
                let h = self.open_path(&target, false, true, false, true).map_err(|e| self.redir_fail(&target, &e))?;
                self.set_fd_saved(fd, Some(h), saved, persist);
            }
            RedirOp::Append => {
                let h = self.open_path(&target, false, true, true, false).map_err(|e| self.redir_fail(&target, &e))?;
                self.set_fd_saved(fd, Some(h), saved, persist);
            }
            RedirOp::ReadWrite => {
                let h = self.open_path(&target, true, true, false, false).map_err(|e| self.redir_fail(&target, &e))?;
                self.set_fd_saved(fd, Some(h), saved, persist);
            }
            RedirOp::OutErr | RedirOp::AppendOutErr => {
                let app = r.op == RedirOp::AppendOutErr;
                if !app && self.o[O_NOCLOBBER] && !target.starts_with("/dev/") && path::to_win(&target, &self.cwd).is_file() {
                    self.eprint(&format!("{}: cannot overwrite existing file", target));
                    return Err(RedirErr::Failed);
                }
                let h = self.open_path(&target, false, true, app, !app).map_err(|e| self.redir_fail(&target, &e))?;
                self.set_fd_saved(1, Some(h.clone()), saved, persist);
                self.set_fd_saved(2, Some(h), saved, persist);
            }
            RedirOp::DupIn | RedirOp::DupOut => {
                if target == "-" {
                    self.set_fd_saved(fd, None, saved, persist);
                    return Ok(());
                }
                let (num, mv) = match target.strip_suffix('-') {
                    Some(n) => (n, true),
                    None => (target.as_str(), false),
                };
                if let Ok(src) = num.parse::<i32>() {
                    match self.fds.get(src).cloned() {
                        Some(h) => {
                            self.set_fd_saved(fd, Some(h), saved, persist);
                            if mv && src != fd {
                                self.set_fd_saved(src, None, saved, persist);
                            }
                        }
                        None => {
                            self.eprint(&format!("{}: Bad file descriptor", src));
                            return Err(RedirErr::Failed);
                        }
                    }
                } else if r.op == RedirOp::DupOut && r.fd.is_none() {
                    let h = self.open_path(&target, false, true, false, true).map_err(|e| self.redir_fail(&target, &e))?;
                    self.set_fd_saved(1, Some(h.clone()), saved, persist);
                    self.set_fd_saved(2, Some(h), saved, persist);
                } else {
                    self.eprint(&format!("{}: ambiguous redirect", target));
                    return Err(RedirErr::Failed);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Does an argument name file descriptor `fd` as a file (`/dev/stdin`, `/dev/fd/0`,
/// `/proc/self/fd/0`, also inside a longer argument such as `--file=/dev/stdin` or an
/// `sh -c` script)?
fn names_std_fd(args: &[String], fd: i32) -> bool {
    let std_name = ["/dev/stdin", "/dev/stdout", "/dev/stderr"][fd as usize];
    let numbered = [format!("/dev/fd/{}", fd), format!("/proc/self/fd/{}", fd)];
    let ends_name = |rest: &str| !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
    args.iter().any(|a| {
        a.match_indices(std_name).any(|(i, m)| ends_name(&a[i + m.len()..]))
            || numbered.iter().any(|n| a.match_indices(n.as_str()).any(|(i, m)| ends_name(&a[i + m.len()..])))
    })
}

/// Applies the options on a script's `#!` line (`#!/bin/bash -e`, `#!/usr/bin/env -S bash -eu`)
/// to the shell that runs it, with bash's command-line syntax. Returns false for anything that
/// would make bash print something or refuse to start (an invalid option, `--version`, ...);
/// the script then runs as a separate gash process, which reports it the way bash does.
fn apply_shebang_opts(sh: &mut Shell, opts: &[String], script: &str) -> bool {
    // The script comes last, as bash gets it, so that `-o` without a name takes it.
    let mut argv = vec![sh.name.clone()];
    argv.extend(opts.iter().cloned());
    argv.push(script.to_string());
    match crate::parse_cli(&argv, &sh.name, false) {
        Ok(cli) => crate::apply_options(sh, &cli).is_ok(),
        Err(_) => false,
    }
}

/// If `r` is one of Git's MSYS tools and Microsoft Coreutils provides the same utility,
/// resolve to the native one instead (see `coreutils.rs`).
fn prefer_native_coreutils(r: Resolved) -> Resolved {
    if r.kind != ExeKind::Msys {
        return r;
    }
    let cu = match crate::coreutils::get() {
        Some(cu) => cu,
        None => return r,
    };
    let stem = match r.path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_ascii_lowercase(),
        None => return r,
    };
    match crate::coreutils::util(&stem) {
        Some((util, _)) if path::is_msys_binary(&r.path) => {
            Resolved { path: cu.program(util), kind: ExeKind::Coreutils, interp: None, util: Some(util), opts: Vec::new() }
        }
        _ => r,
    }
}

static MSYS_DIRS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();

/// bash's `stdin_redirects()`: does this redirection list count as redirecting stdin?
/// Any `<`, `<>`, here-doc or here-string does, whatever its fd; `<&N` and `<&-`
/// only on fd 0, and moves (`<&N-`) and `{var}<` never.
fn stdin_redirects(redirs: &[Redir]) -> bool {
    redirs.iter().any(|r| {
        r.fd_var.is_none()
            && match r.op {
                RedirOp::In | RedirOp::ReadWrite | RedirOp::HereDoc | RedirOp::HereString => true,
                RedirOp::DupIn => {
                    let is_move = matches!(&r.target, RedirTarget::Word(w)
                        if w.as_plain().is_some_and(|t| t.len() > 1 && t.ends_with('-')));
                    r.fd.unwrap_or(0) == 0 && !is_move
                }
                _ => false,
            }
    })
}

/// Is this executable in a directory that holds an MSYS2/Cygwin runtime DLL?
pub fn is_msys_dir(p: &Path) -> bool {
    let dir = match p.parent() {
        Some(d) => d.to_path_buf(),
        None => return false,
    };
    let m = MSYS_DIRS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    if let Some(v) = m.lock().unwrap().get(&dir) {
        return *v;
    }
    let v = dir.join("msys-2.0.dll").exists() || dir.join("cygwin1.dll").exists();
    m.lock().unwrap().insert(dir, v);
    v
}
