//! File-descriptor table and handle abstraction. The shell never relies on process-wide
//! stdio state, so subshells can run in threads with their own fd tables.

use crate::sys;
use std::fs::File;
use std::io::{self, BufRead, PipeReader, PipeWriter, Read, Seek, SeekFrom, Write};
use std::mem::ManuallyDrop;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Here-document and here-string input. It starts in memory so builtins can read it
/// cheaply. The first time it's handed to a child process, the unread part moves to a
/// pipe or file that the shell and every later child share, as bash does: a child that
/// reads only part of it, or none, leaves the rest for the next reader.
#[derive(Debug, Default)]
pub struct MemReader {
    data: Vec<u8>,
    pos: usize,
    /// Where the input went once a child needed it: `Handle::PipeR` or `Handle::File`.
    shared: Option<Handle>,
}

/// Input up to this size goes in a pipe that holds all of it, and larger input in a
/// temporary file. Like bash 5.1+ (and Git Bash): a program that reads part of its
/// input can seek back over the excess in a file, but not in a pipe.
const HEREDOC_PIPE_MAX: usize = 64 * 1024;

impl MemReader {
    pub fn new(data: Vec<u8>) -> MemReader {
        MemReader { data, pos: 0, shared: None }
    }

    /// The pipe or file holding the unread input, creating it on first use.
    fn shared(&mut self) -> io::Result<Handle> {
        if let Some(h) = &self.shared {
            return Ok(h.clone());
        }
        let rest = &self.data[self.pos..];
        let h = if rest.len() <= HEREDOC_PIPE_MAX {
            // The pipe's buffer holds all of it, so this write doesn't wait for a reader.
            let (r, mut w) = sys::pipe_with_size(HEREDOC_PIPE_MAX as u32)?;
            w.write_all(rest)?;
            Handle::PipeR(Arc::new(r))
        } else {
            // All of it, positioned after what builtins already read: a program that
            // reopens the file by name (/dev/stdin) reads from the start, as with bash.
            let mut f = temp_file()?;
            f.write_all(&self.data)?;
            f.seek(SeekFrom::Start(self.pos as u64))?;
            Handle::File(Arc::new(f))
        };
        self.data = Vec::new();
        self.pos = 0;
        self.shared = Some(h.clone());
        Ok(h)
    }

    /// The unread input as a file, for an MSYS child whose arguments name it
    /// (`/dev/stdin`, `/dev/fd/0`, `/proc/self/fd/0`). MSYS can reopen those only when
    /// they are a file (or a pipe it made itself; ours shows up as `pipe:[0]` and fails
    /// with "No such file or directory").
    ///
    /// Returns `None` when the input is already in a file, which the child then shares
    /// as usual. Otherwise the input moves to a new file for the duration of the child,
    /// and [`MemReader::return_file`] must be called once the child has exited.
    pub fn lend_file(&mut self) -> io::Result<Option<Arc<File>>> {
        let rest = match &self.shared {
            Some(Handle::File(_)) => return Ok(None),
            Some(h) => {
                // The pipe's write end is closed, so this stops at the end of the input.
                let mut v = Vec::new();
                let mut buf = [0u8; 8192];
                while let Ok(n @ 1..) = h.read(&mut buf) {
                    v.extend_from_slice(&buf[..n]);
                }
                v
            }
            None if self.data.len() > HEREDOC_PIPE_MAX => {
                self.shared()?;
                return Ok(None);
            }
            None => self.data.split_off(self.pos),
        };
        let mut f = temp_file()?;
        f.write_all(&rest)?;
        f.seek(SeekFrom::Start(0))?;
        let f = Arc::new(f);
        self.data = Vec::new();
        self.pos = 0;
        self.shared = Some(Handle::File(f.clone()));
        Ok(Some(f))
    }

    /// After a child that got [`MemReader::lend_file`]'s file has exited: put what is left
    /// back in a pipe, as bash would have had it all along.
    ///
    /// With bash the child would have reopened that pipe, and whatever it read through
    /// /dev/stdin would be gone. Reads through a reopened file don't move our position,
    /// so they can't be measured: if the child didn't read through its stdin handle at
    /// all, it is taken to have read everything by name. (Most programs read a file
    /// they are given to the end.)
    pub fn return_file(&mut self, f: &Arc<File>) {
        if !matches!(&self.shared, Some(Handle::File(s)) if Arc::ptr_eq(s, f)) {
            return;
        }
        let mut rest = Vec::new();
        if (&**f).stream_position().map(|p| p > 0).unwrap_or(false) {
            let _ = (&**f).read_to_end(&mut rest);
        }
        if rest.len() > HEREDOC_PIPE_MAX {
            return;
        }
        self.shared = None;
        self.data = rest;
        self.pos = 0;
        // On failure it stays in memory, and the next child gets it through a pump thread.
        let _ = self.shared();
    }
}

/// A new temporary file that Windows deletes once the last handle to it, ours or a
/// child's, is closed.
fn temp_file() -> io::Result<File> {
    temp_file_named("gash-heredoc").map(|(f, _)| f)
}

/// A response file holding `content`, for an MSYS program to read through an `@file`
/// argument (see `exec::add_msys_args`). Windows deletes it once the last handle is closed.
pub fn response_file(content: &[u8]) -> io::Result<(File, std::path::PathBuf)> {
    let (mut f, path) = temp_file_named("gash-args")?;
    f.write_all(content)?;
    f.flush()?;
    Ok((f, path))
}

/// Like `temp_file`, and also returns the path. The file name starts with `prefix`.
fn temp_file_named(prefix: &str) -> io::Result<(File, std::path::PathBuf)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    const FILE_ATTRIBUTE_TEMPORARY: u32 = 0x100;
    const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut last = None;
    for _ in 0..16 {
        let name = format!(
            "{}-{}-{}-{:08x}.tmp",
            prefix,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            nanos
        );
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(0x7) // read | write | delete
            .attributes(FILE_ATTRIBUTE_TEMPORARY)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(&path)
        {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "cannot create temporary file")))
}

#[derive(Clone)]
pub enum Handle {
    Stdin,
    Stdout,
    Stderr,
    File(Arc<File>),
    /// A file opened with `>>`. It has ordinary write access, because MSYS programs take an
    /// inherited handle as writable only when it has FILE_WRITE_DATA, so it can't be an
    /// append-only handle. Instead, like Cygwin's O_APPEND, it moves to the end of the file
    /// before each write we make and before handing it to a child. Children share its
    /// position, so they and the shell write one after another rather than over each other.
    Append(Arc<File>),
    PipeR(Arc<PipeReader>),
    PipeW(Arc<PipeWriter>),
    Buf(Arc<Mutex<Vec<u8>>>),
    Mem(Arc<Mutex<MemReader>>),
    Null,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Handle::Stdin => "stdin",
            Handle::Stdout => "stdout",
            Handle::Stderr => "stderr",
            Handle::File(_) | Handle::Append(_) => "file",
            Handle::PipeR(_) => "pipe-r",
            Handle::PipeW(_) => "pipe-w",
            Handle::Buf(_) => "buf",
            Handle::Mem(_) => "mem",
            Handle::Null => "null",
        };
        f.write_str(s)
    }
}

fn raw_std(n: sys::DWORD) -> ManuallyDrop<File> {
    unsafe { ManuallyDrop::new(File::from_raw_handle(sys::std_handle(n) as _)) }
}

pub fn console_stdin() -> bool {
    sys::is_console(sys::STD_INPUT_HANDLE)
}

/// Write bytes to the process stdout/stderr. Console output goes through Rust's std which
/// converts UTF-8 to UTF-16; invalid UTF-8 is written lossily.
fn write_std(err: bool, data: &[u8]) -> io::Result<()> {
    sys::seek_std_append(if err { sys::STD_ERROR_HANDLE } else { sys::STD_OUTPUT_HANDLE });
    let r = if err {
        let mut e = io::stderr().lock();
        e.write_all(data).and_then(|_| e.flush())
    } else {
        let mut o = io::stdout().lock();
        o.write_all(data).and_then(|_| o.flush())
    };
    match r {
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            let s = String::from_utf8_lossy(data).into_owned();
            if err {
                let mut e = io::stderr().lock();
                e.write_all(s.as_bytes())?;
                e.flush()
            } else {
                let mut o = io::stdout().lock();
                o.write_all(s.as_bytes())?;
                o.flush()
            }
        }
        r => r,
    }
}

impl Handle {
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        match self {
            Handle::Stdout => write_std(false, data),
            Handle::Stderr => write_std(true, data),
            Handle::File(f) => (&**f).write_all(data),
            Handle::Append(f) => {
                // Each write is one append at the current end (sys::write_at_end), which also
                // leaves the shared position there. Seeking to the end and then writing could
                // land at a stale end when another writer appended in between, and overwrite it.
                let mut rest = data;
                while !rest.is_empty() {
                    let n = sys::write_at_end(f.as_raw_handle() as sys::HANDLE, rest)?;
                    if n == 0 {
                        return Err(io::ErrorKind::WriteZero.into());
                    }
                    rest = &rest[n..];
                }
                Ok(())
            }
            Handle::PipeW(p) => (&**p).write_all(data),
            Handle::Buf(b) => {
                b.lock().unwrap().extend_from_slice(data);
                Ok(())
            }
            Handle::Null => Ok(()),
            Handle::Stdin | Handle::PipeR(_) | Handle::Mem(_) => {
                Err(io::Error::new(io::ErrorKind::Other, "Bad file descriptor"))
            }
        }
    }

    pub fn is_console(&self) -> bool {
        match self {
            Handle::Stdin => sys::is_console(sys::STD_INPUT_HANDLE),
            Handle::Stdout => sys::is_console(sys::STD_OUTPUT_HANDLE),
            Handle::Stderr => sys::is_console(sys::STD_ERROR_HANDLE),
            _ => false,
        }
    }

    pub fn readable(&self) -> bool {
        matches!(self, Handle::Stdin | Handle::File(_) | Handle::PipeR(_) | Handle::Mem(_) | Handle::Null)
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Handle::Stdin => {
                if console_stdin() {
                    io::stdin().lock().read(buf)
                } else {
                    let f = raw_std(sys::STD_INPUT_HANDLE);
                    match (&*f).read(buf) {
                        Err(e) if e.raw_os_error() == Some(109) => Ok(0),
                        r => r,
                    }
                }
            }
            Handle::File(f) => (&**f).read(buf),
            Handle::PipeR(p) => match (&**p).read(buf) {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
                r => r,
            },
            Handle::Mem(m) => {
                let mut m = m.lock().unwrap();
                if let Some(h) = m.shared.clone() {
                    drop(m);
                    return h.read(buf);
                }
                let n = buf.len().min(m.data.len() - m.pos);
                let p = m.pos;
                buf[..n].copy_from_slice(&m.data[p..p + n]);
                m.pos += n;
                Ok(n)
            }
            Handle::Null => Ok(0),
            _ => Err(io::Error::new(io::ErrorKind::Other, "Bad file descriptor")),
        }
    }

    pub fn read_to_end(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let n = self.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }

    /// Read up to and including `delim` without consuming input beyond it (so that
    /// child processes sharing the handle see the rest). Returns (data without delim, found_delim).
    pub fn read_until(&self, delim: u8, max: Option<usize>) -> io::Result<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        let limit = max.unwrap_or(usize::MAX);
        match self {
            Handle::Mem(m) => {
                let mut m = m.lock().unwrap();
                if let Some(h) = m.shared.clone() {
                    drop(m);
                    return h.read_until(delim, max);
                }
                while m.pos < m.data.len() && out.len() < limit {
                    let b = m.data[m.pos];
                    m.pos += 1;
                    if b == delim {
                        return Ok((out, true));
                    }
                    out.push(b);
                }
                Ok((out, false))
            }
            Handle::File(f) => read_until_seekable(&f, delim, limit),
            Handle::Stdin if console_stdin() => {
                let stdin = io::stdin();
                let mut lock = stdin.lock();
                if max.is_none() {
                    let n = lock.read_until(delim, &mut out)?;
                    if n > 0 && out.last() == Some(&delim) {
                        out.pop();
                        return Ok((out, true));
                    }
                    return Ok((out, false));
                }
                let mut b = [0u8; 1];
                while out.len() < limit {
                    if lock.read(&mut b)? == 0 {
                        return Ok((out, false));
                    }
                    if b[0] == delim {
                        return Ok((out, true));
                    }
                    out.push(b[0]);
                }
                Ok((out, false))
            }
            Handle::Stdin if sys::file_type(sys::STD_INPUT_HANDLE) == sys::FILE_TYPE_DISK => {
                let f = raw_std(sys::STD_INPUT_HANDLE);
                read_until_seekable(&f, delim, limit)
            }
            _ => {
                let mut b = [0u8; 1];
                while out.len() < limit {
                    if self.read(&mut b)? == 0 {
                        return Ok((out, false));
                    }
                    if b[0] == delim {
                        return Ok((out, true));
                    }
                    out.push(b[0]);
                }
                Ok((out, false))
            }
        }
    }

    /// Convert to a Stdio for a child process. Returns an optional helper thread
    /// (pump) that must be joined after the child exits.
    pub fn to_stdio(&self, fd: i32) -> io::Result<(Stdio, Option<JoinHandle<()>>)> {
        Ok(match self {
            Handle::Stdin => {
                if fd == 0 {
                    (Stdio::inherit(), None)
                } else {
                    (dup_std(io::stdin().as_handle())?, None)
                }
            }
            Handle::Stdout => {
                sys::seek_std_append(sys::STD_OUTPUT_HANDLE);
                if fd == 1 {
                    (Stdio::inherit(), None)
                } else {
                    (dup_std(io::stdout().as_handle())?, None)
                }
            }
            Handle::Stderr => {
                sys::seek_std_append(sys::STD_ERROR_HANDLE);
                if fd == 2 {
                    (Stdio::inherit(), None)
                } else {
                    (dup_std(io::stderr().as_handle())?, None)
                }
            }
            Handle::File(f) => (Stdio::from(f.try_clone()?), None),
            Handle::Append(f) => {
                // Writes through this handle leave its position at the end (see `write_all`), so it
                // needs moving only when another handle to the file has appended since. Programs
                // started with it before may still be writing through it.
                sys::seek_end_if_behind(f.as_raw_handle() as sys::HANDLE);
                (Stdio::from(f.try_clone()?), None)
            }
            Handle::PipeR(p) => (Stdio::from(p.try_clone()?), None),
            Handle::PipeW(p) => (Stdio::from(p.try_clone()?), None),
            Handle::Null => (Stdio::null(), None),
            Handle::Buf(b) => {
                let (mut r, w) = io::pipe()?;
                let b = b.clone();
                let h = std::thread::spawn(move || {
                    let mut tmp = vec![0u8; 65536];
                    loop {
                        match r.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => b.lock().unwrap().extend_from_slice(&tmp[..n]),
                        }
                    }
                });
                (Stdio::from(w), Some(h))
            }
            Handle::Mem(m) => {
                let mut m = m.lock().unwrap();
                match m.shared() {
                    Ok(h) => h.to_stdio(fd)?,
                    // No pipe or file to share: feed the child through a pump thread
                    // instead. It gets all of the unread input, so none is left for
                    // later readers.
                    Err(_) => {
                        let data = std::mem::take(&mut m.data).split_off(m.pos);
                        m.pos = 0;
                        let (r, mut w) = io::pipe()?;
                        let h = std::thread::spawn(move || {
                            let _ = w.write_all(&data);
                        });
                        (Stdio::from(r), Some(h))
                    }
                }
            }
        })
    }
}

fn dup_std(h: std::os::windows::io::BorrowedHandle<'_>) -> io::Result<Stdio> {
    match h.try_clone_to_owned() {
        Ok(o) => Ok(Stdio::from(o)),
        Err(_) => Ok(Stdio::null()),
    }
}

fn read_until_seekable(f: &File, delim: u8, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut f = f;
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let want = chunk.len().min(limit.saturating_sub(out.len()).max(1));
        let n = f.read(&mut chunk[..want])?;
        if n == 0 {
            return Ok((out, false));
        }
        if let Some(i) = chunk[..n].iter().position(|&b| b == delim) {
            out.extend_from_slice(&chunk[..i]);
            let back = (n - i - 1) as i64;
            if back > 0 {
                f.seek(SeekFrom::Current(-back))?;
            }
            return Ok((out, true));
        }
        let take = n.min(limit - out.len());
        out.extend_from_slice(&chunk[..take]);
        if take < n {
            f.seek(SeekFrom::Current(-((n - take) as i64)))?;
        }
        if out.len() >= limit {
            return Ok((out, false));
        }
    }
}

#[derive(Clone)]
pub struct Fds {
    pub v: Vec<Option<Handle>>,
}

impl Fds {
    pub fn std() -> Fds {
        Fds { v: vec![Some(Handle::Stdin), Some(Handle::Stdout), Some(Handle::Stderr)] }
    }
    pub fn get(&self, fd: i32) -> Option<&Handle> {
        if fd < 0 {
            return None;
        }
        self.v.get(fd as usize).and_then(|h| h.as_ref())
    }
    pub fn set(&mut self, fd: i32, h: Option<Handle>) -> Option<Handle> {
        let i = fd as usize;
        if self.v.len() <= i {
            self.v.resize(i + 1, None);
        }
        std::mem::replace(&mut self.v[i], h)
    }
    pub fn free_fd(&self, from: i32) -> i32 {
        let mut fd = from;
        while self.get(fd).is_some() {
            fd += 1;
        }
        fd
    }
}

/// A file open for `>>`, shared by every redirection to it in this process (see
/// `shared_append`).
struct AppendFile {
    file: Arc<File>,
    /// Programs it was handed to. It stays shared while they run, even after the redirection
    /// that started them is gone (`cmd >> log &`). Process handles rather than ids: Windows
    /// reuses an id once its process is gone, and an unrelated process with it would keep the
    /// file open.
    children: Vec<std::os::windows::io::OwnedHandle>,
}

static APPEND_FILES: Mutex<Vec<AppendFile>> = Mutex::new(Vec::new());

/// The handle for a file just opened with `>>`. If another `>>` redirection in this process,
/// or a program started with one, still has the same file open, this shares that handle, and
/// so its file object and position, instead of adding a second one. Programs appending to the
/// file through separate redirections at the same time (`cmd >> log &` in a loop) then write
/// one after another, as with Cygwin's O_APPEND, rather than at the same offset and over each
/// other.
pub fn shared_append(f: File) -> Arc<File> {
    let mut open = APPEND_FILES.lock().unwrap();
    prune_append(&mut open);
    let h = f.as_raw_handle() as sys::HANDLE;
    if let Some(a) = open.iter().find(|a| sys::same_file(a.file.as_raw_handle() as sys::HANDLE, h)) {
        return a.file.clone();
    }
    let file = Arc::new(f);
    open.push(AppendFile { file: file.clone(), children: Vec::new() });
    file
}

/// Record that `child` was started with these `>>` handles.
pub fn append_started(files: &[Arc<File>], child: &std::process::Child) {
    let mut open = APPEND_FILES.lock().unwrap();
    for a in open.iter_mut().filter(|a| files.iter().any(|f| Arc::ptr_eq(f, &a.file))) {
        if let Ok(h) = child.as_handle().try_clone_to_owned() {
            a.children.push(h);
        }
    }
}

/// Forget the `>>` files that no redirection holds and no program started with them still
/// writes to, closing them. Called when redirections are undone.
pub fn prune_append_files() {
    if let Ok(mut open) = APPEND_FILES.try_lock() {
        prune_append(&mut open);
    }
}

fn prune_append(open: &mut Vec<AppendFile>) {
    open.retain_mut(|a| {
        a.children.retain(|h| sys::process_running(h.as_raw_handle() as sys::HANDLE));
        Arc::strong_count(&a.file) > 1 || !a.children.is_empty()
    });
}
