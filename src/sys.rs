//! Minimal Win32 FFI (no external crates).
#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::sync::atomic::{AtomicBool, Ordering};

pub type HANDLE = *mut core::ffi::c_void;
pub type BOOL = i32;
pub type DWORD = u32;

pub const STD_INPUT_HANDLE: DWORD = -10i32 as DWORD;
pub const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
pub const STD_ERROR_HANDLE: DWORD = -12i32 as DWORD;
pub const FILE_TYPE_DISK: DWORD = 1;
pub const FILE_TYPE_CHAR: DWORD = 2;
pub const FILE_TYPE_PIPE: DWORD = 3;
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: DWORD = 0x0004;
pub const ENABLE_PROCESSED_OUTPUT: DWORD = 0x0001;
pub const PROCESS_TERMINATE: DWORD = 0x0001;
pub const SYNCHRONIZE: DWORD = 0x0010_0000;
pub const WAIT_OBJECT_0: DWORD = 0;
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct COORD {
    pub X: i16,
    pub Y: i16,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct SMALL_RECT {
    pub Left: i16,
    pub Top: i16,
    pub Right: i16,
    pub Bottom: i16,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct CONSOLE_SCREEN_BUFFER_INFO {
    pub dwSize: COORD,
    pub dwCursorPosition: COORD,
    pub wAttributes: u16,
    pub srWindow: SMALL_RECT,
    pub dwMaximumWindowSize: COORD,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILETIME {
    pub lo: u32,
    pub hi: u32,
}

#[link(name = "kernel32")]
extern "system" {
    pub fn GetStdHandle(n: DWORD) -> HANDLE;
    pub fn GetConsoleMode(h: HANDLE, mode: *mut DWORD) -> BOOL;
    pub fn SetConsoleMode(h: HANDLE, mode: DWORD) -> BOOL;
    pub fn GetFileType(h: HANDLE) -> DWORD;
    pub fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(DWORD) -> BOOL>, add: BOOL) -> BOOL;
    pub fn GetConsoleScreenBufferInfo(h: HANDLE, info: *mut CONSOLE_SCREEN_BUFFER_INFO) -> BOOL;
    pub fn GetConsoleOutputCP() -> u32;
    pub fn SetConsoleOutputCP(cp: u32) -> BOOL;
    pub fn GetConsoleCP() -> u32;
    pub fn SetConsoleCP(cp: u32) -> BOOL;
    pub fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
    pub fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn WaitForSingleObject(h: HANDLE, ms: DWORD) -> DWORD;
    pub fn GetProcessTimes(h: HANDLE, c: *mut FILETIME, e: *mut FILETIME, k: *mut FILETIME, u: *mut FILETIME) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn FlushConsoleInputBuffer(h: HANDLE) -> BOOL;
    pub fn GetConsoleWindow() -> HANDLE;
    pub fn SetConsoleTitleW(title: *const u16) -> BOOL;
    pub fn GetLastError() -> DWORD;
    pub fn ReOpenFile(h: HANDLE, access: DWORD, share: DWORD, flags: DWORD) -> HANDLE;
    pub fn SetStdHandle(n: DWORD, h: HANDLE) -> BOOL;
    pub fn SetFilePointerEx(h: HANDLE, dist: i64, new_pos: *mut i64, method: DWORD) -> BOOL;
    pub fn GetFileInformationByHandle(h: HANDLE, info: *mut BY_HANDLE_FILE_INFORMATION) -> BOOL;
    fn CreatePipe(read: *mut HANDLE, write: *mut HANDLE, sa: *mut core::ffi::c_void, size: DWORD) -> BOOL;
}

/// An anonymous pipe whose buffer holds at least `size` bytes, so that much can be
/// written to it without a reader.
pub fn pipe_with_size(size: DWORD) -> std::io::Result<(std::io::PipeReader, std::io::PipeWriter)> {
    use std::os::windows::io::FromRawHandle;
    let mut r: HANDLE = std::ptr::null_mut();
    let mut w: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null_mut(), size) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe { Ok((std::io::PipeReader::from_raw_handle(r as _), std::io::PipeWriter::from_raw_handle(w as _))) }
}

#[repr(C)]
#[derive(Default)]
pub struct BY_HANDLE_FILE_INFORMATION {
    pub dwFileAttributes: DWORD,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub dwVolumeSerialNumber: DWORD,
    pub nFileSizeHigh: DWORD,
    pub nFileSizeLow: DWORD,
    pub nNumberOfLinks: DWORD,
    pub nFileIndexHigh: DWORD,
    pub nFileIndexLow: DWORD,
}

#[repr(C)]
struct IO_STATUS_BLOCK {
    status: usize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationFile(h: HANDLE, iosb: *mut IO_STATUS_BLOCK, info: *mut core::ffi::c_void, len: u32, class: u32) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn NtWriteFile(
        h: HANDLE,
        event: HANDLE,
        apc: *mut core::ffi::c_void,
        apc_context: *mut core::ffi::c_void,
        iosb: *mut IO_STATUS_BLOCK,
        buf: *const core::ffi::c_void,
        len: u32,
        offset: *const i64,
        key: *const u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

/// Write to the end of the file, wherever the handle's position is, as one append
/// (FILE_WRITE_TO_END_OF_FILE), so that writers sharing the file can't land on the same
/// offset. The position is left just past the data, at the end. Returns the bytes written.
pub fn write_at_end(h: HANDLE, data: &[u8]) -> std::io::Result<usize> {
    let mut iosb = IO_STATUS_BLOCK { status: 0, information: 0 };
    let end: i64 = -1; // FILE_WRITE_TO_END_OF_FILE in the low part, -1 in the high part
    let len = data.len().min(u32::MAX as usize) as u32;
    let st = unsafe {
        NtWriteFile(
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut iosb,
            data.as_ptr() as *const _,
            len,
            &end,
            std::ptr::null(),
        )
    };
    if st < 0 {
        return Err(std::io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(st) } as i32));
    }
    Ok(iosb.information)
}

const FILE_READ_DATA: DWORD = 0x0001;
const FILE_WRITE_DATA: DWORD = 0x0002;
const FILE_APPEND_DATA: DWORD = 0x0004;
const FILE_GENERIC_READ: DWORD = 0x0012_0089;
const FILE_GENERIC_WRITE: DWORD = 0x0012_0116;
const FILE_ACCESS_INFORMATION: u32 = 8;
const FILE_POSITION_INFORMATION: u32 = 14;
const FILE_SHARE_ALL: DWORD = 0x7; // read | write | delete
const FILE_END: DWORD = 2;

/// The position of `h`'s file object. `SetFilePointerEx(h, 0, FILE_CURRENT)`, what
/// `stream_position` does, sets the position it has just read, so when a program sharing the
/// handle writes in between, it moves the position back over that write, and the next write
/// lands on top of it. This only reads it.
fn file_position(h: HANDLE) -> Option<u64> {
    let mut iosb = IO_STATUS_BLOCK { status: 0, information: 0 };
    let mut pos: i64 = 0;
    let st = unsafe {
        NtQueryInformationFile(h, &mut iosb, &mut pos as *mut i64 as *mut _, 8, FILE_POSITION_INFORMATION)
    };
    (st >= 0).then_some(pos as u64)
}

/// Move `h`, a file handle that programs we started may be writing through, to the end of its
/// file if it isn't there, that is if another handle to the file has appended since. Moving
/// it there takes two calls (SetFilePointerEx reads the length, then sets the position), so a
/// write through `h` in between would be undone; leaving the position alone when it is at the
/// end keeps that to the case where it was behind anyway. The length is read first: writes
/// through `h` only move the position forward, so they can't make it look behind.
pub fn seek_end_if_behind(h: HANDLE) {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return;
    }
    let len = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    if file_position(h).is_some_and(|pos| pos < len) {
        unsafe { SetFilePointerEx(h, 0, std::ptr::null_mut(), FILE_END) };
    }
}

fn valid(h: HANDLE) -> bool {
    !h.is_null() && h as isize != -1
}

pub fn is_disk(h: HANDLE) -> bool {
    valid(h) && unsafe { GetFileType(h) } == FILE_TYPE_DISK
}

/// The access rights a handle was opened with.
fn granted_access(h: HANDLE) -> Option<DWORD> {
    let mut iosb = IO_STATUS_BLOCK { status: 0, information: 0 };
    let mut flags: DWORD = 0;
    let st = unsafe {
        NtQueryInformationFile(h, &mut iosb, &mut flags as *mut DWORD as *mut _, 4, FILE_ACCESS_INFORMATION)
    };
    (st >= 0).then_some(flags)
}

/// Same file (volume and file index), not necessarily the same handle or file object.
pub fn same_file(a: HANDLE, b: HANDLE) -> bool {
    let mut x = BY_HANDLE_FILE_INFORMATION::default();
    let mut y = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(a, &mut x) != 0
            && GetFileInformationByHandle(b, &mut y) != 0
            && x.dwVolumeSerialNumber == y.dwVolumeSerialNumber
            && (x.nFileIndexHigh, x.nFileIndexLow) == (y.nFileIndexHigh, y.nFileIndexLow)
    }
}

/// For a disk file opened append-only (FILE_APPEND_DATA without FILE_WRITE_DATA), a second
/// handle to it with ordinary write access. The caller owns it. None for any other handle,
/// or if the file can't be opened again.
fn reopen_append_only(h: HANDLE) -> Option<HANDLE> {
    if !is_append_only(h) {
        return None;
    }
    let acc = granted_access(h)?;
    let want = FILE_GENERIC_WRITE | if acc & FILE_READ_DATA != 0 { FILE_GENERIC_READ } else { 0 };
    let nh = unsafe { ReOpenFile(h, want, FILE_SHARE_ALL, 0) };
    valid(nh).then_some(nh)
}

static STDOUT_APPEND: AtomicBool = AtomicBool::new(false);
static STDERR_APPEND: AtomicBool = AtomicBool::new(false);

/// Our parent may give us stdout or stderr opened append-only (Node's `'a'` flag, Rust's
/// `OpenOptions::append`). MSYS programs take an inherited handle as writable only when it
/// has FILE_WRITE_DATA, so every write the MSYS programs we start made to it would fail.
/// Replace such a handle with a writable one to the same file and treat it like a `>>` file
/// (`io::Handle::Append`): see `seek_std_append`. When stdout and stderr are the same file,
/// they share one handle, and so one position, so their output can't overwrite each other.
pub fn make_append_only_stdio_writable() {
    let out = std_handle(STD_OUTPUT_HANDLE);
    let err = std_handle(STD_ERROR_HANDLE);
    let new_out = reopen_append_only(out);
    if let Some(h) = new_out {
        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, h) };
        STDOUT_APPEND.store(true, Ordering::SeqCst);
    }
    let new_err = match new_out {
        Some(h) if out == err || (is_append_only(err) && same_file(out, err)) => Some(h),
        _ => reopen_append_only(err),
    };
    if let Some(h) = new_err {
        unsafe { SetStdHandle(STD_ERROR_HANDLE, h) };
        STDERR_APPEND.store(true, Ordering::SeqCst);
    }
}

/// Is `h` a disk file opened append-only?
fn is_append_only(h: HANDLE) -> bool {
    is_disk(h)
        && granted_access(h).is_some_and(|a| a & FILE_APPEND_DATA != 0 && a & FILE_WRITE_DATA == 0)
}

/// Move stdout or stderr to the end of its file if `make_append_only_stdio_writable`
/// replaced it, as O_APPEND would before a write. Called before we write to it and before
/// a child inherits it, while children started before may still be writing through it, so
/// only when another handle has appended since (`seek_end_if_behind`).
pub fn seek_std_append(n: DWORD) {
    let flag = if n == STD_ERROR_HANDLE { &STDERR_APPEND } else { &STDOUT_APPEND };
    if flag.load(Ordering::Relaxed) {
        seek_end_if_behind(std_handle(n));
    }
}

#[link(name = "advapi32")]
extern "system" {
    pub fn GetUserNameW(buf: *mut u16, len: *mut DWORD) -> BOOL;
}

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn ctrl_handler(kind: DWORD) -> BOOL {
    match kind {
        0 | 1 => {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        }
        _ => 0,
    }
}

pub fn install_ctrl_handler() {
    unsafe {
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

/// Clear an inherited "ignore Ctrl+C" attribute (set when a parent started us with
/// CREATE_NEW_PROCESS_GROUP), so Ctrl+C reaches us and the children we start.
pub fn clear_ctrl_c_ignore() {
    unsafe {
        SetConsoleCtrlHandler(None, 0);
    }
}

pub fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Whether this process is attached to a console at all (GetConsoleCP fails without one).
/// It isn't when a parent started us with DETACHED_PROCESS, e.g. an editor's or agent's
/// background task. Then every console program we start would get a new visible console
/// window, so run_external starts them with CREATE_NO_WINDOW instead (Cygwin does the
/// same job with a hidden console).
pub fn has_console() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| unsafe { GetConsoleCP() != 0 })
}

pub fn std_handle(n: DWORD) -> HANDLE {
    unsafe { GetStdHandle(n) }
}

pub fn is_console(n: DWORD) -> bool {
    let h = std_handle(n);
    if h.is_null() || h as isize == -1 {
        return false;
    }
    let mut m = 0;
    unsafe { GetConsoleMode(h, &mut m) != 0 }
}

pub fn file_type(n: DWORD) -> DWORD {
    let h = std_handle(n);
    if h.is_null() || h as isize == -1 {
        return 0;
    }
    unsafe { GetFileType(h) }
}

/// Console window size (columns, lines).
pub fn console_size() -> Option<(usize, usize)> {
    for n in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE, STD_INPUT_HANDLE] {
        let h = std_handle(n);
        let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
        if unsafe { GetConsoleScreenBufferInfo(h, &mut info) } != 0 {
            let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as usize;
            let lines = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as usize;
            return Some((cols, lines));
        }
    }
    None
}

pub fn enable_vt_output() {
    for n in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = std_handle(n);
        let mut m = 0;
        unsafe {
            if GetConsoleMode(h, &mut m) != 0 {
                SetConsoleMode(h, m | ENABLE_VIRTUAL_TERMINAL_PROCESSING | ENABLE_PROCESSED_OUTPUT);
            }
        }
    }
}

pub fn kill_pid(pid: u32, code: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, code) != 0;
        CloseHandle(h);
        ok
    }
}

/// Whether the process behind a handle we hold is still running.
pub fn process_running(h: HANDLE) -> bool {
    unsafe { WaitForSingleObject(h, 0) != WAIT_OBJECT_0 }
}

pub fn pid_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return false;
        }
        let r = WaitForSingleObject(h, 0);
        CloseHandle(h);
        r != WAIT_OBJECT_0
    }
}

fn ft_secs(f: &FILETIME) -> f64 {
    let v = ((f.hi as u64) << 32) | f.lo as u64;
    v as f64 / 10_000_000.0
}

/// (user, system) CPU seconds of another process (by handle).
pub fn handle_times(h: HANDLE) -> (f64, f64) {
    let mut c = FILETIME::default();
    let mut e = FILETIME::default();
    let mut k = FILETIME::default();
    let mut u = FILETIME::default();
    unsafe {
        if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
            return (ft_secs(&u), ft_secs(&k));
        }
    }
    (0.0, 0.0)
}

/// Accumulated CPU time of child processes waited for by this process.
pub static CHILD_CPU_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static CHILD_SYS_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn add_child_times(u: f64, s: f64) {
    CHILD_CPU_US.fetch_add((u * 1e6) as u64, Ordering::SeqCst);
    CHILD_SYS_US.fetch_add((s * 1e6) as u64, Ordering::SeqCst);
}

/// (user, system) CPU seconds of this process plus waited-for children.
pub fn total_times() -> (f64, f64) {
    let (u, s) = process_times();
    (u + CHILD_CPU_US.load(Ordering::SeqCst) as f64 / 1e6, s + CHILD_SYS_US.load(Ordering::SeqCst) as f64 / 1e6)
}

/// (user, system) CPU seconds of this process.
pub fn process_times() -> (f64, f64) {
    let mut c = FILETIME::default();
    let mut e = FILETIME::default();
    let mut k = FILETIME::default();
    let mut u = FILETIME::default();
    unsafe {
        if GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) != 0 {
            return (ft_secs(&u), ft_secs(&k));
        }
    }
    (0.0, 0.0)
}

pub fn user_name() -> String {
    if let Ok(u) = std::env::var("USERNAME") {
        return u;
    }
    let mut buf = [0u16; 257];
    let mut len = buf.len() as DWORD;
    unsafe {
        if GetUserNameW(buf.as_mut_ptr(), &mut len) != 0 && len > 0 {
            return String::from_utf16_lossy(&buf[..(len - 1) as usize]);
        }
    }
    "user".into()
}

pub fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}

pub fn set_title(t: &str) {
    let w: Vec<u16> = t.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        SetConsoleTitleW(w.as_ptr());
    }
}

/// Wait until the console input handle has input or `ms` elapses. Returns true if input is ready.
pub fn wait_stdin(ms: u32) -> bool {
    let h = std_handle(STD_INPUT_HANDLE);
    unsafe { WaitForSingleObject(h, ms) == WAIT_OBJECT_0 }
}

pub struct ConsoleCp {
    out: u32,
    inp: u32,
}

pub fn set_utf8_console() -> Option<ConsoleCp> {
    unsafe {
        let out = GetConsoleOutputCP();
        let inp = GetConsoleCP();
        if out == 0 {
            return None;
        }
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
        Some(ConsoleCp { out, inp })
    }
}

impl Drop for ConsoleCp {
    fn drop(&mut self) {
        unsafe {
            SetConsoleOutputCP(self.out);
            SetConsoleCP(self.inp);
        }
    }
}
