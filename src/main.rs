//! gash — a fast, native, bash-compatible shell for Windows.
#![allow(dead_code)]

mod arith;
mod ast;
mod builtins;
mod builtins_io;
mod coreutils;
mod du;
mod exec;
mod expand;
mod interactive;
mod io;
mod lineedit;
mod parser;
mod path;
mod pattern;
mod prompt;
mod regex;
mod shell;
mod sys;
mod vars;

use shell::*;
use std::path::PathBuf;
use std::sync::OnceLock;
use vars::*;

pub const BASH_VERSION: &str = "5.2.37(1)-release";
pub const BASH_VERSINFO: &[&str] = &["5", "2", "37", "1", "release", "x86_64-pc-msys"];

pub struct Global {
    pub exe: PathBuf,
    pub exe_posix: String,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

pub fn global() -> &'static Global {
    GLOBAL.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gash.exe"));
        let exe_posix = path::to_posix(&exe);
        let exe_posix = exe_posix.strip_suffix(".exe").map(|s| s.to_string()).unwrap_or(exe_posix);
        Global { exe, exe_posix }
    })
}

struct Cli {
    command: Option<String>,
    script: Option<String>,
    read_stdin: bool,
    interactive: Option<bool>,
    login: bool,
    norc: bool,
    noprofile: bool,
    rcfile: Option<String>,
    set_opts: Vec<(String, bool)>,
    set_letters: Vec<(char, bool)>,
    shopts: Vec<(String, bool)>,
    arg0: Option<String>,
    args: Vec<String>,
    posix: bool,
}

fn usage(name: &str) {
    eprintln!(
        "Usage:\t{name} [option] ...\n\t{name} [option] script-file ...\n\
         Options:\n\t-c command\tread commands from the string\n\t-i\t\tinteractive\n\t-l, --login\tlogin shell\n\
         \t-s\t\tread commands from standard input\n\t--norc, --noprofile, --rcfile FILE, --posix, --version\n\
         \t-o option, -O shopt_option, and any `set` flag (-e -u -x ...)"
    );
}

/// `report`: print errors, `--help` and `--version` (off when checking a script's `#!` line).
fn parse_cli(argv: &[String], name: &str, report: bool) -> Result<Cli, i32> {
    let mut c = Cli {
        command: None,
        script: None,
        read_stdin: false,
        interactive: None,
        login: false,
        norc: false,
        noprofile: false,
        rcfile: None,
        set_opts: vec![],
        set_letters: vec![],
        shopts: vec![],
        arg0: None,
        args: vec![],
        posix: false,
    };
    let mut want_c = false;
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" || a == "-" {
            i += 1;
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            match long {
                "login" => c.login = true,
                "norc" => c.norc = true,
                "noprofile" => c.noprofile = true,
                "rcfile" | "init-file" => {
                    i += 1;
                    c.rcfile = argv.get(i).cloned();
                }
                "posix" => c.posix = true,
                "verbose" => c.set_letters.push(('v', true)),
                "noediting" | "restricted" | "debugger" | "dump-strings" | "dump-po-strings" | "wordexp" | "pretty-print" => {}
                "version" => {
                    if report {
                        println!("gash {} (bash-compatible shell for Windows; BASH_VERSION {})", env!("CARGO_PKG_VERSION"), BASH_VERSION);
                        println!("A from-scratch native replacement for Git Bash. License: MIT.");
                    }
                    return Err(0);
                }
                "help" => {
                    if report {
                        usage(name);
                    }
                    return Err(0);
                }
                _ => {
                    if report {
                        eprintln!("{}: --{}: invalid option", name, long);
                        usage(name);
                    }
                    return Err(2);
                }
            }
            i += 1;
            continue;
        }
        let on = a.starts_with('-');
        if !(on || a.starts_with('+')) || a.len() < 2 {
            break;
        }
        let flags: Vec<char> = a[1..].chars().collect();
        for f in flags {
            match f {
                'c' if on => want_c = true,
                's' if on => c.read_stdin = true,
                'i' => c.interactive = Some(on),
                'l' => c.login = on,
                'r' | 'D' => {}
                'o' => {
                    i += 1;
                    match argv.get(i) {
                        Some(n) => c.set_opts.push((n.clone(), on)),
                        None => {}
                    }
                }
                'O' => {
                    i += 1;
                    if let Some(n) = argv.get(i) {
                        c.shopts.push((n.clone(), on));
                    }
                }
                other => {
                    if set_opt_by_letter(other).is_some() {
                        c.set_letters.push((other, on));
                    } else {
                        if report {
                            eprintln!("{}: -{}: invalid option", name, other);
                            usage(name);
                        }
                        return Err(2);
                    }
                }
            }
        }
        i += 1;
    }
    let rest: Vec<String> = argv[i.min(argv.len())..].to_vec();
    if want_c {
        let mut it = rest.into_iter();
        match it.next() {
            Some(cmd) => c.command = Some(cmd),
            None => {
                if report {
                    eprintln!("{}: -c: option requires an argument", name);
                }
                return Err(2);
            }
        }
        c.arg0 = it.next();
        c.args = it.collect();
    } else if c.read_stdin || rest.is_empty() {
        c.args = rest;
    } else {
        let mut it = rest.into_iter();
        c.script = it.next();
        c.args = it.collect();
    }
    Ok(c)
}

/// Applies the `set` and `shopt` options of a command line (or of a script's `#!` line).
/// Err holds the message for an unknown option name.
fn apply_options(sh: &mut Shell, cli: &Cli) -> Result<(), String> {
    if cli.posix {
        sh.o[O_POSIX] = true;
    }
    for (c, on) in &cli.set_letters {
        if let Some(i) = set_opt_by_letter(*c) {
            sh.o[i] = *on;
        }
    }
    for (n, on) in &cli.set_opts {
        match set_opt_index(n) {
            Some(i) => sh.o[i] = *on,
            None => return Err(format!("{}: invalid option name", n)),
        }
    }
    for (n, on) in &cli.shopts {
        match shopt_index(n) {
            Some(i) => sh.sh[i] = *on,
            None => return Err(format!("{}: invalid shell option name", n)),
        }
    }
    Ok(())
}

fn is_login_argv0(a0: &str) -> bool {
    a0.starts_with('-')
}

fn import_environment(sh: &mut Shell) {
    let mut extra = Vec::new();
    for (k, v) in std::env::vars_os() {
        let k = k.to_string_lossy().into_owned();
        let v = v.to_string_lossy().into_owned();
        if k.is_empty() {
            continue;
        }
        let k = if k.eq_ignore_ascii_case("PATH") { "PATH".to_string() } else { k };
        if !parser::is_valid_name(&k) {
            extra.push((k, v));
            continue;
        }
        let v = if k == "PATH" && (v.contains(';') || path::is_windows_abs(&v)) { path::win_list_to_posix(&v) } else { v };
        let var = sh.vars.global_mut(&k);
        var.value = Some(Value::Str(v));
        var.attrs |= EXPORT;
    }
    sh.env_extra = std::sync::Arc::new(extra);
}

fn setv(sh: &mut Shell, k: &str, v: &str, export: bool) {
    let var = sh.vars.global_mut(k);
    var.value = Some(Value::Str(v.to_string()));
    if export {
        var.attrs |= EXPORT;
    }
}

fn setup_environment(sh: &mut Shell, login: bool) {
    let m = path::mounts();
    // HOME
    let home = match sh.get_str("HOME") {
        Some(h) if !h.is_empty() => {
            if path::is_windows_abs(&h) {
                path::to_posix(std::path::Path::new(&h))
            } else {
                h
            }
        }
        _ => path::to_posix(&expand::home_dir()),
    };
    setv(sh, "HOME", &home, true);
    // MSYSTEM
    let msystem = match sh.get_str("MSYSTEM") {
        Some(s) if !s.is_empty() => s,
        _ => {
            if m.root.join("ucrt64").join("bin").is_dir() {
                "UCRT64".into()
            } else if m.root.join("mingw64").join("bin").is_dir() {
                "MINGW64".into()
            } else if m.root.join("clangarm64").join("bin").is_dir() {
                "CLANGARM64".into()
            } else {
                "MINGW64".into()
            }
        }
    };
    let prefix = format!("/{}", msystem.to_lowercase());
    setv(sh, "MSYSTEM", &msystem, true);
    if sh.get_str("MSYSTEM_PREFIX").is_none() {
        setv(sh, "MSYSTEM_PREFIX", &prefix, true);
        setv(sh, "MINGW_PREFIX", &prefix, true);
        setv(sh, "MSYSTEM_CARCH", "x86_64", true);
        setv(sh, "MSYSTEM_CHOST", "x86_64-w64-mingw32", true);
        setv(sh, "MINGW_CHOST", "x86_64-w64-mingw32", true);
        let pkg = match msystem.as_str() {
            "UCRT64" => "mingw-w64-ucrt-x86_64",
            "CLANGARM64" => "mingw-w64-clang-aarch64",
            _ => "mingw-w64-x86_64",
        };
        setv(sh, "MINGW_PACKAGE_PREFIX", pkg, true);
    }
    // PATH: make sure the MSYS tool directories are present (like Git Bash's launcher + /etc/profile).
    let cur = sh.get_str("PATH").unwrap_or_default();
    if sh.get_str("ORIGINAL_PATH").is_none() {
        setv(sh, "ORIGINAL_PATH", &cur, true);
    }
    let mut parts = path::split_posix_list(&cur);
    let mut front: Vec<String> = Vec::new();
    let want = if login { vec![format!("{}/bin", home), format!("{}/bin", prefix), "/usr/local/bin".to_string(), "/usr/bin".to_string(), "/bin".to_string()] } else { vec![format!("{}/bin", prefix), "/usr/bin".to_string()] };
    for w in want {
        let present = parts.iter().any(|p| p == &w || (w == "/bin" && p == "/usr/bin" && false));
        if !present {
            if w == "/bin" && parts.iter().chain(front.iter()).any(|p| p == "/usr/bin") {
                continue;
            }
            if w == "/usr/local/bin" && !m.root.join("usr").join("local").join("bin").is_dir() {
                continue;
            }
            front.push(w);
        }
    }
    front.append(&mut parts);
    // If gash is installed as bash/sh (bash.exe or sh.exe beside gash.exe), make that folder
    // win over Git's /usr/bin, so `bash`, `sh` and child processes that run them get gash —
    // just as Git Bash puts its own bash first. ~/bin and anything earlier still take precedence.
    if let Some(own) = own_shell_dir() {
        front.retain(|p| !p.eq_ignore_ascii_case(&own));
        let at = front.iter().position(|p| p == "/usr/bin" || p == "/bin").unwrap_or(0);
        front.insert(at, own);
    }
    setv(sh, "PATH", &front.join(":"), true);
    // misc
    let exe_posix = global().exe_posix.clone();
    if sh.get_str("SHELL").map(|s| s.is_empty() || path::is_windows_abs(&s)).unwrap_or(true) {
        setv(sh, "SHELL", &exe_posix, true);
    }
    if sh.get_str("TERM").map(|s| s.is_empty()).unwrap_or(true) {
        setv(sh, "TERM", "xterm-256color", true);
    }
    if sh.get_str("USER").is_none() {
        let u = sys::user_name();
        setv(sh, "USER", &u, true);
    }
    if sh.get_str("LOGNAME").is_none() {
        let u = sh.get_str("USER").unwrap_or_default();
        setv(sh, "LOGNAME", &u, false);
    }
    if login && sh.get_str("LANG").is_none() && sh.get_str("LC_ALL").is_none() && sh.get_str("LC_CTYPE").is_none() {
        setv(sh, "LANG", "en_US.UTF-8", true);
    }
    for t in ["TMP", "TEMP"] {
        let cur = sh.get_str(t).unwrap_or_default();
        if !cur.is_empty() {
            let orig = format!("ORIGINAL_{}", t);
            if sh.get_str(&orig).is_none() {
                setv(sh, &orig, &cur, true);
            }
        }
        setv(sh, t, "/tmp", true);
    }
    if sh.get_str("HOSTNAME").is_none() {
        let h = sys::host_name();
        setv(sh, "HOSTNAME", &h, false);
    }
    if sh.get_str("TITLEPREFIX").is_none() {
        setv(sh, "TITLEPREFIX", &msystem, false);
    }
    if sh.get_str("CONFIG_SITE").is_none() && login {
        setv(sh, "CONFIG_SITE", "/etc/config.site", true);
    }
    let lvl = sh.get_str("SHLVL").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    setv(sh, "SHLVL", &(lvl + 1).to_string(), true);
    let pwd = path::to_posix(&sh.cwd);
    setv(sh, "PWD", &pwd, true);
    if sh.vars.get("OLDPWD").is_none() {
        let v = sh.vars.global_mut("OLDPWD");
        v.attrs |= EXPORT;
    }
    if sh.get_str("HISTFILE").map(|s| s.is_empty()).unwrap_or(true) {
        setv(sh, "HISTFILE", &format!("{}/.bash_history", home), false);
    }
    if sh.get_str("HISTSIZE").is_none() {
        setv(sh, "HISTSIZE", "500", false);
    }
    if sh.get_str("HISTFILESIZE").is_none() {
        setv(sh, "HISTFILESIZE", "500", false);
    }
}

/// MSYS-form folder of our executable, if it also provides `bash.exe` or `sh.exe`
/// (or we are ourselves running as bash/sh).
fn own_shell_dir() -> Option<String> {
    let exe = &global().exe;
    let dir = exe.parent()?;
    let stem = exe.file_stem()?.to_string_lossy().to_ascii_lowercase();
    let provides = stem == "bash" || stem == "sh" || dir.join("bash.exe").is_file() || dir.join("sh.exe").is_file();
    if !provides || path::is_msys_binary(exe) {
        return None;
    }
    Some(path::to_posix(dir))
}

fn source_if_exists(sh: &mut Shell, posix: &str) -> Option<R> {
    let p = path::to_win(posix, &sh.cwd);
    if !p.is_file() {
        return None;
    }
    let src = exec::read_script_file(&p).ok()?;
    let saved = std::mem::replace(&mut sh.source_name, posix.to_string());
    sh.sourcing += 1;
    let r = sh.run_source(&src, false);
    sh.sourcing -= 1;
    sh.source_name = saved;
    Some(match r {
        Err(Flow::Return(s)) => Ok(s),
        r => r,
    })
}

fn real_main(argv: Vec<String>) -> i32 {
    path::init();
    let argv0 = argv.first().cloned().unwrap_or_else(|| "gash".into());
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or("gash").trim_start_matches('-');
    let base = base.strip_suffix(".exe").or_else(|| base.strip_suffix(".EXE")).unwrap_or(base).to_string();
    let name = if base.is_empty() { "gash".to_string() } else { base };
    let cli = match parse_cli(&argv, &name, true) {
        Ok(c) => c,
        Err(code) => return code,
    };
    let _ = global();
    sys::install_ctrl_handler();
    sys::make_append_only_stdio_writable();

    let mut sh = Shell::new(&name);
    import_environment(&mut sh);
    let login = cli.login || is_login_argv0(&argv0);
    setup_environment(&mut sh, login);
    sh.init_shell_vars();

    let stdin_tty = sys::is_console(sys::STD_INPUT_HANDLE);
    let interactive = cli.interactive.unwrap_or(cli.command.is_none() && cli.script.is_none() && stdin_tty);
    sh.interactive = interactive;
    if interactive && stdin_tty {
        sys::clear_ctrl_c_ignore();
    }
    if login {
        sh.sh[sho::login_shell()] = true;
    }
    if name == "sh" {
        sh.o[O_POSIX] = true;
    }
    if let Err(msg) = apply_options(&mut sh, &cli) {
        eprintln!("{}: {}", name, msg);
        return 2;
    }
    if interactive {
        sh.sh[sho::expand_aliases()] = true;
        sh.o[O_HISTEXPAND] = true;
        sh.o[O_HISTORY] = true;
        sh.o[O_MONITOR] = true;
        sh.o[O_EMACS] = true;
        if sh.get_str("PS1").is_none() {
            setv(
                &mut sh,
                "PS1",
                "\\[\\033]0;$TITLEPREFIX:$PWD\\007\\]\\n\\[\\033[32m\\]\\u@\\h \\[\\033[35m\\]$MSYSTEM \\[\\033[33m\\]\\w\\[\\033[36m\\]`__git_ps1`\\[\\033[0m\\]\\n$ ",
                false,
            );
        }
        // Git for Windows' default aliases (/etc/profile.d/aliases.sh)
        let a = std::sync::Arc::make_mut(&mut sh.aliases);
        a.insert("ls".into(), "ls -F --color=auto --show-control-chars".into());
        a.insert("ll".into(), "ls -l".into());
    }
    if sh.get_str("PS1").is_none() {
        // non-interactive shells have no PS1
    }

    // positional parameters / $0
    match (&cli.command, &cli.script) {
        (Some(_), _) => {
            sh.arg0 = cli.arg0.clone().unwrap_or_else(|| argv0.clone());
            sh.source_name = name.clone();
        }
        (None, Some(s)) => {
            sh.arg0 = s.clone();
            sh.source_name = s.clone();
        }
        _ => {
            sh.arg0 = argv0.clone();
            sh.source_name = name.clone();
        }
    }
    sh.positional = cli.args.clone();

    // startup files
    let home = sh.get_str("HOME").unwrap_or_default();
    let run_startup = |sh: &mut Shell, r: Option<R>| -> Option<i32> {
        if let Some(Err(Flow::Exit(s))) = r {
            return Some(sh.finish_main(Err(Flow::Exit(s))));
        }
        None
    };
    if login && !cli.noprofile {
        if std::env::var_os("GASH_ETC_PROFILE").is_some() {
            let r = source_if_exists(&mut sh, "/etc/profile");
            if let Some(s) = run_startup(&mut sh, r) {
                return s;
            }
        }
        for f in [".bash_profile", ".bash_login", ".profile"] {
            let p = format!("{}/{}", home, f);
            if let Some(r) = source_if_exists(&mut sh, &p) {
                if let Some(s) = run_startup(&mut sh, Some(r)) {
                    return s;
                }
                break;
            }
        }
    } else if interactive && !cli.norc {
        let rc = cli.rcfile.clone().unwrap_or_else(|| format!("{}/.bashrc", home));
        let r = source_if_exists(&mut sh, &rc);
        if let Some(s) = run_startup(&mut sh, r) {
            return s;
        }
    } else if !interactive {
        if let Some(be) = sh.get_str("BASH_ENV") {
            if !be.is_empty() {
                let _ = source_if_exists(&mut sh, &be);
            }
        }
    }

    if let Some(cmd) = &cli.command {
        return sh.run_command_string(cmd.strip_prefix('\u{feff}').unwrap_or(cmd));
    }
    if let Some(script) = &cli.script {
        let wp = path::to_win(script, &sh.cwd);
        let wp = if wp.is_file() {
            wp
        } else {
            // bash searches PATH for the script if not found
            match sh.find_command(script) {
                Some(r) if r.path.is_file() => r.path,
                _ => {
                    eprintln!("{}: {}: No such file or directory", name, script);
                    return 127;
                }
            }
        };
        let src = match exec::read_script_file(&wp) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}: {}", name, script, exec::io_err_msg(&e));
                return 126;
            }
        };
        sh.script_mode = true;
        return sh.run_main(&src);
    }
    if interactive && stdin_tty {
        sys::enable_vt_output();
        let _cp = sys::set_utf8_console();
        return interactive::repl(&mut sh);
    }
    // commands from stdin
    let data = io::Handle::Stdin.read_to_end().unwrap_or_default();
    let mut src = String::from_utf8_lossy(&data).into_owned();
    if src.starts_with('\u{feff}') {
        src.remove(0);
    }
    if src.contains("\r\n") {
        src = src.replace("\r\n", "\n");
    }
    if interactive {
        // -i with piped input: execute but keep going after errors
        let r = sh.run_source(&src, false);
        return sh.finish_main(r);
    }
    sh.run_main(&src)
}

fn main() {
    let argv: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
    let h = std::thread::Builder::new().stack_size(STACK_SIZE).spawn(move || real_main(argv)).expect("failed to start interpreter thread");
    let code = h.join().unwrap_or(1);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}
