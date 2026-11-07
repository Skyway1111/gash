//! `du` in-process, for Git's GNU du found by PATH lookup. GNU du under the MSYS runtime opens
//! every file to stat it (about 1.5 ms each with Defender); this reads sizes from directory
//! listings, one call per directory, and computes blocks as the runtime's `fstat_helper` does,
//! so the output is GNU du's. It covers `-a -s -c -h -k -m -b -x -0 -d N --apparent-size`; for
//! anything else, or anything unusual on the way (an error, a reparse point other than a link,
//! a name that isn't Unicode), it returns `None` and GNU du runs instead. As in Explorer, the
//! size of a file another program is writing can lag until that program closes it.

use crate::shell::{Flow, Shell};
use crate::sys;
use std::collections::HashSet;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const FILE_STANDARD_INFO: u32 = 1;
const FILE_ID_BOTH_DIRECTORY_INFO: u32 = 10;
const FILE_ID_BOTH_DIRECTORY_RESTART_INFO: u32 = 11;
/// gnulib's power_letter in coreutils 8.32 (Git's du).
const LETTERS: [char; 9] = ['\0', 'K', 'M', 'G', 'T', 'P', 'E', 'Z', 'Y'];

struct Du {
    apparent: bool,
    all: bool,
    max_depth: usize,
    /// Output block size; 1 with `human`.
    block: u64,
    human: bool,
    null: bool,
    /// More than one operand: GNU du then counts every file and directory once.
    hash_all: bool,
    seen: HashSet<(u32, u64)>,
    out: Vec<u8>,
}

/// Run `du args` if this implementation covers it; `None` means run GNU du.
pub fn run(sh: &Shell, args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Option<Result<i32, Flow>> {
    if ["DU_BLOCK_SIZE", "BLOCK_SIZE", "BLOCKSIZE", "POSIXLY_CORRECT"].iter().any(|v| env(v).is_some()) {
        return None;
    }
    let mut du = Du { apparent: false, all: false, max_depth: usize::MAX, block: 1024, human: false, null: false, hash_all: false, seen: HashSet::new(), out: Vec::new() };
    let (mut summarize, mut total, mut operands, mut depth) = (false, false, Vec::new(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => operands.extend(it.by_ref().cloned()),
            "--all" => du.all = true,
            "--summarize" => summarize = true,
            "--total" => total = true,
            "--human-readable" => (du.human, du.block) = (true, 1),
            "--bytes" => (du.apparent, du.human, du.block) = (true, false, 1),
            "--apparent-size" => du.apparent = true,
            "--one-file-system" => {} // links and mount points are never followed
            "--null" => du.null = true,
            "--max-depth" => depth = Some(it.next()?.parse().ok()?),
            _ if a.starts_with("--max-depth=") => depth = Some(a["--max-depth=".len()..].parse().ok()?),
            _ if a.starts_with("--") => return None,
            _ if a.len() > 1 && a.starts_with('-') => {
                for (i, c) in a.char_indices().skip(1) {
                    match c {
                        'a' => du.all = true,
                        's' => summarize = true,
                        'c' => total = true,
                        'h' => (du.human, du.block) = (true, 1),
                        'k' => (du.human, du.block) = (false, 1024),
                        'm' => (du.human, du.block) = (false, 1024 * 1024),
                        'b' => (du.apparent, du.human, du.block) = (true, false, 1),
                        'x' => {}
                        '0' => du.null = true,
                        'd' => {
                            let rest = &a[i + 1..];
                            depth = Some(if rest.is_empty() { it.next()? } else { rest }.parse().ok()?);
                            break;
                        }
                        _ => return None,
                    }
                }
            }
            _ => operands.push(a.clone()),
        }
    }
    if summarize {
        if du.all || depth.is_some_and(|d| d != 0) {
            return None; // GNU's usage errors
        }
        depth = Some(0);
    }
    du.max_depth = depth.unwrap_or(usize::MAX);
    if du.human {
        // `-h` prints the locale's decimal point.
        let loc = ["LC_ALL", "LC_NUMERIC", "LANG"].iter().find_map(|v| env(v).filter(|s| !s.is_empty())).unwrap_or_default();
        if !(loc.is_empty() || loc.starts_with('C') || loc == "POSIX" || loc.starts_with("en_")) {
            return None;
        }
    }
    if operands.is_empty() {
        operands.push(".".into());
    }
    if operands.iter().any(|o| o.is_empty() || o == "-" || o.ends_with("//")) {
        return None;
    }
    du.hash_all = operands.len() > 1;
    let mut grand = 0;
    for op in &operands {
        grand += du.operand(op, &crate::path::to_win(op, &sh.cwd))?;
    }
    if total {
        du.print(grand, "total");
    }
    Some(sh.out(1, &du.out).map(|()| 0))
}

impl Du {
    fn operand(&mut self, disp: &str, win: &Path) -> Option<u64> {
        let md = std::fs::symlink_metadata(win).ok()?;
        use std::os::windows::fs::MetadataExt;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return None;
        }
        if md.is_dir() {
            return self.dir(disp, win, 0);
        }
        let f = open(win, 0x80, 0)?; // FILE_READ_ATTRIBUTES
        let (key, attrs) = handle_id(&f)?;
        if self.hash_all && !self.seen.insert(key) {
            return Some(0);
        }
        let (alloc, size) = standard_info(&f)?;
        let n = self.size(win, attrs, alloc, size)?;
        self.print(n, disp);
        Some(n)
    }

    /// A directory's total; prints it and, per the options, what's below it. `None` on anything
    /// GNU du should handle.
    fn dir(&mut self, disp: &str, win: &Path, level: usize) -> Option<u64> {
        let f = open(win, 0x1 | 0x80, 0x0200_0000)?; // FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, BACKUP_SEMANTICS
        let ((vol, id), _) = handle_id(&f)?;
        if self.hash_all && !self.seen.insert((vol, id)) {
            return Some(0);
        }
        let (alloc, _) = standard_info(&f)?;
        // The runtime reports a directory's size as 0 and its blocks from its allocation.
        let mut sum = if self.apparent { 0 } else { alloc.div_ceil(1024) * 1024 };
        let entries = list(&f)?;
        drop(f);
        for e in entries {
            let path = if disp.ends_with('/') { format!("{disp}{}", e.name) } else { format!("{disp}/{}", e.name) };
            let child = win.join(&e.name);
            if e.attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                // The runtime takes symlinks and junctions as symlinks: 0 blocks, not followed.
                if self.apparent || (e.tag != IO_REPARSE_TAG_SYMLINK && e.tag != IO_REPARSE_TAG_MOUNT_POINT) {
                    return None;
                }
                if !self.hash_all || self.seen.insert((vol, e.id)) {
                    self.leaf(0, &path, level + 1);
                }
            } else if e.attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
                sum += self.dir(&path, &child, level + 1)?;
            } else if self.seen.insert((vol, e.id)) {
                let n = self.size(&child, e.attrs, e.alloc, e.size)?;
                sum += n;
                self.leaf(n, &path, level + 1);
            }
        }
        if level <= self.max_depth {
            self.print(sum, disp);
        }
        Some(sum)
    }

    fn leaf(&mut self, n: u64, path: &str, level: usize) {
        if self.all && level <= self.max_depth {
            self.print(n, path);
        }
    }

    /// A file's size in bytes: its length, or its blocks as `fstat_helper` counts them (the
    /// runtime's `S_BLKSIZE` is 1024).
    fn size(&self, win: &Path, attrs: u32, alloc: u64, size: u64) -> Option<u64> {
        if self.apparent {
            // The runtime reports its own symlinks (system files) with the target's length.
            return (attrs & FILE_ATTRIBUTE_SYSTEM == 0).then_some(size);
        }
        let bytes = if size == 0 && alloc == 0 {
            0
        } else if alloc > 0 {
            alloc
        } else {
            compressed_size(win)?
        };
        Some(bytes.div_ceil(1024) * 1024)
    }

    fn print(&mut self, n: u64, path: &str) {
        let s = if self.human { human(n, 1, true) } else { human(n, self.block, false) };
        self.out.extend_from_slice(s.as_bytes());
        self.out.push(b'\t');
        self.out.extend_from_slice(path.as_bytes());
        self.out.push(if self.null { 0 } else { b'\n' });
    }
}

struct Entry {
    name: String,
    attrs: u32,
    tag: u32,
    size: u64,
    alloc: u64,
    id: u64,
}

fn open(p: &Path, access: u32, flags: u32) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .access_mode(access | 0x0010_0000) // SYNCHRONIZE
        .share_mode(7)
        .custom_flags(flags | 0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
        .open(p)
        .ok()
}

/// ((volume serial, file id), attributes)
fn handle_id(f: &std::fs::File) -> Option<((u32, u64), u32)> {
    let mut i = sys::BY_HANDLE_FILE_INFORMATION::default();
    (unsafe { sys::GetFileInformationByHandle(f.as_raw_handle() as _, &mut i) } != 0)
        .then(|| ((i.dwVolumeSerialNumber, (i.nFileIndexHigh as u64) << 32 | i.nFileIndexLow as u64), i.dwFileAttributes))
}

/// (allocation size, end of file)
fn standard_info(f: &std::fs::File) -> Option<(u64, u64)> {
    let mut b = [0u64; 3];
    let ok = unsafe { sys::GetFileInformationByHandleEx(f.as_raw_handle() as _, FILE_STANDARD_INFO, b.as_mut_ptr().cast(), 24) };
    (ok != 0).then(|| (b[0], b[1]))
}

fn compressed_size(p: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let w: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
    if w.len() > 260 {
        return None;
    }
    let mut high = 0;
    let low = unsafe { sys::GetCompressedFileSizeW(w.as_ptr(), &mut high) };
    if low == u32::MAX && unsafe { sys::GetLastError() } != 0 {
        return None;
    }
    Some((high as u64) << 32 | low as u64)
}

/// A directory's entries in the order the file system lists them, as the runtime's readdir
/// does, without `.` and `..`.
fn list(f: &std::fs::File) -> Option<Vec<Entry>> {
    let mut buf = vec![0u64; 8192]; // 64 KiB, 8-byte aligned
    let mut out = Vec::new();
    let mut class = FILE_ID_BOTH_DIRECTORY_RESTART_INFO;
    loop {
        let ok = unsafe { sys::GetFileInformationByHandleEx(f.as_raw_handle() as _, class, buf.as_mut_ptr().cast(), 65536) };
        if ok == 0 {
            return (unsafe { sys::GetLastError() } == 18).then_some(out); // ERROR_NO_MORE_FILES
        }
        class = FILE_ID_BOTH_DIRECTORY_INFO;
        let b: &[u8] = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), 65536) };
        let rd32 = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let rd64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let mut o = 0;
        loop {
            let len = rd32(o + 60) as usize;
            let name: Vec<u16> = b[o + 104..o + 104 + len].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            // The runtime stores characters Windows forbids in names as U+F000 + the character.
            let name: String = char::decode_utf16(name)
                .map(|c| c.map(|c| if ('\u{f000}'..'\u{f080}').contains(&c) { char::from((c as u32 - 0xf000) as u8) } else { c }))
                .collect::<Result<_, _>>()
                .ok()?;
            if name != "." && name != ".." {
                let attrs = rd32(o + 56);
                out.push(Entry { name, attrs, tag: rd32(o + 64), size: rd64(o + 40), alloc: rd64(o + 48), id: rd64(o + 96) });
            }
            match rd32(o) as usize {
                0 => break,
                next => o += next,
            }
        }
    }
}

/// gnulib's human_readable (n, opts, 1, to) as du calls it: rounding up; `autoscale` (`-h`)
/// scales by 1024 with a suffix letter and one decimal below 10.
fn human(n: u64, to: u64, autoscale: bool) -> String {
    let (mut amt, mut tenths, mut rounding) = if to <= 1 {
        (n, 0, 0)
    } else {
        let (r10, r2) = ((n % to) * 10, ((n % to) * 10 % to) * 2);
        (n / to, r10 / to, if r2 < to { (r2 > 0) as u64 } else { 2 + (to < r2) as u64 })
    };
    let (mut exp, mut point) = (0, None);
    if autoscale && amt >= 1024 {
        loop {
            let r10 = (amt % 1024) * 10 + tenths;
            let r2 = (r10 % 1024) * 2 + (rounding >> 1);
            amt /= 1024;
            tenths = r10 / 1024;
            rounding = if r2 < 1024 { (r2 + rounding != 0) as u64 } else { 2 + (1024 < r2 + rounding) as u64 };
            exp += 1;
            if amt < 1024 || exp == LETTERS.len() - 1 {
                break;
            }
        }
        if amt < 10 {
            if rounding > 0 {
                tenths += 1;
                rounding = 0;
                if tenths == 10 {
                    amt += 1;
                    tenths = 0;
                }
            }
            if amt < 10 {
                point = Some(tenths);
                tenths = 0;
                rounding = 0;
            }
        }
    }
    if tenths + rounding > 0 {
        amt += 1;
        if autoscale && amt == 1024 && exp < LETTERS.len() - 1 {
            exp += 1;
            point = Some(0);
            amt = 1;
        }
    }
    let mut s = amt.to_string();
    if let Some(t) = point {
        s.push('.');
        s.push(char::from(b'0' + t as u8));
    }
    if exp > 0 {
        s.push(LETTERS[exp]);
    }
    s
}

