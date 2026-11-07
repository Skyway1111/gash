//! Shell builtins (part 1): dispatch, control flow, directories, options, aliases, jobs.

pub use crate::builtins_io::*;
use crate::expand::shell_quote;
use crate::path;
use crate::shell::*;
use crate::vars::*;
use std::sync::Arc;

pub const BUILTINS: &[&str] = &[
    ":", ".", "[", "__git_ps1", "alias", "bg", "bind", "break", "builtin", "caller", "cd", "command", "compgen", "complete",
    "compopt", "continue", "declare", "dirs", "disown", "echo", "enable", "eval", "exec", "exit", "export", "false", "fc",
    "fg", "getopts", "hash", "help", "history", "jobs", "kill", "let", "local", "logout", "mapfile", "popd", "printf",
    "pushd", "pwd", "read", "readarray", "readonly", "return", "set", "shift", "shopt", "source", "suspend", "test",
    "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset", "wait",
];

pub const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until", "do", "done", "in", "function",
    "time", "{", "}", "!", "[[", "]]", "coproc",
];

pub fn is_builtin(name: &str) -> bool {
    BUILTINS.binary_search(&name).is_ok() || BUILTINS.contains(&name)
}

pub fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "." | "break" | "continue" | "eval" | "exec" | "exit" | "export" | "readonly" | "return" | "set" | "shift"
            | "trap" | "unset" | "source"
    )
}

fn usage_err(sh: &mut Shell, name: &str, msg: &str) -> R {
    sh.eprint(&format!("{}: {}", name, msg));
    Ok(2)
}

pub fn run(sh: &mut Shell, argv: &[String]) -> R {
    let name = argv[0].as_str();
    let args = &argv[1..];
    match name {
        ":" | "true" => Ok(0),
        "false" => Ok(1),
        "echo" => echo(sh, args),
        "printf" => printf(sh, args),
        "cd" => cd(sh, args),
        "pwd" => pwd(sh, args),
        "exit" | "logout" => exit(sh, args),
        "return" => ret(sh, args),
        "break" | "continue" => brk(sh, name, args),
        "export" => export(sh, args),
        "readonly" => readonly(sh, args),
        "declare" | "typeset" | "local" => declare(sh, name, args),
        "unset" => unset(sh, args),
        "set" => set(sh, args),
        "shift" => shift(sh, args),
        "source" | "." => source(sh, name, args),
        "eval" => eval(sh, args),
        "exec" => exec(sh, args),
        "test" | "[" => test(sh, name, args),
        "read" => read(sh, args),
        "mapfile" | "readarray" => mapfile(sh, name, args),
        "alias" => alias(sh, args),
        "unalias" => unalias(sh, args),
        "type" => type_(sh, args),
        "command" => command(sh, args),
        "builtin" => builtin(sh, args),
        "hash" => hash(sh, args),
        "getopts" => getopts(sh, args),
        "let" => let_(sh, args),
        "trap" => trap(sh, args),
        "wait" => wait(sh, args),
        "jobs" => jobs(sh, args),
        "fg" => fg(sh, args),
        "bg" => Ok(0),
        "kill" => kill(sh, args),
        "disown" => disown(sh, args),
        "shopt" => shopt(sh, args),
        "pushd" => pushd(sh, args),
        "popd" => popd(sh, args),
        "dirs" => dirs(sh, args),
        "umask" => umask(sh, args),
        "times" => times(sh),
        "ulimit" => ulimit(sh, args),
        "enable" => enable(sh, args),
        "help" => help(sh, args),
        "history" => history(sh, args),
        "caller" => caller(sh, args),
        "compgen" => compgen(sh, args),
        "complete" | "compopt" | "bind" => Ok(0),
        "suspend" => usage_err(sh, name, "cannot suspend a shell on Windows"),
        "fc" => usage_err(sh, name, "not supported"),
        "__git_ps1" => git_ps1(sh, args),
        _ => {
            sh.eprint(&format!("{}: not a shell builtin", name));
            Ok(1)
        }
    }
}

// ---------------- echo ----------------

#[derive(Clone, Copy, PartialEq)]
pub enum Esc {
    AnsiC,
    Echo,
    PrintfFmt,
    /// printf %b: like echo -e, but \nnn (no leading 0) is also octal
    PrintfB,
}

/// Process backslash escapes. Returns (bytes, stop_requested_by_\c).
pub fn unescape(s: &str, mode: Esc) -> (Vec<u8>, bool) {
    let c: Vec<char> = s.chars().collect();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    let push_char = |out: &mut Vec<u8>, ch: char| {
        let mut b = [0u8; 4];
        out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
    };
    while i < c.len() {
        let ch = c[i];
        if ch != '\\' || i + 1 >= c.len() {
            push_char(&mut out, ch);
            i += 1;
            continue;
        }
        let e = c[i + 1];
        i += 2;
        match e {
            'a' => out.push(7),
            'b' => out.push(8),
            'e' | 'E' => out.push(0x1b),
            'f' => out.push(12),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'v' => out.push(11),
            '\\' => out.push(b'\\'),
            '\'' if mode != Esc::Echo && mode != Esc::PrintfB => out.push(b'\''),
            '"' if mode != Esc::Echo && mode != Esc::PrintfB => out.push(b'"'),
            '?' if mode == Esc::AnsiC => out.push(b'?'),
            'c' => {
                if mode == Esc::AnsiC {
                    if i < c.len() {
                        let x = c[i];
                        i += 1;
                        out.push(((x.to_ascii_uppercase() as u32) ^ 0x40) as u8);
                    }
                } else {
                    return (out, true);
                }
            }
            '0'..='7' => {
                let (maxd, start_val) = if mode == Esc::Echo || (mode == Esc::PrintfB && e == '0') {
                    if e == '0' {
                        (3, 0u32)
                    } else {
                        push_char(&mut out, '\\');
                        push_char(&mut out, e);
                        continue;
                    }
                } else {
                    (2, e.to_digit(8).unwrap())
                };
                let mut v = start_val;
                let mut n = 0;
                while n < maxd && i < c.len() && c[i].is_digit(8) {
                    v = v * 8 + c[i].to_digit(8).unwrap();
                    i += 1;
                    n += 1;
                }
                out.push((v & 0xff) as u8);
            }
            'x' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 2 && i < c.len() && c[i].is_ascii_hexdigit() {
                    v = v * 16 + c[i].to_digit(16).unwrap();
                    i += 1;
                    n += 1;
                }
                if n == 0 {
                    out.extend_from_slice(b"\\x");
                } else {
                    out.push(v as u8);
                }
            }
            'u' | 'U' => {
                let max = if e == 'u' { 4 } else { 8 };
                let mut v = 0u32;
                let mut n = 0;
                while n < max && i < c.len() && c[i].is_ascii_hexdigit() {
                    v = v.wrapping_mul(16) + c[i].to_digit(16).unwrap();
                    i += 1;
                    n += 1;
                }
                if n == 0 {
                    push_char(&mut out, '\\');
                    push_char(&mut out, e);
                } else if let Some(ch) = char::from_u32(v) {
                    push_char(&mut out, ch);
                }
            }
            other => {
                push_char(&mut out, '\\');
                push_char(&mut out, other);
            }
        }
    }
    (out, false)
}

fn echo(sh: &mut Shell, args: &[String]) -> R {
    let mut newline = true;
    let mut esc = sh.sh[sho::xpg_echo()];
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            for c in a[1..].chars() {
                match c {
                    'n' => newline = false,
                    'e' => esc = true,
                    'E' => esc = false,
                    _ => {}
                }
            }
            i += 1;
        } else {
            break;
        }
    }
    let mut out: Vec<u8> = Vec::new();
    for (k, a) in args[i..].iter().enumerate() {
        if k > 0 {
            out.push(b' ');
        }
        if esc {
            let (b, stop) = unescape(a, Esc::Echo);
            out.extend_from_slice(&b);
            if stop {
                sh.out(1, &out)?;
                return Ok(0);
            }
        } else {
            out.extend_from_slice(a.as_bytes());
        }
    }
    if newline {
        out.push(b'\n');
    }
    sh.out(1, &out)?;
    Ok(0)
}

// ---------------- directories ----------------

pub fn change_dir(sh: &mut Shell, target: &str, physical: bool) -> Result<(), String> {
    let cur_pwd = sh.get_str("PWD").unwrap_or_else(|| path::to_posix(&sh.cwd));
    let newpwd = if path::is_windows_abs(target) {
        path::to_posix(&path::to_win(target, &sh.cwd))
    } else if target.starts_with('/') {
        path::normalize_posix(target)
    } else {
        path::normalize_posix(&format!("{}/{}", cur_pwd.trim_end_matches('/'), target))
    };
    let mut win = path::to_win(&newpwd, &sh.cwd);
    if !win.is_dir() {
        // maybe logical path failed because of symlinks; try relative to physical cwd
        let w2 = path::to_win(target, &sh.cwd);
        if w2.is_dir() && !target.starts_with('/') {
            win = w2;
        } else if win.exists() || w2.exists() {
            return Err(format!("{}: Not a directory", target));
        } else {
            return Err(format!("{}: No such file or directory", target));
        }
    }
    if std::fs::read_dir(&win).is_err() {
        return Err(format!("{}: Permission denied", target));
    }
    let final_pwd = if physical {
        match std::fs::canonicalize(&win) {
            Ok(c) => {
                let s = c.to_string_lossy().into_owned();
                let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
                win = std::path::PathBuf::from(&s);
                path::to_posix(&win)
            }
            Err(_) => newpwd.clone(),
        }
    } else {
        path::to_posix(&path::to_win(&newpwd, &sh.cwd)).replace("//", "/")
    };
    let final_pwd = if newpwd.starts_with('/') && !physical && !path::is_windows_abs(target) { newpwd } else { final_pwd };
    sh.cwd = win;
    let _ = sh.set_str("OLDPWD", &cur_pwd);
    let _ = sh.set_str("PWD", &final_pwd);
    sh.export("OLDPWD");
    Ok(())
}

fn cd(sh: &mut Shell, args: &[String]) -> R {
    let mut physical = sh.o[O_PHYSICAL];
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        match args[i].as_str() {
            "-P" => physical = true,
            "-L" => physical = false,
            "-e" | "-@" => {}
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
    }
    let rest = &args[i..];
    if rest.len() > 1 {
        sh.eprint("cd: too many arguments");
        return Ok(1);
    }
    let mut print = false;
    let target = match rest.first() {
        None => match sh.get_str("HOME") {
            Some(h) if !h.is_empty() => h,
            _ => {
                sh.eprint("cd: HOME not set");
                return Ok(1);
            }
        },
        Some(t) if t == "-" => match sh.get_str("OLDPWD") {
            Some(o) if !o.is_empty() => {
                print = true;
                o
            }
            _ => {
                sh.eprint("cd: OLDPWD not set");
                return Ok(1);
            }
        },
        Some(t) if t.is_empty() => return Ok(0),
        Some(t) => t.clone(),
    };
    // CDPATH
    if !target.starts_with('/') && !target.starts_with('.') && !path::is_windows_abs(&target) {
        if let Some(cdpath) = sh.get_str("CDPATH") {
            for d in path::split_posix_list(&cdpath) {
                if d.is_empty() {
                    continue;
                }
                let cand = format!("{}/{}", d.trim_end_matches('/'), target);
                if path::to_win(&cand, &sh.cwd).is_dir() {
                    return match change_dir(sh, &cand, physical) {
                        Ok(()) => {
                            let p = sh.get_str_or_empty("PWD");
                            sh.out(1, format!("{}\n", p).as_bytes())?;
                            Ok(0)
                        }
                        Err(e) => {
                            sh.eprint(&format!("cd: {}", e));
                            Ok(1)
                        }
                    };
                }
            }
        }
    }
    match change_dir(sh, &target, physical) {
        Ok(()) => {
            if print {
                let p = sh.get_str_or_empty("PWD");
                sh.out(1, format!("{}\n", p).as_bytes())?;
            }
            Ok(0)
        }
        Err(e) => {
            if sh.sh[sho::cdable_vars()] {
                if let Some(v) = sh.get_str(&target) {
                    if change_dir(sh, &v, physical).is_ok() {
                        return Ok(0);
                    }
                }
            }
            sh.eprint(&format!("cd: {}", e));
            Ok(1)
        }
    }
}

pub fn physical_pwd(sh: &Shell) -> String {
    path::to_posix(&physical_cwd(sh))
}

/// The current directory with symlinks resolved, as a Windows path.
fn physical_cwd(sh: &Shell) -> std::path::PathBuf {
    match std::fs::canonicalize(&sh.cwd) {
        Ok(c) => {
            let s = c.to_string_lossy().into_owned();
            std::path::PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s))
        }
        Err(_) => sh.cwd.clone(),
    }
}

fn pwd(sh: &mut Shell, args: &[String]) -> R {
    // -L, -P and MSYS bash's -W (the physical directory in Windows form, `C:/...`) each choose a
    // form; as in Git Bash, the last one given wins.
    let mut form = if sh.o[O_PHYSICAL] { 'P' } else { 'L' };
    for a in args {
        if a == "--" || a == "-" || !a.starts_with('-') {
            break;
        }
        for c in a[1..].chars() {
            match c {
                'L' | 'P' | 'W' => form = c,
                _ => {
                    sh.eprint(&format!("pwd: -{}: invalid option", c));
                    sh.eprint("pwd: usage: pwd [-LPW]");
                    return Ok(2);
                }
            }
        }
    }
    let p = match form {
        'P' => physical_pwd(sh),
        'W' => path::win_fwd(&physical_cwd(sh)),
        _ => match sh.get_str("PWD") {
            Some(p) if !p.is_empty() && path::to_win(&p, &sh.cwd).is_dir() => p,
            _ => path::to_posix(&sh.cwd),
        },
    };
    sh.out(1, format!("{}\n", p).as_bytes())?;
    Ok(0)
}

fn dirs_list(sh: &mut Shell) -> Vec<String> {
    let mut v = vec![sh.get_str("PWD").unwrap_or_default()];
    v.extend(sh.dirstack.iter().rev().cloned());
    v
}

fn tilde_short(sh: &mut Shell, p: &str) -> String {
    let home = sh.get_str("HOME").unwrap_or_default();
    if !home.is_empty() && (p == home || p.starts_with(&format!("{}/", home))) {
        format!("~{}", &p[home.len()..])
    } else {
        p.to_string()
    }
}

fn print_dirs(sh: &mut Shell, long: bool, per_line: bool, numbered: bool) -> R {
    let v = dirs_list(sh);
    let items: Vec<String> = v.iter().map(|d| if long { d.clone() } else { tilde_short(sh, d) }).collect();
    let s = if numbered {
        items.iter().enumerate().map(|(i, d)| format!("{:2}  {}\n", i, d)).collect::<String>()
    } else if per_line {
        items.iter().map(|d| format!("{}\n", d)).collect::<String>()
    } else {
        format!("{}\n", items.join(" "))
    };
    sh.out(1, s.as_bytes())?;
    Ok(0)
}

fn dirs(sh: &mut Shell, args: &[String]) -> R {
    let mut long = false;
    let mut per_line = false;
    let mut numbered = false;
    for a in args {
        match a.as_str() {
            "-c" => {
                sh.dirstack.clear();
                return Ok(0);
            }
            "-l" => long = true,
            "-p" => per_line = true,
            "-v" => {
                numbered = true;
                per_line = true
            }
            _ => {}
        }
    }
    print_dirs(sh, long, per_line, numbered)
}

fn pushd(sh: &mut Shell, args: &[String]) -> R {
    let quiet = args.iter().any(|a| a == "-n");
    let args: Vec<&String> = args.iter().filter(|a| *a != "-n").collect();
    let cur = sh.get_str("PWD").unwrap_or_default();
    match args.first() {
        None => {
            let top = match sh.dirstack.pop() {
                Some(t) => t,
                None => {
                    sh.eprint("pushd: no other directory");
                    return Ok(1);
                }
            };
            if let Err(e) = change_dir(sh, &top, false) {
                sh.dirstack.push(top);
                sh.eprint(&format!("pushd: {}", e));
                return Ok(1);
            }
            sh.dirstack.push(cur);
        }
        Some(a) if a.starts_with('+') || (a.starts_with('-') && a.len() > 1) => {
            let n: usize = a[1..].parse().unwrap_or(0);
            let mut all = dirs_list(sh);
            if n >= all.len() {
                sh.eprint(&format!("pushd: {}: directory stack index out of range", a));
                return Ok(1);
            }
            let k = if a.starts_with('+') { n } else { all.len() - 1 - n };
            all.rotate_left(k);
            let first = all[0].clone();
            if let Err(e) = change_dir(sh, &first, false) {
                sh.eprint(&format!("pushd: {}", e));
                return Ok(1);
            }
            sh.dirstack = all[1..].iter().rev().cloned().collect();
        }
        Some(d) => {
            let d = d.to_string();
            if let Err(e) = change_dir(sh, &d, false) {
                sh.eprint(&format!("pushd: {}", e));
                return Ok(1);
            }
            sh.dirstack.push(cur);
        }
    }
    if !quiet {
        print_dirs(sh, false, false, false)?;
    }
    Ok(0)
}

fn popd(sh: &mut Shell, args: &[String]) -> R {
    let quiet = args.iter().any(|a| a == "-n");
    let idx = args.iter().find(|a| a.starts_with('+') || (a.starts_with('-') && *a != "-n"));
    if sh.dirstack.is_empty() {
        sh.eprint("popd: directory stack empty");
        return Ok(1);
    }
    match idx {
        Some(a) => {
            let n: usize = a[1..].parse().unwrap_or(0);
            let mut all = dirs_list(sh);
            if n >= all.len() {
                sh.eprint(&format!("popd: {}: directory stack index out of range", a));
                return Ok(1);
            }
            let k = if a.starts_with('+') { n } else { all.len() - 1 - n };
            if k == 0 {
                let next = all[1].clone();
                if let Err(e) = change_dir(sh, &next, false) {
                    sh.eprint(&format!("popd: {}", e));
                    return Ok(1);
                }
                sh.dirstack.pop();
            } else {
                all.remove(k);
                sh.dirstack = all[1..].iter().rev().cloned().collect();
            }
        }
        None => {
            let top = sh.dirstack.pop().unwrap();
            if let Err(e) = change_dir(sh, &top, false) {
                sh.eprint(&format!("popd: {}", e));
                return Ok(1);
            }
        }
    }
    if !quiet {
        print_dirs(sh, false, false, false)?;
    }
    Ok(0)
}

// ---------------- control ----------------

fn parse_status(sh: &mut Shell, name: &str, a: Option<&String>) -> Result<i32, i32> {
    match a {
        None => Ok(sh.status),
        Some(s) => match s.trim().parse::<i64>() {
            Ok(n) => Ok((n & 0xff) as i32),
            Err(_) => {
                sh.eprint(&format!("{}: {}: numeric argument required", name, s));
                Err(2)
            }
        },
    }
}

fn exit(sh: &mut Shell, args: &[String]) -> R {
    let st = match parse_status(sh, "exit", args.first()) {
        Ok(s) => s,
        Err(s) => s,
    };
    if sh.interactive && sh.subshell == 0 && !sh.is_subshell {
        sh.write_fd(2, b"exit\n");
    }
    Err(Flow::Exit(st))
}

fn ret(sh: &mut Shell, args: &[String]) -> R {
    if sh.func_depth == 0 && sh.sourcing == 0 {
        sh.eprint("return: can only `return' from a function or sourced script");
        return Ok(1);
    }
    let st = match parse_status(sh, "return", args.first()) {
        Ok(s) => s,
        Err(s) => s,
    };
    Err(Flow::Return(st))
}

fn brk(sh: &mut Shell, name: &str, args: &[String]) -> R {
    let n: usize = match args.first() {
        None => 1,
        Some(a) => match a.parse::<i64>() {
            Ok(n) if n >= 1 => n as usize,
            Ok(_) => {
                sh.eprint(&format!("{}: {}: loop count out of range", name, a));
                return Ok(1);
            }
            Err(_) => {
                sh.eprint(&format!("{}: {}: numeric argument required", name, a));
                return Ok(1);
            }
        },
    };
    if sh.loop_depth == 0 {
        sh.eprint(&format!("{}: only meaningful in a `for', `while', or `until' loop", name));
        return Ok(0);
    }
    let n = n.min(sh.loop_depth);
    if name == "break" {
        Err(Flow::Break(n))
    } else {
        Err(Flow::Continue(n))
    }
}

fn find_source_file(sh: &mut Shell, f: &str) -> Option<std::path::PathBuf> {
    if !f.contains('/') && !f.contains('\\') && sh.sh[sho::sourcepath()] {
        let pv = sh.get_str("PATH").unwrap_or_default();
        for d in path::split_posix_list(&pv) {
            if d.is_empty() {
                continue;
            }
            let p = path::to_win(&format!("{}/{}", d, f), &sh.cwd);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let p = path::to_win(f, &sh.cwd);
    if p.is_file() {
        return Some(p);
    }
    None
}

fn source(sh: &mut Shell, name: &str, args: &[String]) -> R {
    let args: Vec<String> = if args.first().map(|a| a == "--").unwrap_or(false) { args[1..].to_vec() } else { args.to_vec() };
    let f = match args.first() {
        Some(f) => f.clone(),
        None => {
            sh.eprint(&format!("{}: filename argument required", name));
            return Ok(2);
        }
    };
    let p = match find_source_file(sh, &f) {
        Some(p) => p,
        None => {
            sh.eprint(&format!("{}: No such file or directory", f));
            return Ok(1);
        }
    };
    let src = match crate::exec::read_script_file(&p) {
        Ok(s) => s,
        Err(e) => {
            sh.eprint(&format!("{}: {}", f, crate::exec::io_err_msg(&e)));
            return Ok(1);
        }
    };
    let saved_pos = if args.len() > 1 { Some(std::mem::replace(&mut sh.positional, args[1..].to_vec())) } else { None };
    // As in bash, a sourced file gets a frame of its own: FUNCNAME "source", and BASH_SOURCE and
    // BASH_LINENO say where it was sourced from.
    sh.call_stack.push(Frame { func: "source".into(), source: sh.source_name.clone(), call_line: sh.lineno });
    let saved_name = std::mem::replace(&mut sh.source_name, f.clone());
    let saved_line = sh.lineno;
    sh.sourcing += 1;
    let r = sh.run_nested(&src);
    sh.sourcing -= 1;
    sh.call_stack.pop();
    sh.source_name = saved_name;
    sh.lineno = saved_line;
    if let Some(p) = saved_pos {
        if sh.positional == args[1..] {
            sh.positional = p;
        }
    }
    let had_rt = sh.traps.get("RETURN").is_some();
    let r = match r {
        Ok(s) => Ok(s),
        Err(Flow::Return(s)) => Ok(s),
        Err(e) => Err(e),
    };
    if had_rt {
        sh.run_trap("RETURN")?;
    }
    r
}

fn eval(sh: &mut Shell, args: &[String]) -> R {
    let args = if args.first().map(|a| a == "--").unwrap_or(false) { &args[1..] } else { args };
    if args.is_empty() {
        return Ok(0);
    }
    let src = args.join(" ");
    let saved_line = sh.lineno;
    let r = sh.run_nested(&src);
    sh.lineno = saved_line;
    r
}

fn exec(sh: &mut Shell, args: &[String]) -> R {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        match args[i].as_str() {
            "--" => {
                i += 1;
                break;
            }
            "-a" => i += 2,
            "-c" | "-l" | "-cl" | "-lc" => i += 1,
            _ => break,
        }
    }
    let rest = &args[i.min(args.len())..];
    if rest.is_empty() {
        sh.keep_redirs = true;
        return Ok(0);
    }
    let st = run_argv(sh, rest, false)?;
    Err(Flow::Exit(st))
}

/// Run argv as a command (used by `command`, `exec`, `builtin`).
pub fn run_argv(sh: &mut Shell, argv: &[String], skip_funcs: bool) -> R {
    let name = argv[0].clone();
    if !skip_funcs {
        if let Some(f) = sh.funcs.get(&name).cloned() {
            return sh.run_function(&f, &argv[1..]);
        }
    }
    if is_builtin(&name) && !sh.disabled_builtins.contains(&name) {
        return run(sh, argv);
    }
    match sh.find_command(&name) {
        Some(r) => sh.run_external(&r, argv, &[]),
        None => {
            sh.eprint(&format!("{}: command not found", name));
            Ok(127)
        }
    }
}

// ---------------- set / shopt ----------------

fn print_vars(sh: &mut Shell) -> R {
    let mut out = String::new();
    for (name, v) in sh.vars.visible() {
        match &v.value {
            Some(Value::Str(s)) => out.push_str(&format!("{}={}\n", name, shell_quote(s))),
            Some(Value::Arr(a)) => {
                let items: Vec<String> = a.iter().map(|(k, v)| format!("[{}]={}", k, decl_quote(v))).collect();
                out.push_str(&format!("{}=({})\n", name, items.join(" ")));
            }
            Some(Value::Assoc(m)) => {
                let items: Vec<String> = m.iter().map(|(k, v)| format!("[{}]={}", decl_key(k), decl_quote(v))).collect();
                out.push_str(&format!("{}=({} )\n", name, items.join(" ")));
            }
            None => {}
        }
    }
    sh.out(1, out.as_bytes())?;
    Ok(0)
}

fn set(sh: &mut Shell, args: &[String]) -> R {
    if args.is_empty() {
        return print_vars(sh);
    }
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            sh.positional = args[i + 1..].to_vec();
            return Ok(0);
        }
        if a == "-" {
            sh.o[O_XTRACE] = false;
            sh.o[O_VERBOSE] = false;
            if i + 1 < args.len() {
                sh.positional = args[i + 1..].to_vec();
            }
            return Ok(0);
        }
        let on = a.starts_with('-');
        if !(on || a.starts_with('+')) || a.len() < 2 {
            sh.positional = args[i..].to_vec();
            return Ok(0);
        }
        let flags: Vec<char> = a[1..].chars().collect();
        for c in flags {
            if c == 'o' {
                i += 1;
                match args.get(i) {
                    None => {
                        // print options
                        let mut out = String::new();
                        for (k, (n, _)) in SET_OPTIONS.iter().enumerate() {
                            if on {
                                out.push_str(&format!("{:<15}\t{}\n", n, if sh.o[k] { "on" } else { "off" }));
                            } else {
                                out.push_str(&format!("set {}o {}\n", if sh.o[k] { '-' } else { '+' }, n));
                            }
                        }
                        sh.out(1, out.as_bytes())?;
                    }
                    Some(name) => match set_opt_index(name) {
                        Some(k) => {
                            sh.o[k] = on;
                            if k == O_VI && on {
                                sh.o[O_EMACS] = false;
                            }
                            if k == O_EMACS && on {
                                sh.o[O_VI] = false;
                            }
                        }
                        None => {
                            sh.eprint(&format!("set: {}: invalid option name", name));
                            return Ok(2);
                        }
                    },
                }
                continue;
            }
            match set_opt_by_letter(c) {
                Some(k) => sh.o[k] = on,
                None => {
                    if c == 'i' || c == 's' || c == 'c' || c == 'l' {
                        continue;
                    }
                    sh.eprint(&format!("set: {}{}: invalid option", if on { '-' } else { '+' }, c));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    Ok(0)
}

fn shopt(sh: &mut Shell, args: &[String]) -> R {
    let mut set: Option<bool> = None;
    let mut quiet = false;
    let mut print = false;
    let mut oflag = false;
    let mut names = Vec::new();
    let mut opts_done = false;
    for a in args {
        if !opts_done && a.starts_with('-') && a.len() > 1 {
            if a == "--" {
                opts_done = true;
                continue;
            }
            for c in a[1..].chars() {
                match c {
                    's' => set = Some(true),
                    'u' => set = Some(false),
                    'q' => quiet = true,
                    'p' => print = true,
                    'o' => oflag = true,
                    _ => {
                        sh.eprint(&format!("shopt: -{}: invalid option", c));
                        return Ok(2);
                    }
                }
            }
        } else {
            opts_done = true;
            names.push(a.clone());
        }
    }
    let table: Vec<(String, bool)> = if oflag {
        SET_OPTIONS.iter().enumerate().map(|(i, (n, _))| (n.to_string(), sh.o[i])).collect()
    } else {
        SHOPTS.iter().enumerate().map(|(i, (n, _))| (n.to_string(), sh.sh[i])).collect()
    };
    if let Some(v) = set {
        let mut st = 0;
        for n in &names {
            let idx = if oflag { set_opt_index(n) } else { shopt_index(n) };
            match idx {
                Some(i) => {
                    if oflag {
                        sh.o[i] = v
                    } else if i != sho::login_shell() && i != sho::restricted_shell() {
                        // Read-only: as in bash, setting them succeeds and changes nothing.
                        sh.sh[i] = v
                    }
                }
                None => {
                    sh.eprint(&format!("shopt: {}: invalid shell option name", n));
                    st = 1;
                }
            }
        }
        if names.is_empty() {
            let mut out = String::new();
            for (n, on) in &table {
                if *on == v {
                    if print {
                        out.push_str(&format!("shopt {} {}\n", if *on { "-s" } else { "-u" }, n));
                    } else {
                        out.push_str(&format!("{:<15}\t{}\n", n, if *on { "on" } else { "off" }));
                    }
                }
            }
            sh.out(1, out.as_bytes())?;
        }
        return Ok(st);
    }
    let mut out = String::new();
    let mut st = 0;
    let show: Vec<(String, bool)> = if names.is_empty() {
        table
    } else {
        let mut v = Vec::new();
        for n in &names {
            match table.iter().find(|(x, _)| x == n) {
                Some(e) => v.push(e.clone()),
                None => {
                    sh.eprint(&format!("shopt: {}: invalid shell option name", n));
                    st = 1;
                }
            }
        }
        v
    };
    for (n, on) in &show {
        if !on {
            st = if st == 0 { 1 } else { st };
        }
        if !quiet {
            if print {
                if oflag {
                    out.push_str(&format!("set {}o {}\n", if *on { '-' } else { '+' }, n));
                } else {
                    out.push_str(&format!("shopt {} {}\n", if *on { "-s" } else { "-u" }, n));
                }
            } else {
                out.push_str(&format!("{:<15}\t{}\n", n, if *on { "on" } else { "off" }));
            }
        }
    }
    if names.is_empty() {
        st = 0;
    }
    sh.out(1, out.as_bytes())?;
    Ok(st)
}

fn shift(sh: &mut Shell, args: &[String]) -> R {
    let n: usize = match args.first() {
        None => 1,
        Some(a) => match a.parse::<i64>() {
            Ok(n) if n >= 0 => n as usize,
            _ => {
                sh.eprint(&format!("shift: {}: numeric argument required", a));
                return Ok(1);
            }
        },
    };
    if n > sh.positional.len() {
        if sh.sh[sho::shift_verbose()] {
            sh.eprint(&format!("shift: {}: shift count out of range", n));
        }
        return Ok(1);
    }
    sh.positional.drain(..n);
    Ok(0)
}

// ---------------- aliases ----------------

fn alias(sh: &mut Shell, args: &[String]) -> R {
    let args: Vec<&String> = args.iter().filter(|a| *a != "-p" && *a != "--").collect();
    if args.is_empty() {
        let mut v: Vec<(&String, &String)> = sh.aliases.iter().collect();
        v.sort();
        let out: String = v.iter().map(|(k, val)| format!("alias {}={}\n", k, shell_quote_alias(val))).collect();
        sh.out(1, out.as_bytes())?;
        return Ok(0);
    }
    let mut st = 0;
    for a in args {
        match a.find('=') {
            Some(i) => {
                let (k, v) = (&a[..i], &a[i + 1..]);
                Arc::make_mut(&mut sh.aliases).insert(k.to_string(), v.to_string());
            }
            None => match sh.aliases.get(a.as_str()).cloned() {
                Some(v) => {
                    sh.out(1, format!("alias {}={}\n", a, shell_quote_alias(&v)).as_bytes())?;
                }
                None => {
                    sh.eprint(&format!("alias: {}: not found", a));
                    st = 1;
                }
            },
        }
    }
    Ok(st)
}

fn shell_quote_alias(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\\''"))
}

fn unalias(sh: &mut Shell, args: &[String]) -> R {
    let mut st = 0;
    for a in args {
        if a == "-a" {
            Arc::make_mut(&mut sh.aliases).clear();
            continue;
        }
        if Arc::make_mut(&mut sh.aliases).remove(a.as_str()).is_none() {
            sh.eprint(&format!("unalias: {}: not found", a));
            st = 1;
        }
    }
    Ok(st)
}

// ---------------- type / command / hash ----------------

fn describe(sh: &mut Shell, name: &str, mode: char, all: bool, path_only: bool) -> Option<String> {
    // mode: 'V' verbose (type), 't' type word, 'v' command -v, 'p' path
    let mut out = String::new();
    let mut found = false;
    if !path_only {
        if let Some(a) = sh.aliases.get(name).cloned() {
            found = true;
            match mode {
                't' => out.push_str("alias\n"),
                'v' => out.push_str(&format!("alias {}={}\n", name, shell_quote_alias(&a))),
                'p' => {}
                _ => out.push_str(&format!("{} is aliased to `{}'\n", name, a)),
            }
            if !all {
                return Some(out);
            }
        }
        if KEYWORDS.contains(&name) {
            found = true;
            match mode {
                't' => out.push_str("keyword\n"),
                'v' => out.push_str(&format!("{}\n", name)),
                'p' => {}
                _ => out.push_str(&format!("{} is a shell keyword\n", name)),
            }
            if !all {
                return Some(out);
            }
        }
        if let Some(f) = sh.funcs.get(name).cloned() {
            found = true;
            match mode {
                't' => out.push_str("function\n"),
                'v' => out.push_str(&format!("{}\n", name)),
                'p' => {}
                _ => out.push_str(&format!("{} is a function\n{}\n", name, format_function(&f))),
            }
            if !all {
                return Some(out);
            }
        }
        if is_builtin(name) && !sh.disabled_builtins.contains(name) {
            found = true;
            match mode {
                't' => out.push_str("builtin\n"),
                'v' => out.push_str(&format!("{}\n", name)),
                'p' => {}
                _ => out.push_str(&format!("{} is a shell builtin\n", name)),
            }
            if !all {
                return Some(out);
            }
        }
    }
    let was_hashed = sh.hash.contains_key(name);
    if let Some(r) = sh.find_command(name) {
        if !was_hashed {
            sh.hash.remove(name);
        }
        found = true;
        let mut p = path::to_posix(&r.path);
        // bash shows paths without .exe
        let lower = p.to_lowercase();
        if lower.ends_with(".exe") && !name.to_lowercase().ends_with(".exe") {
            p.truncate(p.len() - 4);
        }
        match mode {
            't' => out.push_str("file\n"),
            'v' | 'p' => out.push_str(&format!("{}\n", p)),
            _ => {
                if was_hashed && !all {
                    out.push_str(&format!("{} is hashed ({})\n", name, p));
                } else {
                    out.push_str(&format!("{} is {}\n", name, p));
                }
            }
        }
    }
    if found {
        Some(out)
    } else {
        None
    }
}

pub fn format_function(f: &crate::ast::FuncDef) -> String {
    let src = f.source.trim();
    let body = if let Some(rest) = src.strip_prefix("function") {
        let rest = rest.trim_start();
        let rest = rest.strip_prefix(f.name.as_str()).unwrap_or(rest).trim_start();
        let rest = rest.strip_prefix("()").unwrap_or(rest).trim_start();
        rest.to_string()
    } else {
        let rest = src.strip_prefix(f.name.as_str()).unwrap_or(src).trim_start();
        let rest = rest.strip_prefix('(').unwrap_or(rest).trim_start();
        let rest = rest.strip_prefix(')').unwrap_or(rest).trim_start();
        rest.to_string()
    };
    format!("{} () \n{}", f.name, body)
}

fn type_(sh: &mut Shell, args: &[String]) -> R {
    let mut mode = 'V';
    let mut all = false;
    let mut path_only = false;
    let mut names = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 && names.is_empty() {
            for c in a[1..].chars() {
                match c {
                    't' => mode = 't',
                    'p' => mode = 'p',
                    'P' => {
                        mode = 'p';
                        path_only = true
                    }
                    'a' => all = true,
                    'f' => {}
                    _ => {}
                }
            }
        } else {
            names.push(a.clone());
        }
    }
    let mut st = 0;
    for n in names {
        match describe(sh, &n, mode, all, path_only) {
            Some(s) => sh.out(1, s.as_bytes())?,
            None => {
                if mode == 'V' {
                    sh.eprint(&format!("type: {}: not found", n));
                }
                st = 1;
            }
        }
    }
    Ok(st)
}

fn command(sh: &mut Shell, args: &[String]) -> R {
    let mut i = 0;
    let mut mode = None;
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        if args[i] == "--" {
            i += 1;
            break;
        }
        for c in args[i][1..].chars() {
            match c {
                'v' => mode = Some('v'),
                'V' => mode = Some('V'),
                'p' => {}
                _ => {
                    sh.eprint(&format!("command: -{}: invalid option", c));
                    return Ok(2);
                }
            }
        }
        i += 1;
    }
    let rest = &args[i..];
    if rest.is_empty() {
        return Ok(0);
    }
    if let Some(m) = mode {
        let mut st = 0;
        for n in rest {
            match describe(sh, n, m, false, false) {
                Some(s) => sh.out(1, s.as_bytes())?,
                None => {
                    if m == 'V' {
                        sh.eprint(&format!("command: {}: not found", n));
                    }
                    st = 1;
                }
            }
        }
        return Ok(st);
    }
    run_argv(sh, rest, true)
}

fn builtin(sh: &mut Shell, args: &[String]) -> R {
    if args.is_empty() {
        return Ok(0);
    }
    if !is_builtin(&args[0]) {
        sh.eprint(&format!("builtin: {}: not a shell builtin", args[0]));
        return Ok(1);
    }
    run(sh, args)
}

fn hash(sh: &mut Shell, args: &[String]) -> R {
    if args.is_empty() {
        if sh.hash.is_empty() {
            sh.out(1, b"hash: hash table empty\n")?;
            return Ok(0);
        }
        let mut out = String::from("hits\tcommand\n");
        let mut v: Vec<(String, String)> = sh.hash.iter().map(|(k, r)| (k.clone(), path::to_posix(&r.path))).collect();
        v.sort();
        for (_, p) in v {
            out.push_str(&format!("   1\t{}\n", p));
        }
        sh.out(1, out.as_bytes())?;
        return Ok(0);
    }
    let mut st = 0;
    let mut i = 0;
    let mut print_t = false;
    let mut delete = false;
    while i < args.len() && args[i].starts_with('-') {
        match args[i].as_str() {
            "-r" => sh.hash.clear(),
            "-t" => print_t = true,
            "-d" => delete = true,
            "-l" => {}
            "-p" => {
                if i + 2 < args.len() + 1 && i + 2 <= args.len() - 1 {
                    let p = args[i + 1].clone();
                    let n = args[i + 2].clone();
                    let wp = path::to_win(&p, &sh.cwd);
                    if let Some(r) = sh.classify_file(&wp, false) {
                        sh.hash.insert(n, r);
                    }
                    return Ok(0);
                }
            }
            _ => {}
        }
        i += 1;
    }
    for n in &args[i..] {
        if delete {
            sh.hash.remove(n);
            continue;
        }
        if print_t {
            match sh.find_command(n) {
                Some(r) => {
                    let p = path::to_posix(&r.path);
                    sh.out(1, format!("{}\n", p).as_bytes())?;
                }
                None => {
                    sh.eprint(&format!("hash: {}: not found", n));
                    st = 1;
                }
            }
            continue;
        }
        if is_builtin(n) {
            continue;
        }
        if sh.find_command(n).is_none() {
            sh.eprint(&format!("hash: {}: not found", n));
            st = 1;
        }
    }
    Ok(st)
}

// ---------------- traps ----------------

pub fn norm_sig(s: &str) -> Option<String> {
    let u = s.to_uppercase();
    let u = u.strip_prefix("SIG").unwrap_or(&u).to_string();
    let named = match u.as_str() {
        "0" | "EXIT" => "EXIT",
        "1" | "HUP" => "HUP",
        "2" | "INT" => "INT",
        "3" | "QUIT" => "QUIT",
        "6" | "ABRT" => "ABRT",
        "9" | "KILL" => "KILL",
        "13" | "PIPE" => "PIPE",
        "14" | "ALRM" => "ALRM",
        "15" | "TERM" => "TERM",
        "10" | "USR1" => "USR1",
        "12" | "USR2" => "USR2",
        "17" | "CHLD" => "CHLD",
        "28" | "WINCH" => "WINCH",
        "ERR" => "ERR",
        "DEBUG" => "DEBUG",
        "RETURN" => "RETURN",
        _ => return None,
    };
    Some(named.to_string())
}

pub fn sig_num(s: &str) -> Option<i32> {
    Some(match norm_sig(s)?.as_str() {
        "EXIT" => 0,
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ABRT" => 6,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        "CHLD" => 17,
        "WINCH" => 28,
        _ => return None,
    })
}

fn trap(sh: &mut Shell, args: &[String]) -> R {
    let args: Vec<String> = args.iter().filter(|a| *a != "--").cloned().collect();
    if args.is_empty() || args[0] == "-p" {
        let filter: Vec<String> = args.iter().skip(1).filter_map(|s| norm_sig(s)).collect();
        let shown = sh.trap_display.clone().unwrap_or_else(|| sh.traps.clone());
        let mut v: Vec<(String, String)> = shown.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        v.sort();
        let mut out = String::new();
        for (k, a) in v {
            if !filter.is_empty() && !filter.contains(&k) {
                continue;
            }
            let disp = if k == "EXIT" || k == "ERR" || k == "DEBUG" || k == "RETURN" { k.clone() } else { format!("SIG{}", k) };
            out.push_str(&format!("trap -- {} {}\n", shell_quote_alias(&a), disp));
        }
        sh.out(1, out.as_bytes())?;
        return Ok(0);
    }
    if args[0] == "-l" {
        sh.out(1, b" 1) SIGHUP\t 2) SIGINT\t 3) SIGQUIT\t 6) SIGABRT\t 9) SIGKILL\n10) SIGUSR1\t12) SIGUSR2\t13) SIGPIPE\t14) SIGALRM\t15) SIGTERM\n")?;
        return Ok(0);
    }
    let (action, sigs): (Option<String>, &[String]) = if args.len() == 1 || (args[0].parse::<u32>().is_ok() && norm_sig(&args[0]).is_some()) {
        if args.len() == 1 {
            (None, &args[..])
        } else {
            (None, &args[..])
        }
    } else if args[0] == "-" {
        (None, &args[1..])
    } else {
        (Some(args[0].clone()), &args[1..])
    };
    let mut st = 0;
    for s in sigs {
        match norm_sig(s) {
            Some(n) => sh.set_trap(&n, action.clone()),
            None => {
                sh.eprint(&format!("trap: {}: invalid signal specification", s));
                st = 1;
            }
        }
    }
    Ok(st)
}

// ---------------- jobs ----------------

fn find_job(sh: &mut Shell, spec: &str) -> Option<usize> {
    if let Some(s) = spec.strip_prefix('%') {
        let id = match s {
            "" | "%" | "+" => sh.jobs.list.iter().map(|j| j.id).max(),
            "-" => {
                let mut ids: Vec<usize> = sh.jobs.list.iter().map(|j| j.id).collect();
                ids.sort();
                ids.iter().rev().nth(1).copied().or(ids.last().copied())
            }
            n => match n.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => sh.jobs.list.iter().find(|j| j.text.starts_with(n)).map(|j| j.id),
            },
        }?;
        return sh.jobs.list.iter().position(|j| j.id == id);
    }
    let pid: u32 = spec.parse().ok()?;
    sh.jobs.list.iter().position(|j| j.pid == pid)
}

fn wait(sh: &mut Shell, args: &[String]) -> R {
    let mut next = false;
    let mut specs = Vec::new();
    for a in args {
        match a.as_str() {
            "-n" => next = true,
            "-f" => {}
            _ => specs.push(a.clone()),
        }
    }
    if next {
        if sh.jobs.list.is_empty() {
            return Ok(127);
        }
        loop {
            sh.check_int()?;
            for i in 0..sh.jobs.list.len() {
                if let Some(st) = sh.jobs.list[i].poll() {
                    sh.jobs.list.remove(i);
                    return Ok(st);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    if specs.is_empty() {
        let mut st = 0;
        while !sh.jobs.list.is_empty() {
            let mut j = sh.jobs.list.remove(0);
            loop {
                if let Some(s) = j.poll() {
                    st = s;
                    break;
                }
                if crate::sys::interrupted() {
                    sh.jobs.list.insert(0, j);
                    return Err(Flow::Fatal(130));
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        let _ = st;
        return Ok(0);
    }
    let mut st = 0;
    for s in specs {
        match find_job(sh, &s) {
            Some(i) => {
                let mut j = sh.jobs.list.remove(i);
                st = j.wait();
            }
            None => {
                if let Ok(pid) = s.parse::<u32>() {
                    if crate::sys::pid_alive(pid) {
                        while crate::sys::pid_alive(pid) {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        st = 0;
                        continue;
                    }
                }
                sh.eprint(&format!("wait: {}: no such job", s));
                st = 127;
            }
        }
    }
    Ok(st)
}

pub fn job_state(j: &mut Job) -> String {
    match j.poll() {
        None => "Running".into(),
        Some(0) => "Done".into(),
        Some(s) => format!("Exit {}", s),
    }
}

fn jobs(sh: &mut Shell, args: &[String]) -> R {
    let pids_only = args.iter().any(|a| a == "-p");
    let long = args.iter().any(|a| a == "-l");
    let n = sh.jobs.list.len();
    let mut out = String::new();
    for (k, j) in sh.jobs.list.iter_mut().enumerate() {
        let state = job_state(j);
        let mark = if k + 1 == n { '+' } else if k + 2 == n { '-' } else { ' ' };
        if pids_only {
            out.push_str(&format!("{}\n", j.pid));
        } else if long {
            out.push_str(&format!("[{}]{} {} {:<24}{}\n", j.id, mark, j.pid, state, j.text));
        } else {
            out.push_str(&format!("[{}]{}  {:<24}{} &\n", j.id, mark, state, j.text));
        }
    }
    sh.jobs.list.retain_mut(|j| j.status.is_none());
    sh.out(1, out.as_bytes())?;
    Ok(0)
}

fn fg(sh: &mut Shell, args: &[String]) -> R {
    let spec = args.first().cloned().unwrap_or_else(|| "%+".into());
    match find_job(sh, &spec) {
        Some(i) => {
            let mut j = sh.jobs.list.remove(i);
            sh.write_fd(1, format!("{}\n", j.text).as_bytes());
            Ok(j.wait())
        }
        None => {
            sh.eprint(&format!("fg: {}: no such job", spec));
            Ok(1)
        }
    }
}

fn disown(sh: &mut Shell, args: &[String]) -> R {
    if args.is_empty() || args.iter().any(|a| a == "-a") {
        sh.jobs.list.clear();
        return Ok(0);
    }
    for a in args {
        if let Some(i) = find_job(sh, a) {
            sh.jobs.list.remove(i);
        }
    }
    Ok(0)
}

fn kill(sh: &mut Shell, args: &[String]) -> R {
    let mut sig = 15;
    let mut i = 0;
    if args.first().map(|a| a == "-l" || a == "-L").unwrap_or(false) {
        if let Some(n) = args.get(1) {
            if let Ok(num) = n.parse::<i32>() {
                let num = if num > 128 { num - 128 } else { num };
                for s in ["HUP", "INT", "QUIT", "ABRT", "KILL", "USR1", "USR2", "PIPE", "ALRM", "TERM"] {
                    if sig_num(s) == Some(num) {
                        sh.out(1, format!("{}\n", s).as_bytes())?;
                    }
                }
                return Ok(0);
            }
        }
        sh.out(1, b"HUP INT QUIT ABRT KILL USR1 USR2 PIPE ALRM TERM\n")?;
        return Ok(0);
    }
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        let a = &args[i];
        if a == "-s" || a == "-n" {
            i += 1;
            sig = args.get(i).and_then(|s| sig_num(s).or_else(|| s.parse().ok())).unwrap_or(15);
        } else if a == "--" {
            i += 1;
            break;
        } else {
            sig = sig_num(&a[1..]).or_else(|| a[1..].parse().ok()).unwrap_or(15);
        }
        i += 1;
    }
    let mut st = 0;
    for t in &args[i..] {
        if let Some(k) = find_job(sh, t) {
            if sig != 0 {
                sh.jobs.list[k].kill(sig);
            }
            continue;
        }
        match t.parse::<u32>() {
            Ok(pid) if pid == sh.bashpid || pid == sh.pid => {
                if sig == 0 {
                    continue;
                }
                let name = ["HUP", "INT", "QUIT", "ABRT", "KILL", "USR1", "USR2", "PIPE", "ALRM", "TERM", "CHLD", "WINCH"]
                    .iter()
                    .find(|n| sig_num(n) == Some(sig))
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                match sh.traps.get(&name).cloned() {
                    Some(a) if a.is_empty() => {}
                    Some(_) => sh.run_trap(&name)?,
                    None => {
                        if name != "CHLD" && name != "WINCH" {
                            return Err(Flow::Exit(128 + sig));
                        }
                    }
                }
            }
            Ok(pid) => {
                if sig == 0 {
                    if !crate::sys::pid_alive(pid) {
                        st = 1;
                    }
                } else if !crate::sys::kill_pid(pid, 128 + sig as u32) {
                    sh.eprint(&format!("kill: ({}) - No such process", pid));
                    st = 1;
                }
            }
            Err(_) => {
                sh.eprint(&format!("kill: {}: arguments must be process or job IDs", t));
                st = 1;
            }
        }
    }
    Ok(st)
}

// ---------------- misc ----------------

fn umask(sh: &mut Shell, args: &[String]) -> R {
    let sym = args.iter().any(|a| a == "-S");
    let val = args.iter().find(|a| !a.starts_with('-'));
    match val {
        Some(v) => {
            if let Ok(m) = u32::from_str_radix(v, 8) {
                sh.umask = m;
            }
        }
        None => {
            if sym {
                let m = !sh.umask & 0o777;
                let f = |b: u32| {
                    let mut s = String::new();
                    if b & 4 != 0 {
                        s.push('r')
                    }
                    if b & 2 != 0 {
                        s.push('w')
                    }
                    if b & 1 != 0 {
                        s.push('x')
                    }
                    s
                };
                sh.out(1, format!("u={},g={},o={}\n", f(m >> 6), f((m >> 3) & 7), f(m & 7)).as_bytes())?;
            } else {
                sh.out(1, format!("{:04o}\n", sh.umask).as_bytes())?;
            }
        }
    }
    Ok(0)
}

fn times(sh: &mut Shell) -> R {
    let (u, s) = crate::sys::process_times();
    let f = |t: f64| format!("{}m{:.3}s", (t / 60.0) as u64, t % 60.0);
    sh.out(1, format!("{} {}\n0m0.000s 0m0.000s\n", f(u), f(s)).as_bytes())?;
    Ok(0)
}

fn ulimit(sh: &mut Shell, args: &[String]) -> R {
    if args.iter().all(|a| a.starts_with('-')) {
        if args.iter().any(|a| a.contains('a')) {
            sh.out(1, b"core file size              (blocks, -c) unlimited\nfile size                   (blocks, -f) unlimited\nopen files                          (-n) unlimited\nstack size                  (kbytes, -s) unlimited\n")?;
        } else if args.iter().any(|a| a.contains('n')) {
            sh.out(1, b"3200\n")?;
        } else {
            sh.out(1, b"unlimited\n")?;
        }
    }
    Ok(0)
}

fn enable(sh: &mut Shell, args: &[String]) -> R {
    let disable = args.iter().any(|a| a == "-n");
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    if names.is_empty() {
        let mut out = String::new();
        for b in BUILTINS {
            if !sh.disabled_builtins.contains(*b) {
                out.push_str(&format!("enable {}\n", b));
            }
        }
        sh.out(1, out.as_bytes())?;
        return Ok(0);
    }
    for n in names {
        let set = Arc::make_mut(&mut sh.disabled_builtins);
        if disable {
            set.insert(n.clone());
        } else {
            set.remove(n.as_str());
        }
    }
    Ok(0)
}

fn help(sh: &mut Shell, _args: &[String]) -> R {
    let mut out = format!("gash {} - a fast native bash-compatible shell for Windows\nBuiltins:\n", env!("CARGO_PKG_VERSION"));
    for chunk in BUILTINS.chunks(8) {
        out.push_str("  ");
        out.push_str(&chunk.join(" "));
        out.push('\n');
    }
    sh.out(1, out.as_bytes())?;
    Ok(0)
}

fn history(sh: &mut Shell, args: &[String]) -> R {
    if args.first().map(|a| a == "-c").unwrap_or(false) {
        sh.history.lock().unwrap().clear();
        return Ok(0);
    }
    if args.first().map(|a| a == "-d").unwrap_or(false) {
        if let Some(n) = args.get(1).and_then(|s| s.parse::<usize>().ok()) {
            let mut h = sh.history.lock().unwrap();
            if n >= 1 && n <= h.len() {
                h.remove(n - 1);
            }
        }
        return Ok(0);
    }
    if args.first().map(|a| a == "-s").unwrap_or(false) {
        sh.history.lock().unwrap().push(args[1..].join(" "));
        return Ok(0);
    }
    if args.first().map(|a| a.starts_with('-')).unwrap_or(false) {
        return Ok(0);
    }
    let h = sh.history.lock().unwrap().clone();
    let n = args.first().and_then(|s| s.parse::<usize>().ok()).unwrap_or(h.len());
    let start = h.len().saturating_sub(n);
    let mut out = String::new();
    for (i, l) in h.iter().enumerate().skip(start) {
        out.push_str(&format!("{:5}  {}\n", i + 1, l));
    }
    sh.out(1, out.as_bytes())?;
    Ok(0)
}

fn caller(sh: &mut Shell, args: &[String]) -> R {
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let len = sh.call_stack.len();
    if n >= len {
        return Ok(1);
    }
    let f = &sh.call_stack[len - 1 - n];
    let funcname = if len - 1 - n == 0 { "main".to_string() } else { sh.call_stack[len - 2 - n].func.clone() };
    let line = if args.is_empty() { format!("{} {}\n", f.call_line, f.source) } else { format!("{} {} {}\n", f.call_line, funcname, f.source) };
    sh.out(1, line.as_bytes())?;
    Ok(0)
}

fn compgen(sh: &mut Shell, args: &[String]) -> R {
    let mut kinds: Vec<char> = Vec::new();
    let mut words: Option<String> = None;
    let mut prefix = String::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-W" {
            i += 1;
            words = args.get(i).cloned();
        } else if a == "-A" {
            i += 1;
            let k = match args.get(i).map(|s| s.as_str()) {
                Some("function") => 'F',
                Some("command") => 'c',
                Some("file") => 'f',
                Some("directory") => 'd',
                Some("variable") => 'v',
                Some("builtin") => 'b',
                Some("alias") => 'a',
                Some("keyword") => 'k',
                Some("export") => 'e',
                _ => ' ',
            };
            kinds.push(k);
        } else if a.starts_with('-') && a.len() > 1 && a != "--" {
            kinds.extend(a[1..].chars());
        } else if a != "--" {
            prefix = a.clone();
        }
        i += 1;
    }
    let mut res: Vec<String> = Vec::new();
    for k in kinds {
        match k {
            'a' => res.extend(sh.aliases.keys().cloned()),
            'b' => res.extend(BUILTINS.iter().map(|s| s.to_string())),
            'k' => res.extend(KEYWORDS.iter().map(|s| s.to_string())),
            'F' => res.extend(sh.funcs.keys().cloned()),
            'v' => res.extend(sh.vars.names()),
            'e' => res.extend(sh.vars.visible().into_iter().filter(|(_, v)| v.has(EXPORT)).map(|(n, _)| n)),
            'c' => res.extend(crate::interactive::command_names(sh, &prefix)),
            'f' | 'd' => {
                let dirs_only = k == 'd';
                for c in crate::interactive::file_candidates(sh, &prefix, dirs_only) {
                    res.push(c);
                }
            }
            _ => {}
        }
    }
    if let Some(w) = words {
        let ifs = sh.ifs();
        res.extend(w.split(|c: char| ifs.contains(c)).filter(|s| !s.is_empty()).map(|s| s.to_string()));
    }
    let mut res: Vec<String> = res.into_iter().filter(|s| s.starts_with(&prefix)).collect();
    res.sort();
    res.dedup();
    let out: String = res.iter().map(|s| format!("{}\n", s)).collect();
    sh.out(1, out.as_bytes())?;
    Ok(if res.is_empty() { 1 } else { 0 })
}

// ---------------- git prompt ----------------

pub fn git_branch(sh: &Shell) -> Option<String> {
    let mut dir = sh.cwd.clone();
    loop {
        let g = dir.join(".git");
        if g.exists() {
            let gitdir = if g.is_file() {
                let s = std::fs::read_to_string(&g).ok()?;
                let p = s.trim().strip_prefix("gitdir:")?.trim().to_string();
                let pp = std::path::PathBuf::from(&p);
                if pp.is_absolute() || path::is_windows_abs(&p) {
                    path::to_win(&p, &dir)
                } else {
                    dir.join(pp)
                }
            } else {
                g
            };
            let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
            let head = head.trim();
            let mut state = String::new();
            if gitdir.join("rebase-merge").exists() || gitdir.join("rebase-apply").exists() {
                state = "|REBASE".into();
            } else if gitdir.join("MERGE_HEAD").exists() {
                state = "|MERGING".into();
            } else if gitdir.join("CHERRY_PICK_HEAD").exists() {
                state = "|CHERRY-PICKING".into();
            } else if gitdir.join("BISECT_LOG").exists() {
                state = "|BISECTING".into();
            }
            if let Some(r) = head.strip_prefix("ref:") {
                let r = r.trim();
                let b = r.strip_prefix("refs/heads/").unwrap_or(r);
                return Some(format!("{}{}", b, state));
            }
            if head.len() >= 7 {
                return Some(format!("({}...){}", &head[..7], state));
            }
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn git_ps1(sh: &mut Shell, args: &[String]) -> R {
    let st = sh.status;
    if let Some(b) = git_branch(sh) {
        let fmt = args.first().cloned().unwrap_or_else(|| " (%s)".into());
        let s = fmt.replacen("%s", &b, 1);
        sh.out(1, s.as_bytes())?;
    }
    Ok(st)
}
