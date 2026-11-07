//! gash started with stdout and stderr opened append-only, as Node's `'a'` flag opens them
//! for a child (see `sys::make_append_only_stdio_writable`): output that programs it starts
//! and its own builtins write at the same time all ends up in the file.
//!
//! Skipped when Git for Windows' MSYS tools are not installed.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `$OUT` is the file stdout and stderr go to. The jobs start once awk is writing.
const SCRIPT: &str = r#"
/usr/bin/awk 'BEGIN { for (i = 1; i <= 100000; i++) { print "line " i; fflush() } }' &
until [ -s "$OUT" ]; do :; done
for i in $(seq 1 40); do /usr/bin/echo "job $i" & done
for i in $(seq 1 200); do echo "builtin $i"; done
wait
"#;

fn git_awk() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        roots.push(Path::new(&pf).join("Git"));
    }
    roots.push(PathBuf::from(r"C:\Program Files\Git"));
    roots.into_iter().map(|r| r.join("usr").join("bin").join("awk.exe")).find(|p| p.is_file())
}

/// Removes the output file even when an assertion fails.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn append_only_stdout_keeps_concurrent_output() {
    if git_awk().is_none() {
        eprintln!("skipping: Git for Windows' awk not found");
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let scratch = Scratch(std::env::temp_dir().join(format!("gash-append-stdout-{}-{nanos}.txt", std::process::id())));
    // FILE_APPEND_DATA without FILE_WRITE_DATA, like Node's 'a'.
    let out = OpenOptions::new().append(true).create(true).open(&scratch.0).unwrap();
    let err = out.try_clone().unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_gash"))
        .arg("-c")
        .arg(SCRIPT)
        .env("OUT", &scratch.0)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .status()
        .expect("run gash");
    let text = fs::read_to_string(&scratch.0).unwrap();
    let count = |prefix: &str| text.lines().filter(|l| l.starts_with(prefix)).count();
    let got = (count("line "), count("job "), count("builtin "), text.lines().count());
    let tail: Vec<&str> = text.lines().filter(|l| !l.starts_with("line ")).take(20).collect();
    assert_eq!(got, (100000, 40, 200, 100240), "(awk lines, jobs, builtin lines, all lines); other lines: {tail:?}");
    assert_eq!(status.code(), Some(0));
}
