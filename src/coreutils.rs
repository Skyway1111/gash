//! Optional use of Microsoft's native "Coreutils for Windows" (github.com/microsoft/coreutils)
//! in place of Git for Windows' MSYS2 coreutils.
//!
//! When a command name resolves (via PATH) to one of Git's MSYS tools in `/usr/bin` and
//! Microsoft Coreutils provides the same utility, gash runs the native one instead. Because
//! the caller asked for an MSYS tool, gash translates MSYS paths in the arguments itself
//! (regardless of `MSYS_NO_PATHCONV`), using a per-utility policy so that strings such as
//! `tr` sets or `grep` patterns are never mistaken for paths.
//!
//! Located via `GASH_COREUTILS` (a path to coreutils.exe or its folder; `0` disables), else
//! `<gash.exe folder>\coreutils\coreutils.exe`, else `%ProgramFiles%\coreutils\coreutils.exe`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArgPolicy {
    /// Operands and `--opt=/path` values that look like MSYS paths are converted.
    Files,
    /// Arguments are plain strings: never converted.
    Strings,
    /// grep: the pattern operand (and `-e`) is a string; files (and `-f`) are converted.
    Grep,
}

pub struct Coreutils {
    pub exe: PathBuf,
    /// Folder of per-utility hard links (`cat.exe` -> coreutils.exe).
    pub links: PathBuf,
}

static CU: OnceLock<Option<Coreutils>> = OnceLock::new();

pub fn get() -> Option<&'static Coreutils> {
    CU.get_or_init(detect).as_ref()
}

fn detect() -> Option<Coreutils> {
    let setting = std::env::var("GASH_COREUTILS").unwrap_or_default();
    match setting.to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" => return None,
        "" | "1" | "on" | "true" | "yes" => {}
        _ => {
            let p = PathBuf::from(&setting);
            let exe = if p.is_dir() { p.join("coreutils.exe") } else { p };
            return if exe.is_file() { Some(make(exe)) } else { None };
        }
    }
    let mut cands = Vec::new();
    if let Ok(me) = std::env::current_exe() {
        if let Some(d) = me.parent() {
            cands.push(d.join("coreutils").join("coreutils.exe"));
        }
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        cands.push(PathBuf::from(pf).join("coreutils").join("coreutils.exe"));
    }
    cands.into_iter().find(|p| p.is_file()).map(make)
}

fn make(exe: PathBuf) -> Coreutils {
    let links = exe.parent().map(|d| d.join("bin")).unwrap_or_default();
    Coreutils { exe, links }
}

/// Utilities that gash routes to Microsoft Coreutils, with their argument policy.
///
/// Kept on Git's GNU versions: `sort`, `find` (ambiguous with DOS commands), `mktemp`,
/// `realpath`, `readlink` (print Windows paths), `ln` (native symlinks need Developer Mode),
/// `env`, `xargs` (can't start shell scripts); `ls` (lists dotfiles, hides files with the
/// hidden attribute, shows every mode as rwx), `stat` (same modes), `du` (`\` in paths),
/// `touch` (takes `-d` times as UTC), `comm`, `join` (expect a different order than GNU
/// `sort`), `md5sum` and the other checksums (no `*` binary marker), `fmt`, `pr`, `ptx`, `sum`
/// (other layouts), `rm` (can't remove junctions or directory symlinks), `cp` (`-r` makes
/// native links where the runtime copies their targets; symlinks need Developer Mode);
/// recursive `grep` (`\` in paths, see `grep_recursive`); and gash builtins.
const UTILS: &[(&str, ArgPolicy)] = {
    use ArgPolicy::*;
    &[
        ("grep", Grep),
        ("basename", Strings), ("dirname", Strings), ("expr", Strings), ("seq", Strings), ("tr", Strings),
        ("yes", Strings), ("factor", Strings), ("numfmt", Strings), ("sleep", Strings), ("printenv", Strings),
        ("pathchk", Strings), ("arch", Strings), ("nproc", Strings), ("hostname", Strings), ("uptime", Strings),
        ("date", Strings),
        ("base32", Files), ("base64", Files), ("basenc", Files), ("cat", Files), ("cksum", Files),
        ("csplit", Files), ("cut", Files), ("df", Files), ("fold", Files), ("head", Files), ("link", Files),
        ("mkdir", Files), ("mv", Files), ("nl", Files), ("od", Files), ("paste", Files),
        ("rmdir", Files), ("shuf", Files), ("split", Files), ("tac", Files), ("tail", Files), ("tee", Files),
        ("truncate", Files), ("tsort", Files), ("unexpand", Files), ("uniq", Files), ("unlink", Files), ("wc", Files),
    ]
};

/// A routed utility's `'static` name and argument policy.
pub fn util(name: &str) -> Option<(&'static str, ArgPolicy)> {
    UTILS.iter().find(|(n, _)| *n == name).copied()
}

/// Must Git's GNU `util` run these arguments instead? Recursive grep prints `\` paths, and
/// paths only the MSYS runtime provides (`/dev/...`, `/proc/...`; native tools can't read `nul`)
/// mean nothing to a native program.
pub fn keep_gnu(util: &str, args: &[String]) -> bool {
    let msys_only = |a: &String| {
        let p = a.split_once('=').map_or(a.as_str(), |(_, v)| v);
        p.starts_with("/dev/") || p == "/proc" || p.starts_with("/proc/")
    };
    (util == "grep" && grep_recursive(args)) || args.iter().any(msys_only)
}

/// Does this grep command line search directories (`-r`, `-R`, `-d recurse`)? Options may
/// follow operands, as with GNU getopt.
fn grep_recursive(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return false;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            if name == "recursive" || name == "dereference-recursive" {
                return true;
            }
            let takes_value = GREP_LONG_WITH_VALUE.contains(&a.split('=').next().unwrap_or(a));
            let value = if takes_value && value.is_none() { it.next().cloned() } else { value };
            if name == "directories" && value.as_deref() == Some("recurse") {
                return true;
            }
        } else if a.len() > 1 && a.starts_with('-') {
            for (k, c) in a[1..].char_indices() {
                if c == 'r' || c == 'R' {
                    return true;
                }
                if GREP_SHORT_WITH_VALUE.contains(c) {
                    let rest = &a[1 + k + c.len_utf8()..];
                    let value = if rest.is_empty() { it.next().map(String::as_str) } else { Some(rest) };
                    if c == 'd' && value == Some("recurse") {
                        return true;
                    }
                    break;
                }
            }
        }
    }
    false
}

/// grep options whose value is a separate argument.
const GREP_SHORT_WITH_VALUE: &str = "efmABCdD";
const GREP_LONG_WITH_VALUE: &[&str] = &[
    "--regexp", "--file", "--max-count", "--after-context", "--before-context", "--context", "--devices",
    "--directories", "--label", "--include", "--exclude", "--exclude-dir", "--exclude-from", "--binary-files",
    "--group-separator",
];

impl Coreutils {
    /// Executable to run for `util`: a hard link named after it (created or refreshed on
    /// demand), or coreutils.exe itself if links can't be made (then pass `util` first).
    pub fn program(&self, util: &str) -> PathBuf {
        let link = self.links.join(format!("{}.exe", util));
        if same_file_data(&link, &self.exe) {
            return link;
        }
        let _ = std::fs::create_dir_all(&self.links);
        let _ = std::fs::remove_file(&link);
        if std::fs::hard_link(&self.exe, &link).is_ok() {
            return link;
        }
        self.exe.clone()
    }

    /// Is `p` one of our per-utility links? Returns the utility name.
    pub fn util_of(&self, p: &Path) -> Option<&'static str> {
        let parent = p.parent()?;
        if !parent.as_os_str().eq_ignore_ascii_case(self.links.as_os_str()) {
            return None;
        }
        let stem = p.file_stem()?.to_str()?.to_ascii_lowercase();
        util(&stem).map(|(n, _)| n)
    }
}

fn same_file_data(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.len() == y.len() && x.modified().ok() == y.modified().ok(),
        _ => false,
    }
}

/// Translate MSYS paths in `args` for a native utility according to its policy.
pub fn translate_args(policy: ArgPolicy, args: &[String]) -> Vec<String> {
    let conv = |a: &str| crate::path::convert_arg(a).into_owned();
    let conv_opt_value = |a: &str| -> String {
        // --opt=/path
        match a.find('=') {
            Some(i) if a.starts_with("--") => format!("{}{}", &a[..=i], conv(&a[i + 1..])),
            _ => a.to_string(),
        }
    };
    match policy {
        ArgPolicy::Strings => args.to_vec(),
        ArgPolicy::Files => {
            let mut out = Vec::with_capacity(args.len());
            let mut operands_only = false;
            for a in args {
                if !operands_only && a == "--" {
                    operands_only = true;
                    out.push(a.clone());
                } else if !operands_only && a.starts_with("--") {
                    out.push(conv_opt_value(a));
                } else if !operands_only && a.starts_with('-') && a.len() > 1 {
                    out.push(a.clone());
                } else {
                    out.push(conv(a));
                }
            }
            out
        }
        ArgPolicy::Grep => {
            const SHORT_WITH_VALUE: &str = GREP_SHORT_WITH_VALUE;
            const LONG_WITH_VALUE: &[&str] = GREP_LONG_WITH_VALUE;
            let mut out = Vec::with_capacity(args.len());
            let mut pattern_given = args.iter().any(|a| {
                a == "-e" || a == "-f" || a.starts_with("--regexp") || a.starts_with("--file") || {
                    // -e/-f inside a short cluster like -ie or -rf
                    a.starts_with('-') && !a.starts_with("--") && a[1..].chars().any(|c| c == 'e' || c == 'f')
                }
            });
            let mut operands_only = false;
            let mut i = 0;
            while i < args.len() {
                let a = &args[i];
                if !operands_only && a == "--" {
                    operands_only = true;
                    out.push(a.clone());
                } else if !operands_only && a.starts_with("--") {
                    let name = a.split('=').next().unwrap_or(a);
                    let is_file = name == "--file" || name == "--exclude-from";
                    if a.contains('=') {
                        out.push(if is_file { conv_opt_value(a) } else { a.clone() });
                    } else {
                        out.push(a.clone());
                        if LONG_WITH_VALUE.contains(&name) && i + 1 < args.len() {
                            i += 1;
                            out.push(if is_file { conv(&args[i]) } else { args[i].clone() });
                        }
                    }
                } else if !operands_only && a.starts_with('-') && a.len() > 1 {
                    // short cluster; the first value-taking option consumes the rest or the next arg
                    let chars: Vec<char> = a[1..].chars().collect();
                    let mut consumed_next = None;
                    for (k, c) in chars.iter().enumerate() {
                        if SHORT_WITH_VALUE.contains(*c) {
                            if k + 1 == chars.len() && i + 1 < args.len() {
                                consumed_next = Some(*c);
                            }
                            break;
                        }
                    }
                    out.push(a.clone());
                    if let Some(c) = consumed_next {
                        i += 1;
                        out.push(if c == 'f' { conv(&args[i]) } else { args[i].clone() });
                    }
                } else if !pattern_given {
                    pattern_given = true;
                    out.push(a.clone());
                } else {
                    out.push(conv(a));
                }
                i += 1;
            }
            out
        }
    }
}

/// Quote an argument with MSVCRT rules. Quoted arguments are never re-globbed by
/// programs that expand wildcards themselves (such as Microsoft Coreutils via `wild`).
pub fn msvc_quote(a: &str) -> String {
    let mut s = String::with_capacity(a.len() + 2);
    s.push('"');
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                s.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
                s.push('"');
                backslashes = 0;
            }
            c => {
                s.extend(std::iter::repeat('\\').take(backslashes));
                s.push(c);
                backslashes = 0;
            }
        }
    }
    s.extend(std::iter::repeat('\\').take(backslashes * 2));
    s.push('"');
    s
}

/// Does this argument contain characters a self-globbing native program would expand?
pub fn has_glob_chars(a: &str) -> bool {
    a.contains(['*', '?', '['])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn quoting() {
        assert_eq!(msvc_quote("a b"), "\"a b\"");
        assert_eq!(msvc_quote("*.txt"), "\"*.txt\"");
        assert_eq!(msvc_quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(msvc_quote(r"C:\dir\"), "\"C:\\dir\\\\\"");
        assert_eq!(msvc_quote(""), "\"\"");
    }

    #[test]
    fn grep_pattern_untouched() {
        crate::path::init();
        // the pattern looks like a path but must not be converted; the file must be
        let out = translate_args(ArgPolicy::Grep, &s(&["-rn", "/usr/bin", "/c/Windows/win.ini"]));
        assert_eq!(out[1], "/usr/bin");
        assert_eq!(out[2], "C:/Windows/win.ini");
        let out = translate_args(ArgPolicy::Grep, &s(&["-e", "/tmp/x", "/c/f"]));
        assert_eq!(out[1], "/tmp/x");
        assert_eq!(out[2], "C:/f");
        let out = translate_args(ArgPolicy::Grep, &s(&["-f", "/c/pats", "/c/f"]));
        assert_eq!(out[1], "C:/pats");
        assert_eq!(out[2], "C:/f");
        let out = translate_args(ArgPolicy::Grep, &s(&["-A", "2", "pat", "/c/f"]));
        assert_eq!(out, s(&["-A", "2", "pat", "C:/f"]));
    }

    #[test]
    fn files_and_strings() {
        crate::path::init();
        assert_eq!(translate_args(ArgPolicy::Files, &s(&["-n", "5", "/c/x/y"])), s(&["-n", "5", "C:/x/y"]));
        assert_eq!(translate_args(ArgPolicy::Files, &s(&["-d", "/", "rel/path"])), s(&["-d", "/", "rel/path"]));
        assert_eq!(translate_args(ArgPolicy::Strings, &s(&["/c/x"])), s(&["/c/x"]));
    }
}
