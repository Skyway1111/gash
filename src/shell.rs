//! Shell state: variables, options, functions, jobs.

use crate::ast::*;
use crate::io::{Fds, Handle};
use crate::vars::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const STACK_SIZE: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum Flow {
    Break(usize),
    Continue(usize),
    Return(i32),
    Exit(i32),
    /// Fatal expansion/syntax error: aborts a non-interactive shell (status given).
    Fatal(i32),
    /// Error that discards the current top-level command (or `eval`), then execution continues.
    Abort(i32),
}

pub type R = Result<i32, Flow>;

// ---- `set -o` options ----
pub const SET_OPTIONS: &[(&str, char)] = &[
    ("allexport", 'a'),
    ("braceexpand", 'B'),
    ("emacs", '\0'),
    ("errexit", 'e'),
    ("errtrace", 'E'),
    ("functrace", 'T'),
    ("hashall", 'h'),
    ("histexpand", 'H'),
    ("history", '\0'),
    ("ignoreeof", '\0'),
    ("interactive-comments", '\0'),
    ("keyword", 'k'),
    ("monitor", 'm'),
    ("noclobber", 'C'),
    ("noexec", 'n'),
    ("noglob", 'f'),
    ("nolog", '\0'),
    ("notify", 'b'),
    ("nounset", 'u'),
    ("onecmd", 't'),
    ("physical", 'P'),
    ("pipefail", '\0'),
    ("posix", '\0'),
    ("privileged", 'p'),
    ("verbose", 'v'),
    ("vi", '\0'),
    ("xtrace", 'x'),
    ("igncr", '\0'),
];
pub const O_ALLEXPORT: usize = 0;
pub const O_BRACEEXPAND: usize = 1;
pub const O_EMACS: usize = 2;
pub const O_ERREXIT: usize = 3;
pub const O_ERRTRACE: usize = 4;
pub const O_HASHALL: usize = 6;
pub const O_HISTEXPAND: usize = 7;
pub const O_HISTORY: usize = 8;
pub const O_INTERACTIVE_COMMENTS: usize = 10;
pub const O_MONITOR: usize = 12;
pub const O_NOCLOBBER: usize = 13;
pub const O_NOEXEC: usize = 14;
pub const O_NOGLOB: usize = 15;
pub const O_NOUNSET: usize = 18;
pub const O_PHYSICAL: usize = 20;
pub const O_PIPEFAIL: usize = 21;
pub const O_POSIX: usize = 22;
pub const O_VERBOSE: usize = 24;
pub const O_VI: usize = 25;
pub const O_XTRACE: usize = 26;

pub fn set_opt_index(name: &str) -> Option<usize> {
    SET_OPTIONS.iter().position(|(n, _)| *n == name)
}
pub fn set_opt_by_letter(c: char) -> Option<usize> {
    SET_OPTIONS.iter().position(|(_, l)| *l == c && c != '\0')
}

// ---- shopt options ----
pub const SHOPTS: &[(&str, bool)] = &[
    ("array_expand_once", false),
    ("assoc_expand_once", false),
    ("autocd", false),
    ("bash_source_fullpath", false),
    ("cdable_vars", false),
    ("cdspell", false),
    ("checkhash", false),
    ("checkjobs", false),
    ("checkwinsize", true),
    ("cmdhist", true),
    ("compat31", false),
    ("compat32", false),
    ("compat40", false),
    ("compat41", false),
    ("compat42", false),
    ("compat43", false),
    ("compat44", false),
    ("complete_fullquote", true),
    ("direxpand", false),
    ("dirspell", false),
    ("dotglob", false),
    ("execfail", false),
    ("expand_aliases", false),
    ("extdebug", false),
    ("extglob", false),
    ("extquote", true),
    ("failglob", false),
    ("force_fignore", true),
    ("globasciiranges", true),
    ("globskipdots", true),
    ("globstar", false),
    ("gnu_errfmt", false),
    ("histappend", false),
    ("histreedit", false),
    ("histverify", false),
    ("hostcomplete", true),
    ("huponexit", false),
    ("inherit_errexit", false),
    ("interactive_comments", true),
    ("lastpipe", false),
    ("lithist", false),
    ("localvar_inherit", false),
    ("localvar_unset", false),
    ("login_shell", false),
    ("mailwarn", false),
    ("no_empty_cmd_completion", false),
    ("nocaseglob", false),
    ("nocasematch", false),
    ("noexpand_translation", false),
    ("nullglob", false),
    ("patsub_replacement", true),
    ("progcomp", true),
    ("progcomp_alias", false),
    ("promptvars", true),
    ("restricted_shell", false),
    ("shift_verbose", false),
    ("sourcepath", true),
    ("varredir_close", false),
    ("xpg_echo", false),
];

pub fn shopt_index(name: &str) -> Option<usize> {
    SHOPTS.iter().position(|(n, _)| *n == name)
}

macro_rules! sh_idx {
    ($name:ident, $s:expr) => {
        pub fn $name() -> usize {
            static I: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            *I.get_or_init(|| shopt_index($s).unwrap())
        }
    };
}
pub mod sho {
    use super::shopt_index;
    sh_idx!(dotglob, "dotglob");
    sh_idx!(expand_aliases, "expand_aliases");
    sh_idx!(extglob, "extglob");
    sh_idx!(failglob, "failglob");
    sh_idx!(globstar, "globstar");
    sh_idx!(inherit_errexit, "inherit_errexit");
    sh_idx!(lastpipe, "lastpipe");
    sh_idx!(login_shell, "login_shell");
    sh_idx!(nocaseglob, "nocaseglob");
    sh_idx!(nocasematch, "nocasematch");
    sh_idx!(nullglob, "nullglob");
    sh_idx!(patsub_replacement, "patsub_replacement");
    sh_idx!(xpg_echo, "xpg_echo");
    sh_idx!(promptvars, "promptvars");
    sh_idx!(sourcepath, "sourcepath");
    sh_idx!(autocd, "autocd");
    sh_idx!(shift_verbose, "shift_verbose");
    sh_idx!(cdable_vars, "cdable_vars");
    sh_idx!(execfail, "execfail");
    sh_idx!(histappend, "histappend");
    sh_idx!(checkwinsize, "checkwinsize");
}

pub enum JobKind {
    Proc(std::process::Child),
    Thread(Option<std::thread::JoinHandle<i32>>),
}

pub struct Job {
    pub id: usize,
    pub pid: u32,
    pub text: String,
    pub kind: JobKind,
    pub status: Option<i32>,
    pub cancel: Arc<AtomicBool>,
    pub child_pid: Arc<AtomicU32>,
    pub notified: bool,
}

impl Job {
    pub fn poll(&mut self) -> Option<i32> {
        if self.status.is_some() {
            return self.status;
        }
        match &mut self.kind {
            JobKind::Proc(c) => {
                if let Ok(Some(st)) = c.try_wait() {
                    self.status = Some(crate::exec::exit_code(st));
                }
            }
            JobKind::Thread(h) => {
                if h.as_ref().map(|h| h.is_finished()).unwrap_or(true) {
                    let st = h.take().map(|h| h.join().unwrap_or(1)).unwrap_or(0);
                    self.status = Some(st);
                }
            }
        }
        self.status
    }
    pub fn wait(&mut self) -> i32 {
        if let Some(s) = self.status {
            return s;
        }
        let st = match &mut self.kind {
            JobKind::Proc(c) => c.wait().map(crate::exec::exit_code).unwrap_or(127),
            JobKind::Thread(h) => h.take().map(|h| h.join().unwrap_or(1)).unwrap_or(0),
        };
        self.status = Some(st);
        st
    }
    /// Stop the job with signal `sig`. Windows has no signals: its process is terminated with exit
    /// code 128+sig, which `wait` then reports, as bash does for a job killed by a signal.
    pub fn kill(&mut self, sig: i32) {
        let code = 128 + sig as u32;
        match &mut self.kind {
            JobKind::Proc(c) => {
                if !crate::sys::kill_pid(c.id(), code) {
                    let _ = c.kill();
                }
            }
            JobKind::Thread(_) => {
                self.cancel.store(true, Ordering::SeqCst);
                let p = self.child_pid.load(Ordering::SeqCst);
                if p != 0 {
                    crate::sys::kill_pid(p, code);
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    pub last_id: usize,
}

#[derive(Clone)]
pub struct Frame {
    pub func: String,
    pub source: String,
    pub call_line: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExeKind {
    /// Native Windows program (needs MSYS-style argument conversion).
    Native,
    /// MSYS2 program (understands POSIX paths).
    Msys,
    Batch,
    /// Shell script to run in-process.
    Script,
    /// Script with a foreign interpreter (resolved interpreter path + optional arg).
    Interp,
    /// A utility of Microsoft Coreutils for Windows standing in for an MSYS tool.
    Coreutils,
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub path: PathBuf,
    pub kind: ExeKind,
    pub interp: Option<(PathBuf, ExeKind, Option<String>)>,
    /// For ExeKind::Coreutils: the utility name.
    pub util: Option<&'static str>,
    /// For ExeKind::Script: the options on its `#!` line, as bash would be given them.
    pub opts: Vec<String>,
}

static FAKE_PID: AtomicU32 = AtomicU32::new(0);

pub fn fake_pid() -> u32 {
    let base = std::process::id().wrapping_mul(7) % 30000 + 30000;
    base + FAKE_PID.fetch_add(1, Ordering::SeqCst) % 30000
}

pub struct Shell {
    pub vars: Vars,
    pub funcs: Arc<HashMap<String, Arc<FuncDef>>>,
    pub exported_funcs: Arc<HashSet<String>>,
    pub aliases: Arc<HashMap<String, String>>,
    pub positional: Vec<String>,
    pub arg0: String,
    pub fds: Fds,
    pub cwd: PathBuf,
    pub status: i32,
    pub last_bg: Option<u32>,
    pub o: Vec<bool>,
    pub sh: Vec<bool>,
    pub traps: Arc<HashMap<String, String>>,
    pub jobs: Jobs,
    pub hash: HashMap<String, Resolved>,
    pub hash_path: String,
    pub dirstack: Vec<String>,
    pub func_depth: usize,
    pub loop_depth: usize,
    pub subshell: usize,
    pub call_stack: Vec<Frame>,
    pub getopts_pos: usize,
    pub env_extra: Arc<Vec<(String, String)>>,
    pub cancel: Arc<AtomicBool>,
    pub child_pid: Arc<AtomicU32>,
    pub cond_depth: usize,
    pub start: Instant,
    pub seconds_base: i64,
    pub rng: u64,
    pub lineno: usize,
    pub source_name: String,
    pub interactive: bool,
    pub pid: u32,
    pub bashpid: u32,
    pub last_arg: String,
    pub in_trap: bool,
    pub exit_trap_done: bool,
    pub spawn_only: bool,
    pub spawned: Option<std::process::Child>,
    pub procsubst: Vec<(PathBuf, Option<Arc<Program>>)>,
    pub cmdsubst_status: Option<i32>,
    pub name: String,
    pub history: Arc<Mutex<Vec<String>>>,
    pub sourcing: usize,
    pub disabled_builtins: Arc<HashSet<String>>,
    pub umask: u32,
    pub is_subshell: bool,
    pub script_mode: bool,
    pub decl_arrays: HashMap<usize, Vec<(Option<String>, String)>>,
    pub keep_redirs: bool,
    pub completions: Arc<HashMap<String, String>>,
    /// Parent's traps, shown by 	rap -p in a subshell until it sets its own.
    pub trap_display: Option<Arc<HashMap<String, String>>>,
    pub parse_line_base: Option<usize>,
    /// Consumers of > >(cmd) redirections; joined at exit so their output isn't lost.
    pub ps_threads: Vec<std::thread::JoinHandle<i32>>,
    /// bash's `stdin_redir`: set once a compound command with an input
    /// redirection (or a pipe into a compound command, function or builtin)
    /// runs, so `cmd &` keeps that stdin instead of getting /dev/null.
    /// Cleared before each command of a script or stdin reader, not for `-c`.
    pub stdin_redir: bool,
    /// One-shot, for the next `exec_command`: it is a command bash would fork on
    /// its own (a pipeline element, or a lone `cmd &`); `Some(true)` if it reads a pipe.
    pub forked: Option<bool>,
}

impl Shell {
    pub fn new(name: &str) -> Shell {
        let start = Instant::now();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ (std::process::id() as u64) << 17;
        let mut o = vec![false; SET_OPTIONS.len()];
        o[O_BRACEEXPAND] = true;
        o[O_HASHALL] = true;
        o[O_INTERACTIVE_COMMENTS] = true;
        let sh = SHOPTS.iter().map(|(_, d)| *d).collect();
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:\\"));
        Shell {
            vars: Vars::default(),
            funcs: Arc::new(HashMap::new()),
            exported_funcs: Arc::new(HashSet::new()),
            aliases: Arc::new(HashMap::new()),
            positional: Vec::new(),
            arg0: name.to_string(),
            fds: Fds::std(),
            cwd,
            status: 0,
            last_bg: None,
            o,
            sh,
            traps: Arc::new(HashMap::new()),
            jobs: Jobs::default(),
            hash: HashMap::new(),
            hash_path: String::new(),
            dirstack: Vec::new(),
            func_depth: 0,
            loop_depth: 0,
            subshell: 0,
            call_stack: Vec::new(),
            getopts_pos: 0,
            env_extra: Arc::new(Vec::new()),
            cancel: Arc::new(AtomicBool::new(false)),
            child_pid: Arc::new(AtomicU32::new(0)),
            cond_depth: 0,
            start,
            seconds_base: 0,
            rng: seed | 1,
            lineno: 0,
            source_name: name.to_string(),
            interactive: false,
            pid: std::process::id(),
            bashpid: std::process::id(),
            last_arg: String::new(),
            in_trap: false,
            exit_trap_done: false,
            spawn_only: false,
            spawned: None,
            procsubst: Vec::new(),
            cmdsubst_status: None,
            name: name.to_string(),
            history: Arc::new(Mutex::new(Vec::new())),
            sourcing: 0,
            disabled_builtins: Arc::new(HashSet::new()),
            umask: 0o022,
            is_subshell: false,
            script_mode: false,
            decl_arrays: HashMap::new(),
            keep_redirs: false,
            completions: Arc::new(HashMap::new()),
            trap_display: None,
            parse_line_base: None,
            ps_threads: Vec::new(),
            stdin_redir: false,
            forked: None,
        }
    }

    /// Clone for a subshell (`( )`, `$( )`, pipeline stage, background job).
    pub fn subshell_clone(&self) -> Shell {
        let traps: HashMap<String, String> =
            self.traps.iter().filter(|(_, v)| v.is_empty()).map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut o = self.o.clone();
        if !self.sh[sho::inherit_errexit()] && self.cond_depth == 0 {
            // bash: command substitution doesn't inherit -e unless inherit_errexit; subshells do.
        }
        o[O_MONITOR] = false;
        Shell {
            vars: self.vars.clone(),
            funcs: self.funcs.clone(),
            exported_funcs: self.exported_funcs.clone(),
            aliases: self.aliases.clone(),
            positional: self.positional.clone(),
            arg0: self.arg0.clone(),
            fds: self.fds.clone(),
            cwd: self.cwd.clone(),
            status: self.status,
            last_bg: self.last_bg,
            o,
            sh: self.sh.clone(),
            traps: Arc::new(traps),
            jobs: Jobs::default(),
            hash: self.hash.clone(),
            hash_path: self.hash_path.clone(),
            dirstack: self.dirstack.clone(),
            func_depth: self.func_depth,
            loop_depth: self.loop_depth,
            subshell: self.subshell + 1,
            call_stack: self.call_stack.clone(),
            getopts_pos: self.getopts_pos,
            env_extra: self.env_extra.clone(),
            cancel: self.cancel.clone(),
            child_pid: self.child_pid.clone(),
            cond_depth: self.cond_depth,
            start: self.start,
            seconds_base: self.seconds_base,
            rng: self.rng.wrapping_mul(6364136223846793005).wrapping_add(self.subshell as u64 + 1442695040888963407),
            lineno: self.lineno,
            source_name: self.source_name.clone(),
            interactive: false,
            pid: self.pid,
            bashpid: fake_pid(),
            last_arg: self.last_arg.clone(),
            in_trap: self.in_trap,
            exit_trap_done: false,
            spawn_only: false,
            spawned: None,
            procsubst: Vec::new(),
            cmdsubst_status: None,
            name: self.name.clone(),
            history: self.history.clone(),
            sourcing: self.sourcing,
            disabled_builtins: self.disabled_builtins.clone(),
            umask: self.umask,
            is_subshell: true,
            script_mode: self.script_mode,
            decl_arrays: HashMap::new(),
            keep_redirs: false,
            completions: self.completions.clone(),
            trap_display: Some(self.trap_display.clone().unwrap_or_else(|| self.traps.clone())),
            parse_line_base: None,
            ps_threads: Vec::new(),
            stdin_redir: self.stdin_redir,
            forked: None,
        }
    }

    /// A fresh shell for running a script in-process (like exec'ing a new bash).
    pub fn child_shell(&self, extra_env: &[(String, String)]) -> Shell {
        let mut s = Shell::new(&self.name);
        let mut g: Scope = HashMap::new();
        for (name, v) in self.vars.visible() {
            if v.has(EXPORT) {
                if let Some(Value::Str(val)) = &v.value {
                    g.insert(name, Var::new(Value::Str(val.clone()), EXPORT));
                }
            }
        }
        for (k, v) in extra_env {
            g.insert(k.clone(), Var::new(Value::Str(v.clone()), EXPORT));
        }
        s.vars.global = Arc::new(g);
        let mut funcs = HashMap::new();
        for f in self.exported_funcs.iter() {
            if let Some(d) = self.funcs.get(f) {
                funcs.insert(f.clone(), d.clone());
            }
        }
        s.funcs = Arc::new(funcs);
        s.exported_funcs = self.exported_funcs.clone();
        s.fds = self.fds.clone();
        s.cwd = self.cwd.clone();
        s.env_extra = self.env_extra.clone();
        s.cancel = self.cancel.clone();
        s.child_pid = self.child_pid.clone();
        s.history = Arc::new(Mutex::new(Vec::new()));
        s.bashpid = fake_pid();
        s.pid = s.bashpid;
        s.hash = self.hash.clone();
        s.hash_path = self.hash_path.clone();
        s.script_mode = true;
        let lvl = s.get_str("SHLVL").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let _ = s.set_str("SHLVL", &(lvl + 1).to_string());
        s.init_shell_vars();
        s
    }

    pub fn init_shell_vars(&mut self) {
        let set = |s: &mut Shell, k: &str, v: &str| {
            let var = s.vars.global_mut(k);
            if var.value.is_none() {
                var.value = Some(Value::Str(v.to_string()));
            }
        };
        set(self, "IFS", " \t\n");
        set(self, "PS2", "> ");
        set(self, "PS4", "+ ");
        set(self, "OPTIND", "1");
        set(self, "OPTERR", "1");
        set(self, "BASH_VERSION", crate::BASH_VERSION);
        set(self, "HOSTTYPE", "x86_64");
        set(self, "OSTYPE", "msys");
        set(self, "MACHTYPE", "x86_64-pc-msys");
        let host = crate::sys::host_name();
        set(self, "HOSTNAME", &host);
        set(self, "UID", "197609");
        set(self, "EUID", "197609");
        set(self, "PPID", "1");
        set(self, "HISTFILE", "");
        let ver: BTreeMap<i64, String> = crate::BASH_VERSINFO.iter().enumerate().map(|(i, s)| (i as i64, s.to_string())).collect();
        let v = self.vars.global_mut("BASH_VERSINFO");
        if v.value.is_none() {
            v.value = Some(Value::Arr(ver));
            v.attrs |= READONLY | ARRAY;
        }
        if self.get_str("PWD").map(|p| p.is_empty()).unwrap_or(true) || self.vars.get("PWD").is_some() {
            let pwd = crate::path::to_posix(&self.cwd);
            let v = self.vars.global_mut("PWD");
            let attrs = v.attrs;
            *v = Var::new(Value::Str(pwd), attrs | EXPORT);
        }
        let bash = crate::global().exe_posix.clone();
        let v = self.vars.global_mut("BASH");
        v.value = Some(Value::Str(bash));
    }

    // ---------- options ----------

    pub fn opt(&self, i: usize) -> bool {
        self.o[i]
    }
    pub fn shopt(&self, i: usize) -> bool {
        self.sh[i]
    }

    pub fn flags_string(&self) -> String {
        let mut s = String::new();
        for (i, (_, c)) in SET_OPTIONS.iter().enumerate() {
            if *c != '\0' && self.o[i] {
                s.push(*c);
            }
        }
        if self.interactive {
            s.push('i');
        }
        s.push('s');
        let mut v: Vec<char> = s.chars().collect();
        v.sort_by_key(|c| (c.is_uppercase(), *c));
        // bash order roughly: himBH etc; keep alphabetical with lowercase first
        v.into_iter().collect()
    }

    // ---------- errors ----------

    pub fn err_prefix(&self) -> String {
        if self.interactive && self.sourcing == 0 && self.func_depth == 0 {
            format!("{}: ", self.name)
        } else {
            format!("{}: line {}: ", self.source_name, self.lineno)
        }
    }

    pub fn eprint(&self, msg: &str) {
        let line = format!("{}{}\n", self.err_prefix(), msg);
        self.write_fd(2, line.as_bytes());
    }

    pub fn write_fd(&self, fd: i32, data: &[u8]) -> bool {
        match self.fds.get(fd) {
            Some(h) => h.write_all(data).is_ok(),
            None => false,
        }
    }

    /// Write to fd, converting a broken pipe into a SIGPIPE-like exit.
    pub fn out(&self, fd: i32, data: &[u8]) -> Result<(), Flow> {
        match self.fds.get(fd) {
            Some(h) => match h.write_all(data) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe || e.raw_os_error() == Some(232) => {
                    Err(Flow::Exit(141))
                }
                Err(e) => {
                    let m = format!("{}write error: {}\n", self.err_prefix(), crate::exec::io_err_msg(&e));
                    self.write_fd(2, m.as_bytes());
                    Ok(())
                }
            },
            None => {
                if fd != 2 {
                    let m = format!("{}write error: Bad file descriptor\n", self.err_prefix());
                    self.write_fd(2, m.as_bytes());
                }
                Ok(())
            }
        }
    }

    // ---------- variables ----------

    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Dynamic/special variable values.
    fn special_var(&mut self, name: &str) -> Option<Value> {
        let v = match name {
            "RANDOM" => (self.next_random() % 32768).to_string(),
            "SRANDOM" => (self.next_random() & 0xffff_ffff).to_string(),
            "SECONDS" => (self.start.elapsed().as_secs() as i64 + self.seconds_base).to_string(),
            "EPOCHSECONDS" => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .to_string(),
            "EPOCHREALTIME" => {
                let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                format!("{}.{:06}", d.as_secs(), d.subsec_micros())
            }
            "LINENO" => self.lineno.to_string(),
            "BASHPID" => self.bashpid.to_string(),
            "BASH_SUBSHELL" => self.subshell.to_string(),
            // The call stack, innermost first, as bash keeps it: function and `source` frames, then
            // the script itself ("main", its file, line 0). That last entry exists only when the
            // shell runs a script, not for -c or stdin.
            "FUNCNAME" => {
                // Set only while a function runs; `source` frames show up in it as "source".
                if !self.call_stack.iter().any(|f| f.func != "source") {
                    return None;
                }
                let mut m = BTreeMap::new();
                for (i, f) in self.call_stack.iter().rev().enumerate() {
                    m.insert(i as i64, f.func.clone());
                }
                if self.script_mode {
                    m.insert(self.call_stack.len() as i64, "main".into());
                }
                return Some(Value::Arr(m));
            }
            "BASH_SOURCE" => {
                // [0] is the file of the code running now (for a function, the file it was defined
                // in), then the file each frame was entered from.
                let mut v = vec![self.source_name.clone()];
                v.extend(self.call_stack.iter().rev().map(|f| f.source.clone()));
                if !self.script_mode {
                    v.pop();
                }
                return Some(Value::Arr(v.into_iter().enumerate().map(|(i, s)| (i as i64, s)).collect()));
            }
            "BASH_LINENO" => {
                let mut m = BTreeMap::new();
                for (i, f) in self.call_stack.iter().rev().enumerate() {
                    m.insert(i as i64, f.call_line.to_string());
                }
                if self.script_mode {
                    m.insert(self.call_stack.len() as i64, "0".into());
                }
                return Some(Value::Arr(m));
            }
            "DIRSTACK" => {
                let mut m = BTreeMap::new();
                m.insert(0, self.get_str("PWD").unwrap_or_default());
                for (i, d) in self.dirstack.iter().rev().enumerate() {
                    m.insert(i as i64 + 1, d.clone());
                }
                return Some(Value::Arr(m));
            }
            "_" => self.last_arg.clone(),
            "SHELLOPTS" => {
                let mut v = Vec::new();
                for (i, (n, _)) in SET_OPTIONS.iter().enumerate() {
                    if self.o[i] {
                        v.push(*n);
                    }
                }
                v.join(":")
            }
            "BASHOPTS" => {
                let mut v = Vec::new();
                for (i, (n, _)) in SHOPTS.iter().enumerate() {
                    if self.sh[i] {
                        v.push(*n);
                    }
                }
                v.join(":")
            }
            _ => return None,
        };
        Some(Value::Str(v))
    }

    pub fn is_special_dynamic(name: &str) -> bool {
        matches!(
            name,
            "RANDOM" | "SRANDOM" | "SECONDS" | "EPOCHSECONDS" | "EPOCHREALTIME" | "LINENO" | "BASHPID" | "BASH_SUBSHELL"
                | "FUNCNAME" | "BASH_SOURCE" | "BASH_LINENO" | "DIRSTACK" | "_" | "SHELLOPTS" | "BASHOPTS"
        )
    }

    /// Resolve namerefs.
    pub fn resolve_name(&self, name: &str) -> String {
        let mut n = name.to_string();
        for _ in 0..10 {
            match self.vars.get(&n) {
                Some(v) if v.has(NAMEREF) => match &v.value {
                    Some(Value::Str(t)) if !t.is_empty() => n = t.clone(),
                    _ => break,
                },
                _ => break,
            }
        }
        n
    }

    pub fn get_value(&mut self, name: &str) -> Option<Value> {
        if Self::is_special_dynamic(name) {
            if let Some(v) = self.vars.get(name) {
                // user-assigned SECONDS etc. still dynamic, except `_`
                if name == "_" {
                    return v.value.clone();
                }
            }
            return self.special_var(name);
        }
        let n = self.resolve_name(name);
        self.vars.get(&n).and_then(|v| v.value.clone())
    }

    pub fn get_str(&mut self, name: &str) -> Option<String> {
        self.get_value(name).map(|v| v.scalar())
    }

    pub fn get_str_or_empty(&mut self, name: &str) -> String {
        self.get_str(name).unwrap_or_default()
    }

    pub fn ifs(&mut self) -> String {
        match self.vars.get("IFS").and_then(|v| v.value.as_ref()) {
            Some(v) => v.scalar(),
            None => " \t\n".into(),
        }
    }

    pub fn var_attrs(&self, name: &str) -> u32 {
        let n = self.resolve_name(name);
        self.vars.get(&n).map(|v| v.attrs).unwrap_or(0)
    }

    fn transform_value(&mut self, attrs: u32, v: String) -> Result<String, String> {
        let mut v = v;
        if attrs & INTEGER != 0 {
            v = crate::arith::eval(&v, self)?.to_string();
        }
        if attrs & LOWER != 0 {
            v = v.to_lowercase();
        } else if attrs & UPPER != 0 {
            v = v.to_uppercase();
        } else if attrs & CAPITALIZE != 0 {
            let mut c = v.chars();
            v = match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            };
        }
        Ok(v)
    }

    fn on_assign(&mut self, name: &str, value: &str) {
        match name {
            "PATH" => {
                self.hash.clear();
            }
            "SECONDS" => {
                self.seconds_base = value.parse::<i64>().unwrap_or(0) - self.start.elapsed().as_secs() as i64;
            }
            "RANDOM" => {
                self.rng = value.parse::<u64>().unwrap_or(1).wrapping_mul(2654435761) | 1;
            }
            "OPTIND" => {
                self.getopts_pos = 0;
            }
            _ => {}
        }
    }

    /// Assign a scalar (or element 0 of an array).
    pub fn set_var(&mut self, name: &str, value: &str, append: bool) -> Result<(), String> {
        let name = self.resolve_name(name);
        let (attrs, cur) = match self.vars.get(&name) {
            Some(v) => (v.attrs, v.value.clone()),
            None => (0, None),
        };
        if attrs & READONLY != 0 {
            return Err(format!("{}: readonly variable", name));
        }
        let allexport = self.o[O_ALLEXPORT];
        match cur {
            Some(Value::Arr(_)) | Some(Value::Assoc(_)) => {
                return self.set_elem(&name, "0", value, append);
            }
            _ => {}
        }
        let newval = if append {
            if attrs & INTEGER != 0 {
                let a = cur.map(|v| v.scalar()).unwrap_or_default();
                let a = crate::arith::eval(if a.is_empty() { "0" } else { &a }, self)?;
                let b = crate::arith::eval(value, self)?;
                (a.wrapping_add(b)).to_string()
            } else {
                let mut s = cur.map(|v| v.scalar()).unwrap_or_default();
                s.push_str(&self.transform_value(attrs & !INTEGER, value.to_string())?);
                s
            }
        } else {
            self.transform_value(attrs, value.to_string())?
        };
        self.on_assign(&name, &newval);
        let v = self.vars.get_mut_or_create(&name);
        v.value = Some(Value::Str(newval));
        if allexport {
            v.attrs |= EXPORT;
        }
        Ok(())
    }

    pub fn set_str(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.set_var(name, value, false)
    }

    /// Set an array element (index evaluated arithmetically for indexed arrays).
    pub fn set_elem(&mut self, name: &str, key: &str, value: &str, append: bool) -> Result<(), String> {
        let name = self.resolve_name(name);
        let (attrs, is_assoc) = match self.vars.get(&name) {
            Some(v) => (v.attrs, matches!(v.value, Some(Value::Assoc(_))) || v.attrs & ASSOC != 0),
            None => (0, false),
        };
        if attrs & READONLY != 0 {
            return Err(format!("{}: readonly variable", name));
        }
        if is_assoc {
            let cur = self.vars.get(&name).and_then(|v| match &v.value {
                Some(Value::Assoc(m)) => m.get(key).cloned(),
                _ => None,
            });
            let nv = if append {
                cur.unwrap_or_default() + &self.transform_value(attrs & !INTEGER, value.to_string())?
            } else {
                self.transform_value(attrs, value.to_string())?
            };
            let v = self.vars.get_mut_or_create(&name);
            if !matches!(v.value, Some(Value::Assoc(_))) {
                v.value = Some(Value::Assoc(AssocMap::default()));
            }
            if let Some(Value::Assoc(m)) = &mut v.value {
                m.insert(key.to_string(), nv);
            }
            return Ok(());
        }
        let mut idx = crate::arith::eval(key, self)?;
        let cur_len_max = match self.vars.get(&name).and_then(|v| v.value.as_ref()) {
            Some(Value::Arr(a)) => a.keys().next_back().map(|k| k + 1).unwrap_or(0),
            Some(Value::Str(_)) => 1,
            _ => 0,
        };
        if idx < 0 {
            idx += cur_len_max;
            if idx < 0 {
                return Err(format!("{}[{}]: bad array subscript", name, key));
            }
        }
        let cur = match self.vars.get(&name).and_then(|v| v.value.as_ref()) {
            Some(Value::Arr(a)) => a.get(&idx).cloned(),
            Some(Value::Str(s)) if idx == 0 => Some(s.clone()),
            _ => None,
        };
        let nv = if append {
            if attrs & INTEGER != 0 {
                let a = crate::arith::eval(&cur.unwrap_or_else(|| "0".into()), self)?;
                let b = crate::arith::eval(value, self)?;
                a.wrapping_add(b).to_string()
            } else {
                cur.unwrap_or_default() + &self.transform_value(attrs & !INTEGER, value.to_string())?
            }
        } else {
            self.transform_value(attrs, value.to_string())?
        };
        let allexport = self.o[O_ALLEXPORT];
        let v = self.vars.get_mut_or_create(&name);
        match &mut v.value {
            Some(Value::Arr(a)) => {
                a.insert(idx, nv);
            }
            Some(Value::Str(s)) => {
                let mut a = BTreeMap::new();
                a.insert(0, std::mem::take(s));
                a.insert(idx, nv);
                v.value = Some(Value::Arr(a));
            }
            _ => {
                let mut a = BTreeMap::new();
                a.insert(idx, nv);
                v.value = Some(Value::Arr(a));
            }
        }
        v.attrs |= ARRAY;
        if allexport {
            v.attrs |= EXPORT;
        }
        Ok(())
    }

    /// Assign a whole array: `name=(...)`.
    pub fn set_array(&mut self, name: &str, elems: Vec<(Option<String>, String)>, append: bool) -> Result<(), String> {
        let name = self.resolve_name(name);
        let (attrs, is_assoc, cur) = match self.vars.get(&name) {
            Some(v) => (v.attrs, v.attrs & ASSOC != 0 || matches!(v.value, Some(Value::Assoc(_))), v.value.clone()),
            None => (0, false, None),
        };
        if attrs & READONLY != 0 {
            return Err(format!("{}: readonly variable", name));
        }
        if is_assoc {
            let mut m = match (&cur, append) {
                (Some(Value::Assoc(m)), true) => m.clone(),
                _ => AssocMap::default(),
            };
            let mut pending_key: Option<String> = None;
            for (k, v) in elems {
                match k {
                    Some(k) => {
                        let v = self.transform_value(attrs, v)?;
                        m.insert(k, v);
                    }
                    None => {
                        // bash 5.1+: `(k1 v1 k2 v2)` key/value pairs
                        match pending_key.take() {
                            None => pending_key = Some(v),
                            Some(k) => {
                                let v = self.transform_value(attrs, v)?;
                                m.insert(k, v);
                            }
                        }
                    }
                }
            }
            if let Some(k) = pending_key {
                m.insert(k, String::new());
            }
            let var = self.vars.get_mut_or_create(&name);
            var.value = Some(Value::Assoc(m));
            var.attrs |= ASSOC;
            return Ok(());
        }
        let mut a: BTreeMap<i64, String> = match (&cur, append) {
            (Some(Value::Arr(a)), true) => a.clone(),
            (Some(Value::Str(s)), true) => {
                let mut a = BTreeMap::new();
                a.insert(0, s.clone());
                a
            }
            _ => BTreeMap::new(),
        };
        let mut next = a.keys().next_back().map(|k| k + 1).unwrap_or(0);
        for (k, v) in elems {
            let idx = match k {
                Some(k) => {
                    let mut i = crate::arith::eval(&k, self)?;
                    if i < 0 {
                        i += next;
                    }
                    i
                }
                None => next,
            };
            let v = self.transform_value(attrs, v)?;
            a.insert(idx, v);
            next = idx + 1;
        }
        let allexport = self.o[O_ALLEXPORT];
        let var = self.vars.get_mut_or_create(&name);
        var.value = Some(Value::Arr(a));
        var.attrs |= ARRAY;
        if allexport {
            var.attrs |= EXPORT;
        }
        Ok(())
    }

    pub fn unset_var(&mut self, name: &str) -> Result<(), String> {
        let n = self.resolve_name(name);
        if let Some(v) = self.vars.get(&n) {
            if v.has(READONLY) {
                return Err(format!("{}: cannot unset: readonly variable", n));
            }
        }
        if n == "PATH" {
            self.hash.clear();
        }
        if self.func_depth > 0 {
            if let Some(top) = self.vars.frames.last_mut() {
                if top.contains_key(&n) {
                    if let Some(v) = Arc::make_mut(top).get_mut(&n) {
                        v.value = None;
                        v.attrs = 0;
                    }
                    return Ok(());
                }
            }
        }
        self.vars.unset(&n);
        Ok(())
    }

    pub fn export(&mut self, name: &str) {
        let v = self.vars.get_mut_or_create(name);
        v.attrs |= EXPORT;
    }

    /// Environment for a child process (values converted for native Windows programs).
    ///
    /// MSYS programs and child gash processes get POSIX values, except PATH, TMP and TEMP, which
    /// are converted to Windows form because the MSYS runtime reads them from the Windows
    /// environment. It builds its `/tmp` mount (`usertemp` in /etc/fstab) from GetTempPathW,
    /// which reads TMP/TEMP, and `TMP=/tmp` makes that `C:\tmp`. All running MSYS processes of
    /// an installation share one mount table, so the first to start decides `/tmp` for the rest.
    /// MSYS turns TMP/TEMP back into POSIX form at startup, so the program still sees `/tmp`.
    /// A child gash also reads TMP/TEMP to find the folder behind `/tmp`.
    pub fn child_env(&mut self, extra: &[(String, String)], native: bool) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let cwd = self.cwd.clone();
        let convert = |k: &str| native || matches!(k, "PATH" | "TMP" | "TEMP");
        for (k, v) in extra {
            seen.insert(k.to_uppercase());
            let val = if convert(k) { crate::path::convert_env_value(k, v, &cwd) } else { v.clone() };
            out.push((k.clone(), val));
        }
        let names = self.vars.names();
        for name in names {
            if seen.contains(&name.to_uppercase()) {
                continue;
            }
            let v = match self.vars.get(&name) {
                Some(v) => v,
                None => continue,
            };
            if !v.has(EXPORT) {
                continue;
            }
            let val = match &v.value {
                Some(Value::Str(s)) => s.clone(),
                _ => continue,
            };
            seen.insert(name.to_uppercase());
            let val = if convert(&name) { crate::path::convert_env_value(&name, &val, &cwd) } else { val };
            out.push((name, val));
        }
        for (k, v) in self.env_extra.iter() {
            if !seen.contains(&k.to_uppercase()) {
                out.push((k.clone(), v.clone()));
            }
        }
        // PWD for children reflects our virtual cwd
        out
    }

    // ---------- functions ----------

    pub fn define_function(&mut self, f: Arc<FuncDef>) {
        Arc::make_mut(&mut self.funcs).insert(f.name.clone(), f);
    }

    // ---------- traps ----------

    pub fn set_trap(&mut self, sig: &str, action: Option<String>) {
        self.trap_display = None;
        let t = Arc::make_mut(&mut self.traps);
        match action {
            Some(a) => {
                t.insert(sig.to_string(), a);
            }
            None => {
                t.remove(sig);
            }
        }
    }

    pub fn status_flow(&self) -> i32 {
        self.status
    }

    pub fn set_status(&mut self, s: i32) {
        self.status = s;
    }

    pub fn fds_get(&self, fd: i32) -> Option<Handle> {
        self.fds.get(fd).cloned()
    }
}

impl crate::arith::ArithEnv for Shell {
    fn arith_get(&mut self, name: &str, sub: Option<&str>) -> Result<String, String> {
        let n = self.resolve_name(name);
        match sub {
            None => {
                let v = self.get_value(&n);
                match v {
                    Some(v) => Ok(v.scalar()),
                    None => {
                        if self.o[O_NOUNSET] {
                            Err(format!("{}: unbound variable", n))
                        } else {
                            Ok(String::new())
                        }
                    }
                }
            }
            Some(sub) => {
                let v = self.get_value(&n);
                match v {
                    Some(Value::Assoc(m)) => Ok(m.get(sub).cloned().unwrap_or_default()),
                    Some(Value::Arr(a)) => {
                        let mut i = crate::arith::eval(sub, self)?;
                        if i < 0 {
                            i += a.keys().next_back().map(|k| k + 1).unwrap_or(0);
                        }
                        Ok(a.get(&i).cloned().unwrap_or_default())
                    }
                    Some(Value::Str(s)) => {
                        let i = crate::arith::eval(sub, self)?;
                        Ok(if i == 0 { s } else { String::new() })
                    }
                    None => {
                        if self.o[O_NOUNSET] {
                            Err(format!("{}[{}]: unbound variable", n, sub))
                        } else {
                            Ok(String::new())
                        }
                    }
                }
            }
        }
    }

    fn arith_set(&mut self, name: &str, sub: Option<&str>, v: i64) -> Result<(), String> {
        match sub {
            None => self.set_var(name, &v.to_string(), false),
            Some(s) => self.set_elem(name, s, &v.to_string(), false),
        }
    }
}
