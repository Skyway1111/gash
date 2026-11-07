//! MSYS programs that gash starts must resolve `/tmp` to the same folder gash does.
//!
//! The MSYS runtime builds its `/tmp` mount (`usertemp` in /etc/fstab) from GetTempPathW, so from
//! the Windows TMP/TEMP of the first MSYS process of an installation, and keeps the mount table in
//! per-user shared memory for every MSYS process that starts while one is still running.
//! Against Git's own `/usr/bin` the outcome therefore depends on whether, say, a Git Bash
//! window is open. These tests copy the runtime and a few tools into a private root, which
//! MSYS treats as a separate installation, so they decide which MSYS process, if any, is
//! running when gash starts.
//!
//! Skipped when Git for Windows is not installed.

use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

const RUNTIME: &[&str] = &[
    "msys-2.0.dll",
    "msys-iconv-2.dll",
    "msys-intl-8.dll",
    "cat.exe",
    "cygpath.exe",
    "mktemp.exe",
];

const FSTAB: &str = "none / cygdrive binary,posix=0,noacl,user 0 0\n\
                     none /tmp usertemp binary,posix=0,noacl 0 0\n";

const SCRIPT: &str = r#"
echo made-by-gash > /tmp/from-gash
/usr/bin/cat /tmp/from-gash
/usr/bin/cat <(echo procsub)
/usr/bin/cat /dev/stdin <<EOF
heredoc
EOF
d=$(/usr/bin/mktemp -d) && cd "$d" && echo cd-ok
/usr/bin/cygpath -w /tmp
cd /tmp && pwd -W
cmd //d //c "echo %TMP%"
cmd //d //c "echo %TEMP%"
"#;

fn git_usr_bin() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(r) = std::env::var("GASH_ROOT") {
        roots.push(PathBuf::from(r));
    }
    if let Ok(pf) = std::env::var("ProgramFiles") {
        roots.push(Path::new(&pf).join("Git"));
    }
    roots.push(PathBuf::from(r"C:\Program Files\Git"));
    roots
        .into_iter()
        .map(|r| r.join("usr").join("bin"))
        .find(|b| RUNTIME.iter().all(|f| b.join(f).is_file()))
}

/// Removes the scratch folder even when an assertion fails.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The variables that decide where `/tmp` is, for both gash and the MSYS runtime.
struct TempEnv {
    tmp: OsString,
    temp: OsString,
    local_app_data: PathBuf,
}

/// The private MSYS root and folders for one test, removed when it ends.
struct Setup {
    scratch: Scratch,
    root: PathBuf,
}

impl Setup {
    fn new(git_bin: &Path) -> Setup {
        // Tests run in parallel in one process, so the name needs more than the process id.
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let name = format!("gash-msys-tmp-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let scratch = Scratch(std::env::temp_dir().join(name));
        let _ = fs::remove_dir_all(&scratch.0);
        let root = scratch.0.join("root");
        let bin = root.join("usr").join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(root.join("etc")).unwrap();
        for f in RUNTIME {
            fs::copy(git_bin.join(f), bin.join(f)).unwrap_or_else(|e| panic!("copy {f}: {e}"));
        }
        fs::write(root.join("etc").join("fstab"), FSTAB).unwrap();
        Setup { scratch, root }
    }

    fn dir(&self, name: &str) -> PathBuf {
        let d = self.scratch.0.join(name);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn env(&self, cmd: &mut Command, e: &TempEnv) {
        cmd.current_dir(&self.scratch.0)
            .env("TMP", &e.tmp)
            .env("TEMP", &e.temp)
            .env("LOCALAPPDATA", &e.local_app_data);
    }

    /// Runs `f` while an MSYS program of the private root is running, started with `e`, so that
    /// it, not anything gash starts, sets up the root's mount table.
    fn with_msys_running(&self, e: &TempEnv, f: impl FnOnce()) {
        let mut cmd = Command::new(self.root.join("usr").join("bin").join("cat.exe"));
        self.env(&mut cmd, e);
        let mut cat = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("start cat");
        let mut stdin = cat.stdin.take().unwrap();
        let mut stdout = BufReader::new(cat.stdout.take().unwrap());
        stdin.write_all(b"ready\n").unwrap();
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready", "the MSYS program didn't start");
        f();
        drop(stdin);
        let _ = cat.wait();
    }

    /// Runs SCRIPT with `e` and checks that gash, the MSYS programs it starts and a native
    /// program all use `expected` for `/tmp`.
    fn check(&self, e: &TempEnv, expected: &Path) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gash"));
        self.env(&mut cmd, e);
        let out = cmd
            .arg("-c")
            .arg(SCRIPT)
            .env("GASH_ROOT", &self.root)
            .env("GASH_COREUTILS", "0")
            .env_remove("MSYS_NO_PATHCONV")
            .env_remove("MSYS2_ARG_CONV_EXCL")
            .stdin(Stdio::null())
            .output()
            .expect("run gash");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let context = format!(
            "TMP={:?} TEMP={:?}\nexit: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            e.tmp,
            e.temp,
            out.status.code()
        );

        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 8, "{context}");
        assert_eq!(lines[..4], ["made-by-gash", "procsub", "heredoc", "cd-ok"], "{context}");
        // A native program can't read `/x` as gash means it, so it must get a full Windows path.
        for (line, who) in [(lines[6], "native TMP"), (lines[7], "native TEMP")] {
            let b = line.trim().as_bytes();
            let full = (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\') || line.starts_with(r"\\");
            assert!(full, "{who} is not a full Windows path: {line}\n{context}");
        }
        let expected = fs::canonicalize(expected).unwrap();
        for (line, who) in [(lines[4], "MSYS"), (lines[5], "gash"), (lines[6], "native TMP"), (lines[7], "native TEMP")] {
            let got = fs::canonicalize(line.trim()).unwrap_or_else(|e| panic!("{who}: {e}: {line}\n{context}"));
            assert_eq!(got, expected, "{who} /tmp is not the expected folder\n{context}");
        }
        assert_eq!(out.status.code(), Some(0), "{context}");
    }
}

/// `C:\a\b` as `/a/b`: the MSYS form a launcher can pass, which Windows reads on the current drive.
/// None when the scratch folder isn't on a drive letter (a UNC temp folder).
fn drive_relative(p: &Path) -> Option<OsString> {
    let s = p.to_str()?;
    let b = s.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
        eprintln!("skipping: scratch folder is not on a drive: {s}");
        return None;
    }
    Some(s[2..].replace('\\', "/").into())
}

#[test]
fn msys_children_share_gash_tmp() {
    let Some(git_bin) = git_usr_bin() else {
        eprintln!("skipping: Git for Windows' MSYS runtime not found");
        return;
    };
    let s = Setup::new(&git_bin);
    let temp = s.dir("temp");
    let local = s.dir("local");
    s.dir("local/Temp");
    let e = TempEnv { tmp: temp.clone().into(), temp: temp.clone().into(), local_app_data: local };
    s.check(&e, &temp);
}

/// As in GetTempPathW, TMP comes before TEMP.
#[test]
fn tmp_comes_before_temp() {
    let Some(git_bin) = git_usr_bin() else {
        eprintln!("skipping: Git for Windows' MSYS runtime not found");
        return;
    };
    let s = Setup::new(&git_bin);
    let tmp = s.dir("tmp-var");
    let temp = s.dir("temp-var");
    let local = s.dir("local");
    s.dir("local/Temp");
    let e = TempEnv { tmp: tmp.clone().into(), temp: temp.into(), local_app_data: local };
    s.check(&e, &tmp);
}

/// A launcher can pass TMP and TEMP in MSYS form (`/tmp`). The MSYS runtime reads that as a
/// folder on the current drive, so every MSYS process the launcher starts uses that folder for
/// `/tmp`, and gash must too (issue #27).
#[test]
fn msys_form_tmp_is_read_like_the_msys_runtime_does() {
    let Some(git_bin) = git_usr_bin() else {
        eprintln!("skipping: Git for Windows' MSYS runtime not found");
        return;
    };
    let s = Setup::new(&git_bin);
    let msys_form = s.dir("msys-form");
    let local = s.dir("local");
    s.dir("local/Temp");
    let Some(rel) = drive_relative(&msys_form) else { return };
    let e = TempEnv { tmp: rel.clone(), temp: rel, local_app_data: local };
    s.check(&e, &msys_form);
}

/// MSYS mounts the folder TMP names at `/tmp` even when it doesn't exist, so gash creates it rather
/// than use another one. Otherwise, with an MSYS program the launcher started still running, `mkdir
/// /tmp/x` in MSYS and `cd /tmp/x` in gash would use different folders (issue #27).
#[test]
fn missing_tmp_folder_is_created() {
    let Some(git_bin) = git_usr_bin() else {
        eprintln!("skipping: Git for Windows' MSYS runtime not found");
        return;
    };
    let s = Setup::new(&git_bin);
    let missing = s.scratch.0.join("missing");
    let local = s.dir("local");
    s.dir("local/Temp");
    let Some(rel) = drive_relative(&missing) else { return };
    let e = TempEnv { tmp: rel.clone(), temp: rel, local_app_data: local };
    s.with_msys_running(&e, || {
        assert!(!missing.exists(), "the MSYS program created the folder");
        s.check(&e, &missing);
    });
}

/// When the folder TMP names can't be created, gash uses Windows' default per-user temp folder, and
/// heredocs, which gash keeps in temporary files, still work.
#[test]
fn unusable_tmp_folder_falls_back_to_local_app_data() {
    let Some(git_bin) = git_usr_bin() else {
        eprintln!("skipping: Git for Windows' MSYS runtime not found");
        return;
    };
    let s = Setup::new(&git_bin);
    let file = s.scratch.0.join("a-file");
    fs::write(&file, "").unwrap();
    let local = s.dir("local");
    let local_temp = s.dir("local/Temp");
    let under_file: OsString = file.join("tmp").into();
    let e = TempEnv { tmp: under_file.clone(), temp: under_file, local_app_data: local };
    s.check(&e, &local_temp);
}
