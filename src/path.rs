//! POSIX <-> Windows path translation, emulating the MSYS2/Git-for-Windows mount table:
//!   /c/foo        -> C:\foo
//!   /tmp          -> %TMP%, else %TEMP% (see `pick_temp`)
//!   /bin, /lib    -> <root>\usr\bin, <root>\usr\lib
//!   /usr/...      -> <root>\usr\...
//!   /dev/null     -> NUL

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct Mounts {
    pub root: PathBuf,
    pub temp: PathBuf,
    root_lc: String,
    temp_lc: String,
    msys_bins: Vec<String>,
}

static MOUNTS: OnceLock<Mounts> = OnceLock::new();

fn norm_lc(p: &Path) -> String {
    let mut s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    while s.ends_with('\\') && s.len() > 3 {
        s.pop();
    }
    s
}

pub fn find_msys_root() -> PathBuf {
    if let Ok(r) = std::env::var("GASH_ROOT") {
        return PathBuf::from(r);
    }
    // Next to our own executable (if installed into <root>\usr\bin or <root>\bin)
    if let Ok(exe) = std::env::current_exe() {
        let mut p = exe.parent().map(|p| p.to_path_buf());
        for _ in 0..3 {
            if let Some(d) = &p {
                if d.join("usr").join("bin").join("msys-2.0.dll").exists() {
                    return d.clone();
                }
                p = d.parent().map(|x| x.to_path_buf());
            }
        }
    }
    // git.exe on PATH: <root>\cmd\git.exe or <root>\bin\git.exe or <root>\mingw64\bin\git.exe
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(';') {
            if dir.is_empty() {
                continue;
            }
            let d = Path::new(dir);
            if d.join("git.exe").exists() {
                let mut cur = d.parent();
                while let Some(c) = cur {
                    if c.join("usr").join("bin").exists() && c.join("etc").exists() {
                        return c.to_path_buf();
                    }
                    cur = c.parent();
                }
            }
        }
    }
    for cand in [r"C:\Program Files\Git", r"C:\Program Files (x86)\Git"] {
        let p = PathBuf::from(cand);
        if p.join("usr").join("bin").exists() {
            return p;
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("Programs").join("Git");
        if p.join("usr").join("bin").exists() {
            return p;
        }
    }
    PathBuf::from(r"C:\Program Files\Git")
}

/// The folder behind `/tmp`: the one the MSYS runtime mounts there.
///
/// MSYS builds its `/tmp` mount (`usertemp` in /etc/fstab) from GetTempPathW, which takes TMP,
/// then TEMP, then USERPROFILE, then the Windows folder, and makes the value a full path. So a
/// launcher that passes `TMP=/tmp` gets `C:\tmp` (on the current drive) in every MSYS process it
/// starts, and gash must use the same folder to agree with them about what is in `/tmp` (issue
/// #27).
///
/// MSYS mounts that folder whether or not it exists, and an MSYS program that started earlier with
/// the same environment may already be using it as `/tmp`, so a missing folder is created rather
/// than replaced. Only when that fails does gash take Windows' default per-user temp folder, and
/// `<root>\tmp` as a last resort: it is read-only for most users of a Git installed under Program
/// Files. MSYS programs gash starts get TMP/TEMP set to the folder chosen here, so they agree with
/// gash unless another MSYS process already running set up `/tmp` differently.
fn pick_temp(root: &Path) -> PathBuf {
    let mut temp = crate::sys::temp_path().unwrap_or_default().into_os_string().into_string().unwrap_or_default();
    while temp.len() > 3 && temp.ends_with(['\\', '/']) {
        temp.pop();
    }
    let temp = PathBuf::from(temp);
    if temp.is_absolute() && (temp.is_dir() || std::fs::create_dir_all(&temp).is_ok()) {
        return temp;
    }
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|p| Path::new(&p).join("AppData").join("Local")));
    if let Some(local) = local {
        let p = local.join("Temp");
        if p.is_absolute() && p.is_dir() {
            return p;
        }
    }
    root.join("tmp")
}

pub fn init() {
    let root = find_msys_root();
    let temp = pick_temp(&root);
    let root_lc = norm_lc(&root);
    let temp_lc = norm_lc(&temp);
    let msys_bins = vec![norm_lc(&root.join("usr").join("bin")), norm_lc(&root.join("bin"))];
    let _ = MOUNTS.set(Mounts { root, temp, root_lc, temp_lc, msys_bins });
}

pub fn mounts() -> &'static Mounts {
    MOUNTS.get_or_init(|| {
        let root = find_msys_root();
        let temp = pick_temp(&root);
        Mounts {
            root_lc: norm_lc(&root),
            temp_lc: norm_lc(&temp),
            msys_bins: vec![norm_lc(&root.join("usr").join("bin")), norm_lc(&root.join("bin"))],
            root,
            temp,
        }
    })
}

/// `C:\x`, `C:/x`, `C:`, `\\server\share`, `//server/share`(UNC-ish only if not a drive mount)
pub fn is_windows_abs(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return true;
    }
    s.starts_with("\\\\")
}

fn drive_prefix(s: &str) -> Option<(char, &str)> {
    // "/c" or "/c/..." -> ('c', "/...")
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && (b.len() == 2 || b[2] == b'/') {
        return Some(((b[1] as char).to_ascii_uppercase(), &s[2..]));
    }
    None
}

/// Translate a (possibly relative) POSIX-style path to a Windows path.
pub fn to_win(s: &str, cwd: &Path) -> PathBuf {
    if s.is_empty() {
        return cwd.to_path_buf();
    }
    if is_windows_abs(s) {
        let t = s.replace('/', "\\");
        if t.len() == 2 {
            return PathBuf::from(format!("{}\\", t));
        }
        return PathBuf::from(t);
    }
    if let Some(rest) = s.strip_prefix('/') {
        let m = mounts();
        if s.starts_with("//") && !s.starts_with("///") {
            // UNC: //server/share
            return PathBuf::from(s.replace('/', "\\"));
        }
        if let Some((d, rest)) = drive_prefix(s) {
            return PathBuf::from(format!("{}:\\{}", d, rest.trim_start_matches('/').replace('/', "\\")));
        }
        if let Some(r) = s.strip_prefix("/cygdrive") {
            if let Some((d, rest)) = drive_prefix(r) {
                return PathBuf::from(format!("{}:\\{}", d, rest.trim_start_matches('/').replace('/', "\\")));
            }
        }
        if s == "/dev/null" {
            return PathBuf::from("NUL");
        }
        if s == "/dev/tty" {
            return PathBuf::from("CON");
        }
        let first = rest.split('/').next().unwrap_or("");
        match first {
            "tmp" => {
                let r = &rest[3..];
                return join_win(&m.temp, r);
            }
            "bin" | "lib" => {
                return join_win(&m.root.join("usr"), rest);
            }
            _ => {}
        }
        return join_win(&m.root, rest);
    }
    join_win(cwd, s)
}

fn join_win(base: &Path, rel: &str) -> PathBuf {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return base.to_path_buf();
    }
    let mut p = base.to_path_buf();
    for comp in rel.split(['/', '\\']) {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            p.pop();
            continue;
        }
        p.push(comp);
    }
    p
}

fn strip_verbatim(s: &str) -> &str {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        // keep a leading "\\" for UNC; caller handles
        return r;
    }
    s.strip_prefix(r"\\?\").unwrap_or(s)
}

/// Translate a Windows path to the POSIX form Git Bash would show.
pub fn to_posix(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let s = strip_verbatim(&raw).to_string();
    let lc = s.replace('/', "\\").to_lowercase();
    let lc_trim = lc.trim_end_matches('\\').to_string();
    let m = mounts();
    let under = |prefix: &str| -> Option<usize> {
        if lc_trim == prefix {
            return Some(prefix.len());
        }
        if lc_trim.starts_with(prefix) && lc_trim.as_bytes().get(prefix.len()) == Some(&b'\\') {
            return Some(prefix.len());
        }
        None
    };
    if let Some(n) = under(&m.temp_lc) {
        let rest = s.get(n..).unwrap_or("").replace('\\', "/");
        let rest = rest.trim_end_matches('/');
        return format!("/tmp{}", rest);
    }
    if let Some(n) = under(&m.root_lc) {
        let rest = s.get(n..).unwrap_or("").replace('\\', "/");
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return "/".into();
        }
        return rest.to_string();
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let rest = s[2..].replace('\\', "/");
        let rest = rest.trim_end_matches('/');
        return format!("/{}{}", (b[0] as char).to_ascii_lowercase(), rest);
    }
    if s.starts_with("\\\\") {
        return s.replace('\\', "/");
    }
    s.replace('\\', "/")
}

/// Normalize a POSIX path lexically: collapse `//`, remove `.`, resolve `..`.
pub fn normalize_posix(p: &str) -> String {
    let abs = p.starts_with('/');
    let unc = p.starts_with("//") && !p.starts_with("///");
    let mut out: Vec<&str> = Vec::new();
    for comp in p.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if out.last().map(|l| *l != "..").unwrap_or(false) {
                    out.pop();
                } else if !abs {
                    out.push("..");
                }
            }
            c => out.push(c),
        }
    }
    let joined = out.join("/");
    if unc {
        format!("//{}", joined)
    } else if abs {
        format!("/{}", joined)
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

/// Convert a Windows-style `;`-separated PATH to POSIX `:`-separated form.
pub fn win_list_to_posix(list: &str) -> String {
    list.split(';')
        .filter(|s| !s.is_empty())
        .map(|d| {
            let d = d.trim_matches('"');
            if is_windows_abs(d) {
                to_posix(Path::new(d))
            } else {
                d.replace('\\', "/")
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// Convert a POSIX `:`-separated list to Windows `;`-separated form.
pub fn posix_list_to_win(list: &str, cwd: &Path) -> String {
    if list.contains(';') {
        return list.to_string();
    }
    split_posix_list(list)
        .into_iter()
        .map(|d| if d.is_empty() { ".".to_string() } else { to_win(&d, cwd).to_string_lossy().into_owned() })
        .collect::<Vec<_>>()
        .join(";")
}

/// Split a POSIX path list on ':' but keep `C:/...` drive specs together.
pub fn split_posix_list(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b: Vec<char> = list.chars().collect();
    let mut cur = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == ':' {
            // drive letter like "C:" followed by / or \ ?
            if cur.len() == 1 && cur.chars().next().unwrap().is_ascii_alphabetic() && matches!(b.get(i + 1), Some('/') | Some('\\')) {
                cur.push(c);
                i += 1;
                continue;
            }
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
        i += 1;
    }
    out.push(cur);
    out
}

pub fn is_msys_binary(p: &Path) -> bool {
    let m = mounts();
    let parent = match p.parent() {
        Some(x) => norm_lc(x),
        None => return false,
    };
    m.msys_bins.iter().any(|b| *b == parent)
}

fn convertible(s: &str) -> bool {
    if !s.starts_with('/') || s.starts_with("//") {
        return false;
    }
    if drive_prefix(s).map(|(_, r)| !r.is_empty()).unwrap_or(false) {
        return true;
    }
    let rest = &s[1..];
    let first = rest.split('/').next().unwrap_or("");
    if first.is_empty() {
        return false;
    }
    match first {
        "tmp" | "usr" | "bin" | "etc" | "lib" | "dev" | "opt" | "mingw64" | "mingw32" | "ucrt64" | "clang64"
        | "clangarm64" | "cygdrive" | "cmd" | "share" => return rest.contains('/') || first != "dev",
        _ => {}
    }
    // Something under the msys root that actually exists.
    let m = mounts();
    m.root.join(first).exists()
}

/// MSYS-style argument conversion for native (non-msys) programs.
pub fn convert_arg(arg: &str) -> Cow<'_, str> {
    if arg.len() < 2 {
        return Cow::Borrowed(arg);
    }
    // `//x` is an escaped Windows switch (`cmd //c`, `taskkill //F`): as in MSYS, it loses one
    // slash, unless it looks like a UNC path (`//server/share`).
    if let Some(rest) = arg.strip_prefix("//") {
        if rest.is_empty() || rest.starts_with('/') || !rest.contains('/') {
            return Cow::Owned(arg[1..].to_string());
        }
        return Cow::Borrowed(arg);
    }
    let cwd = Path::new("C:\\");
    if convertible(arg) {
        if arg == "/dev/null" {
            return Cow::Owned("nul".into());
        }
        return Cow::Owned(win_fwd(&to_win(arg, cwd)));
    }
    // --opt=/path or VAR=/path
    if let Some(eq) = arg.find('=') {
        let (k, v) = arg.split_at(eq + 1);
        if !k.contains('/') && convertible(v) {
            return Cow::Owned(format!("{}{}", k, win_fwd(&to_win(v, cwd))));
        }
    }
    // path lists /a:/b
    if arg.starts_with('/') && arg.contains(':') && !arg.contains("://") {
        let parts = split_posix_list(arg);
        if parts.len() > 1 && parts.iter().all(|p| convertible(p)) {
            return Cow::Owned(parts.iter().map(|p| win_fwd(&to_win(p, cwd))).collect::<Vec<_>>().join(";"));
        }
    }
    Cow::Borrowed(arg)
}

pub fn win_fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Convert an environment value for a native child.
pub fn convert_env_value(name: &str, v: &str, cwd: &Path) -> String {
    match name {
        "PATH" => posix_list_to_win(v, cwd),
        "HOME" | "TMP" | "TEMP" | "TMPDIR" | "SHELL" | "OLDPWD" | "PWD" => {
            if v.starts_with('/') {
                to_win(v, cwd).to_string_lossy().into_owned()
            } else {
                v.to_string()
            }
        }
        _ => {
            if v.starts_with('/') && !v.contains(' ') && convertible(v) && !v.contains(':') {
                win_fwd(&to_win(v, cwd))
            } else {
                v.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        init();
        let cwd = Path::new(r"C:\Users\x");
        assert_eq!(to_win("/c/Users/x", cwd), PathBuf::from(r"C:\Users\x"));
        assert_eq!(to_win("/c", cwd), PathBuf::from(r"C:\"));
        assert_eq!(to_win("foo/bar", cwd), PathBuf::from(r"C:\Users\x\foo\bar"));
        assert_eq!(to_win("../y", cwd), PathBuf::from(r"C:\Users\y"));
        assert_eq!(to_posix(Path::new(r"C:\Users\x")), "/c/Users/x");
        assert_eq!(to_posix(Path::new(r"D:\")), "/d");
        assert_eq!(normalize_posix("/a/b/../c/./d//e"), "/a/c/d/e");
        assert_eq!(normalize_posix("/.."), "/");
        assert_eq!(split_posix_list("/a:/b:C:/x"), vec!["/a", "/b", "C:/x"]);
    }
}
