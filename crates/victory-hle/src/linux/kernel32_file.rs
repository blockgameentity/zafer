//! kernel32 file handles at host level: `CreateFile*`, `ReadFile` and
//! `WriteFile`, the file pointer, sizes, information and times, the
//! standard handles and the console entries. Names by path are
//! `kernel32_path.rs`; completion ports and `GetOverlappedResult` are
//! `kernel32_io.rs`.
//!
//! Paths come as host paths spelled the Windows way ([`host_path`]): `\`
//! and `/` both separate, a leading one is the host root, no drive letter
//! (one is dropped: `X:\a` is `/a`) means the current directory. A name
//! the host does not have as written is looked up without regard to case,
//! one component at a time, as NTFS would find it. Paths we hand out are
//! the same host paths with `\` ([`guest_path`]). Narrow (`A`) names are
//! UTF-8.
//!
//! `OVERLAPPED` transfers complete before the call returns: `Internal`
//! gets the status, `InternalHigh` the count, the event is set, a port the
//! file is associated with gets its packet (unless the event's low bit is
//! set), and the call returns TRUE, which Windows also answers for an
//! operation that completes at once. A read at or past the end is FALSE
//! with `ERROR_HANDLE_EOF`, as on Windows.
// ponytail: synchronous completion blocks the caller for the transfer; a
// worker or io_uring when a title needs reads in flight while it works.
//!
//! Share modes are not enforced (POSIX has none): no
//! `ERROR_SHARING_VIOLATION`, and an open file can be deleted or renamed.

#![allow(non_snake_case)] // named like the exports

use std::ffi::{c_char, c_void};
use std::fs::Metadata;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use victory_loader::linux::teb::set_last_error;

use super::handles::{self, Handle, Object, Waitable};
use super::kernel32_io::{Overlapped, Port};
use crate::HleModule;
use crate::hle_export;

pub(crate) const ERROR_SUCCESS: u32 = 0;
pub(crate) const ERROR_INVALID_FUNCTION: u32 = 1;
pub(crate) const ERROR_FILE_NOT_FOUND: u32 = 2;
pub(crate) const ERROR_PATH_NOT_FOUND: u32 = 3;
pub(crate) const ERROR_TOO_MANY_OPEN_FILES: u32 = 4;
pub(crate) const ERROR_ACCESS_DENIED: u32 = 5;
pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
pub(crate) const ERROR_NOT_SAME_DEVICE: u32 = 17;
pub(crate) const ERROR_NO_MORE_FILES: u32 = 18;
pub(crate) const ERROR_WRITE_PROTECT: u32 = 19;
pub(crate) const ERROR_GEN_FAILURE: u32 = 31;
pub(crate) const ERROR_HANDLE_EOF: u32 = 38;
pub(crate) const ERROR_BAD_NETPATH: u32 = 53;
pub(crate) const ERROR_FILE_EXISTS: u32 = 80;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
pub(crate) const ERROR_DISK_FULL: u32 = 112;
pub(crate) const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
pub(crate) const ERROR_NEGATIVE_SEEK: u32 = 131;
pub(crate) const ERROR_DIR_NOT_EMPTY: u32 = 145;
pub(crate) const ERROR_BUSY: u32 = 170;
pub(crate) const ERROR_ALREADY_EXISTS: u32 = 183;
pub(crate) const ERROR_FILENAME_EXCED_RANGE: u32 = 206;
pub(crate) const ERROR_DIRECTORY: u32 = 267;
pub(crate) const ERROR_CANT_RESOLVE_FILENAME: u32 = 1921;

pub(crate) const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

pub(crate) const GENERIC_READ: u32 = 0x8000_0000;
pub(crate) const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const FILE_READ_DATA: u32 = 0x1;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;

pub(crate) const CREATE_NEW: u32 = 1;
pub(crate) const CREATE_ALWAYS: u32 = 2;
pub(crate) const OPEN_EXISTING: u32 = 3;
pub(crate) const OPEN_ALWAYS: u32 = 4;
pub(crate) const TRUNCATE_EXISTING: u32 = 5;

pub(crate) const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub(crate) const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
pub(crate) const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
pub(crate) const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
pub(crate) const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

pub(crate) const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
pub(crate) const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
pub(crate) const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
pub(crate) const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

const STATUS_SUCCESS: u32 = 0;
pub(crate) const STATUS_PENDING: u32 = 0x103;
pub(crate) const STATUS_END_OF_FILE: u32 = 0xC000_0011;
const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;

const FILE_TYPE_UNKNOWN: u32 = 0;
const FILE_TYPE_DISK: u32 = 1;
const FILE_TYPE_CHAR: u32 = 2;
const FILE_TYPE_PIPE: u32 = 3;

/// Fails with `error` as the last error.
pub(crate) fn fail<T: From<u8>>(error: u32) -> T {
    set_last_error(error);
    T::from(0)
}

// ---- guest text and paths -------------------------------------------------------------------

/// The NUL-terminated UTF-16 string at `p`; `None` for null.
pub(crate) fn wide<'a>(p: *const u16) -> Option<&'a [u16]> {
    if p.is_null() {
        return None;
    }
    let mut n = 0;
    // SAFETY: the caller passes a NUL-terminated string.
    while unsafe { p.add(n).read() } != 0 {
        n += 1;
    }
    // SAFETY: `n` units before the terminator.
    Some(unsafe { std::slice::from_raw_parts(p, n) })
}

/// The NUL-terminated byte string at `p`; `None` for null.
pub(crate) fn narrow<'a>(p: *const c_char) -> Option<&'a [u8]> {
    // SAFETY: the caller passes a NUL-terminated string.
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes())
}

pub(crate) fn wide_name(p: *const u16) -> Option<String> {
    wide(p).map(String::from_utf16_lossy)
}

pub(crate) fn narrow_name(p: *const c_char) -> Option<String> {
    narrow(p).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// The host path a Windows-spelled host path names, `.` and `..` resolved
/// lexically, as Win32 does, and each component found without regard to
/// case when the host lacks it as written. Trailing dots and spaces of
/// the last component go, except after `\\?\`.
pub(crate) fn host_path(name: &str) -> Result<PathBuf, u32> {
    let mut s = name;
    let verbatim = ["\\\\?\\", "\\??\\", "//?/"]
        .iter()
        .any(|p| s.starts_with(p));
    if verbatim {
        s = &s[4..];
    }
    if s.is_empty() {
        return Err(ERROR_PATH_NOT_FOUND);
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        s = &s[2..];
    }
    let sep = |c: char| c == '\\' || c == '/';
    if s.len() >= 2 && s.chars().take(2).all(sep) {
        return Err(ERROR_BAD_NETPATH); // \\server\share
    }
    let mut out = if s.starts_with(sep) {
        PathBuf::from("/")
    } else {
        std::env::current_dir().map_err(|_| ERROR_PATH_NOT_FOUND)?
    };
    let parts: Vec<&str> = s.split(sep).filter(|p| !p.is_empty()).collect();
    for (i, &part) in parts.iter().enumerate() {
        let part = if verbatim || i + 1 < parts.len() || part == "." || part == ".." {
            part
        } else {
            part.trim_end_matches([' ', '.'])
        };
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    Ok(resolve(out))
}

/// `path` with each component spelled as its folder has it, where the
/// host has it only in another case. Components that are not there are
/// kept as written, so the path is also the one a create would use.
// ponytail: one folder listing per miss; a cache keyed by folder when a title opens many miscased names.
fn resolve(path: PathBuf) -> PathBuf {
    if path.symlink_metadata().is_ok() {
        return path;
    }
    let mut out = PathBuf::from("/");
    let mut found = true;
    for part in path.iter().skip(1) {
        if found {
            let direct = out.join(part);
            if direct.symlink_metadata().is_ok() {
                out = direct;
                continue;
            }
            let wanted = part.to_string_lossy().to_lowercase();
            let entry = std::fs::read_dir(&out).ok().and_then(|d| {
                d.flatten()
                    .find(|e| e.file_name().to_string_lossy().to_lowercase() == wanted)
            });
            if let Some(entry) = entry {
                out.push(entry.file_name());
                continue;
            }
            found = false;
        }
        out.push(part);
    }
    out
}

/// A host path as we hand it to title code: `\` separators.
pub(crate) fn guest_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\")
}

/// Copies `text` and a NUL to `buffer` of `capacity` units, the rule of
/// `GetCurrentDirectory` and friends: the length without the NUL, or the
/// size needed with it when it does not fit.
pub(crate) fn deliver<T: Copy + Default>(text: &[T], buffer: *mut T, capacity: u32) -> u32 {
    let need = text.len() + 1;
    if buffer.is_null() || (capacity as usize) < need {
        return need as u32;
    }
    // SAFETY: the caller's buffer holds `capacity` >= `need` units.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer, text.len());
        buffer.add(text.len()).write(T::default());
    }
    text.len() as u32
}

// ---- errors and times -----------------------------------------------------------------------

/// The Win32 code of a failed host call.
pub(crate) fn win32_error(e: &std::io::Error) -> u32 {
    match e.raw_os_error().unwrap_or(0) {
        libc::ENOENT => ERROR_FILE_NOT_FOUND,
        libc::ENOTDIR => ERROR_PATH_NOT_FOUND,
        libc::EACCES | libc::EPERM | libc::EISDIR => ERROR_ACCESS_DENIED,
        libc::EEXIST => ERROR_FILE_EXISTS,
        libc::ENOTEMPTY => ERROR_DIR_NOT_EMPTY,
        libc::ENOSPC | libc::EDQUOT => ERROR_DISK_FULL,
        libc::EBADF => ERROR_INVALID_HANDLE,
        libc::EINVAL => ERROR_INVALID_PARAMETER,
        libc::EXDEV => ERROR_NOT_SAME_DEVICE,
        libc::ENAMETOOLONG => ERROR_FILENAME_EXCED_RANGE,
        libc::EMFILE | libc::ENFILE => ERROR_TOO_MANY_OPEN_FILES,
        libc::ENOMEM => ERROR_NOT_ENOUGH_MEMORY,
        libc::EROFS => ERROR_WRITE_PROTECT,
        libc::EBUSY | libc::ETXTBSY => ERROR_BUSY,
        libc::ELOOP => ERROR_CANT_RESOLVE_FILENAME,
        _ => ERROR_GEN_FAILURE,
    }
}

/// As [`win32_error`] for a call on `path`: a missing name is
/// `ERROR_FILE_NOT_FOUND` when its folder exists, else
/// `ERROR_PATH_NOT_FOUND`.
pub(crate) fn path_error(e: &std::io::Error, path: &Path) -> u32 {
    match win32_error(e) {
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
            if path.parent().is_some_and(Path::is_dir) {
                ERROR_FILE_NOT_FOUND
            } else {
                ERROR_PATH_NOT_FOUND
            }
        }
        code => code,
    }
}

/// `FILETIME`: 100 ns ticks since 1601, as two halves (4-byte aligned).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FileTime {
    pub low: u32,
    pub high: u32,
}

const EPOCH_1601: u64 = 116_444_736_000_000_000;

impl FileTime {
    pub(crate) fn ticks(self) -> u64 {
        u64::from(self.high) << 32 | u64::from(self.low)
    }

    pub(crate) fn from_ticks(t: u64) -> FileTime {
        FileTime {
            low: t as u32,
            high: (t >> 32) as u32,
        }
    }

    pub(crate) fn from_system(time: SystemTime) -> FileTime {
        let ticks = match time.duration_since(UNIX_EPOCH) {
            Ok(d) => EPOCH_1601 + d.as_secs() * 10_000_000 + u64::from(d.subsec_nanos() / 100),
            Err(e) => EPOCH_1601.saturating_sub(e.duration().as_nanos() as u64 / 100),
        };
        FileTime::from_ticks(ticks)
    }

    pub(crate) fn to_system(self) -> SystemTime {
        let t = self.ticks();
        let ns = |d: u64| Duration::from_nanos(d.saturating_mul(100));
        if t >= EPOCH_1601 {
            UNIX_EPOCH + ns(t - EPOCH_1601)
        } else {
            UNIX_EPOCH - ns(EPOCH_1601 - t)
        }
    }
}

/// What Win32 reports of an entry: `DIRECTORY` for a folder, `ARCHIVE`
/// for a file, `READONLY` for one without an owner write bit,
/// `REPARSE_POINT` for a symbolic link (of `symlink_metadata`).
pub(crate) fn attributes(m: &Metadata) -> u32 {
    let mut a = if m.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if !m.is_dir() && m.mode() & 0o200 == 0 {
        a |= FILE_ATTRIBUTE_READONLY;
    }
    if m.file_type().is_symlink() {
        a |= FILE_ATTRIBUTE_REPARSE_POINT;
    }
    a
}

/// Creation, last access and last write times. Creation is the birth
/// time where the file system keeps one, else the write time.
pub(crate) fn times(m: &Metadata) -> [FileTime; 3] {
    let write = m.modified().unwrap_or(UNIX_EPOCH);
    [
        FileTime::from_system(m.created().unwrap_or(write)),
        FileTime::from_system(m.accessed().unwrap_or(write)),
        FileTime::from_system(write),
    ]
}

// ---- the file object ------------------------------------------------------------------------

/// A file, folder, pipe or terminal handle. Waitable and always signalled,
/// as a file without I/O in flight is. Two handles to one object
/// (`DuplicateHandle`) share the file position, which is the fd's own.
pub(crate) struct File {
    file: ManuallyDrop<std::fs::File>,
    owned: bool,
    access: u32,
    flags: u32,
    kind: u32,
    directory: bool,
    path: PathBuf,
    delete_on_close: AtomicBool,
    /// The completion port and key `CreateIoCompletionPort` associated.
    pub(crate) port: Mutex<Option<(Arc<Port>, usize)>>,
}

impl File {
    fn new(file: std::fs::File, owned: bool, access: u32, flags: u32, path: PathBuf) -> File {
        let (kind, directory) = match file.metadata() {
            Ok(m) => {
                let t = m.file_type();
                let kind = if t.is_char_device() {
                    FILE_TYPE_CHAR
                } else if t.is_fifo() || t.is_socket() {
                    FILE_TYPE_PIPE
                } else {
                    FILE_TYPE_DISK
                };
                (kind, t.is_dir())
            }
            Err(_) => (FILE_TYPE_UNKNOWN, false),
        };
        File {
            file: ManuallyDrop::new(file),
            owned,
            access,
            flags,
            kind,
            directory,
            path,
            delete_on_close: AtomicBool::new(flags & FILE_FLAG_DELETE_ON_CLOSE != 0),
            port: Mutex::new(None),
        }
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn can_read(&self) -> bool {
        self.access & (GENERIC_READ | GENERIC_ALL | MAXIMUM_ALLOWED | FILE_READ_DATA) != 0
    }

    fn can_write(&self) -> bool {
        self.access
            & (GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED | FILE_WRITE_DATA | FILE_APPEND_DATA)
            != 0
    }

    pub(crate) fn overlapped(&self) -> bool {
        self.flags & FILE_FLAG_OVERLAPPED != 0
    }

    /// One read or write; at `offset` (positional) or at the file pointer.
    /// A regular file is read until full or at its end; a pipe or terminal
    /// hands over what it has.
    fn transfer(&self, write: bool, buf: &mut [u8], offset: Option<u64>) -> std::io::Result<usize> {
        if !write {
            // The kernel writes the buffer, and fails on blocks the GPU side guards.
            victory_gpu::kernel_will_write(buf.as_ptr() as usize, buf.len());
        }
        let mut file = &*self.file;
        let mut done = 0;
        while done < buf.len() {
            let chunk = &mut buf[done..];
            let n = match (write, offset) {
                (false, Some(at)) => file.read_at(chunk, at + done as u64),
                (true, Some(at)) => file.write_at(chunk, at + done as u64),
                (false, None) => file.read(chunk),
                (true, None) => file.write(chunk),
            };
            let n = match n {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break, // a partial transfer is a success with its count
            };
            done += n;
            if n == 0 || (!write && self.kind != FILE_TYPE_DISK) {
                break;
            }
        }
        Ok(done)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        if self.delete_on_close.load(Ordering::Relaxed) {
            let _ = if self.directory {
                std::fs::remove_dir(&self.path)
            } else {
                std::fs::remove_file(&self.path)
            };
        }
        if self.owned {
            // SAFETY: dropped once, here.
            unsafe { ManuallyDrop::drop(&mut self.file) };
        }
    }
}

impl Object for File {
    fn as_waitable(&self) -> Option<&dyn Waitable> {
        Some(self)
    }
}

impl Waitable for File {
    fn try_acquire(&self) -> bool {
        true
    }
}

/// The file behind `handle`; `ERROR_INVALID_HANDLE` set when there is none.
pub(crate) fn file(handle: *mut c_void) -> Option<Arc<File>> {
    let file = handles::get::<File>(handle as Handle);
    if file.is_none() {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    file
}

/// A handle to a host fd the caller owns or lends (`owned`: closed with
/// the last handle). For the UCRT's `_get_osfhandle` and the like.
pub(crate) fn handle_from_fd(fd: RawFd, access: u32, owned: bool) -> *mut c_void {
    // SAFETY: the caller owns or lends `fd`; a lent one is never closed here.
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    let path = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap_or_default();
    handles::insert(Arc::new(File::new(file, owned, access, 0, path))) as *mut c_void
}

/// `CreateFile` at host level: a handle, or `INVALID_HANDLE_VALUE` with
/// the last error set. `ERROR_ALREADY_EXISTS` or 0 after `CREATE_ALWAYS`
/// and `OPEN_ALWAYS`, as Win32 does.
pub(crate) fn create(name: &str, access: u32, disposition: u32, flags: u32) -> *mut c_void {
    match open(name, access, disposition, flags) {
        Ok((file, existed)) => {
            if disposition == CREATE_ALWAYS || disposition == OPEN_ALWAYS {
                set_last_error(if existed {
                    ERROR_ALREADY_EXISTS
                } else {
                    ERROR_SUCCESS
                });
            }
            handles::insert(Arc::new(file)) as *mut c_void
        }
        Err(e) => {
            set_last_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

fn open(name: &str, access: u32, disposition: u32, flags: u32) -> Result<(File, bool), u32> {
    let sep = |c: char| c == '\\' || c == '/';
    let device = name.len() >= 4
        && name.starts_with(sep)
        && name[1..].starts_with(sep)
        && name[2..].starts_with(".")
        && name[3..].starts_with(sep);
    let path =
        if name.eq_ignore_ascii_case("nul") || (device && name[4..].eq_ignore_ascii_case("nul")) {
            PathBuf::from("/dev/null")
        } else if device {
            // No pipe server and no device namespace: "\\.\pipe\x" is a pipe nobody made.
            return Err(ERROR_FILE_NOT_FOUND);
        } else {
            host_path(name)?
        };
    let read = access & (GENERIC_READ | GENERIC_ALL | MAXIMUM_ALLOWED | FILE_READ_DATA) != 0;
    let write = access & (GENERIC_WRITE | GENERIC_ALL | MAXIMUM_ALLOWED | FILE_WRITE_DATA) != 0;
    let append = access & FILE_APPEND_DATA != 0 && !write;
    // The disposition as open(2) flags: std refuses O_CREAT without write
    // access, which Windows allows.
    let mut custom = libc::O_CLOEXEC
        | match disposition {
            CREATE_NEW => libc::O_CREAT | libc::O_EXCL,
            CREATE_ALWAYS => libc::O_CREAT | libc::O_TRUNC,
            OPEN_EXISTING => 0,
            OPEN_ALWAYS => libc::O_CREAT,
            TRUNCATE_EXISTING if write => libc::O_TRUNC,
            _ => return Err(ERROR_INVALID_PARAMETER),
        };
    let creates = custom & (libc::O_CREAT | libc::O_TRUNC) != 0;
    if flags & FILE_FLAG_WRITE_THROUGH != 0 {
        custom |= libc::O_DSYNC;
    }
    let meta = std::fs::metadata(&path);
    let existed = meta.is_ok();
    let mut options = std::fs::OpenOptions::new();
    if meta.as_ref().is_ok_and(Metadata::is_dir) {
        if disposition == CREATE_NEW {
            return Err(ERROR_FILE_EXISTS);
        }
        if flags & FILE_FLAG_BACKUP_SEMANTICS == 0 || write || append || creates {
            return Err(ERROR_ACCESS_DENIED);
        }
        options.read(true);
        custom = libc::O_CLOEXEC | libc::O_DIRECTORY;
    } else {
        options
            .read(read || (!write && !append))
            .write(write)
            .append(append);
        if !read && !write && !append && !creates {
            // Attributes only: no data access, as Windows allows.
            custom |= libc::O_PATH;
        }
    }
    options.custom_flags(custom);
    if flags & FILE_ATTRIBUTE_READONLY != 0 {
        options.mode(0o444);
    }
    let file = options.open(&path).map_err(|e| path_error(&e, &path))?;
    let advice = if flags & FILE_FLAG_SEQUENTIAL_SCAN != 0 {
        libc::POSIX_FADV_SEQUENTIAL
    } else if flags & FILE_FLAG_RANDOM_ACCESS != 0 {
        libc::POSIX_FADV_RANDOM
    } else {
        libc::POSIX_FADV_NORMAL
    };
    if advice != libc::POSIX_FADV_NORMAL {
        // SAFETY: our fd; a hint only.
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, advice) };
    }
    Ok((File::new(file, true, access, flags, path), existed))
}

pub extern "win64" fn CreateFileW(
    name: *const u16,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    match wide_name(name) {
        Some(name) => create(&name, access, disposition, flags),
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            INVALID_HANDLE_VALUE
        }
    }
}

pub extern "win64" fn CreateFileA(
    name: *const c_char,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    match narrow_name(name) {
        Some(name) => create(&name, access, disposition, flags),
        None => {
            set_last_error(ERROR_INVALID_PARAMETER);
            INVALID_HANDLE_VALUE
        }
    }
}

/// `CREATEFILE2_EXTENDED_PARAMETERS`.
#[repr(C)]
pub struct CreateFile2Parameters {
    pub size: u32,
    pub attributes: u32,
    pub flags: u32,
    pub security_qos_flags: u32,
    pub security: *const c_void,
    pub template: *mut c_void,
}

pub extern "win64" fn CreateFile2(
    name: *const u16,
    access: u32,
    _share: u32,
    disposition: u32,
    parameters: *const CreateFile2Parameters,
) -> *mut c_void {
    // SAFETY: null or the caller's parameter block.
    let flags = unsafe { parameters.as_ref() }.map_or(FILE_ATTRIBUTE_NORMAL, |p| {
        p.attributes | p.flags | p.security_qos_flags
    });
    CreateFileW(
        name,
        access,
        0,
        std::ptr::null(),
        disposition,
        flags,
        std::ptr::null_mut(),
    )
}

// ---- ReadFile and WriteFile -----------------------------------------------------------------

/// Sets (`true`) or resets the event an `OVERLAPPED` names; its low bit
/// only says "no completion packet".
fn signal(event: usize, on: bool) {
    if event & !1 != 0
        && let Some(object) = handles::object(event & !1)
    {
        object.set_signal(on);
    }
}

fn read_write(
    write: bool,
    handle: *mut c_void,
    buffer: *mut u8,
    size: u32,
    done_out: *mut u32,
    overlapped: *mut Overlapped,
) -> i32 {
    if !done_out.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { done_out.write(0) };
    }
    let Some(file) = file(handle) else {
        return 0;
    };
    if file.directory {
        return fail(ERROR_INVALID_FUNCTION);
    }
    if !(if write {
        file.can_write()
    } else {
        file.can_read()
    }) {
        return fail(ERROR_ACCESS_DENIED);
    }
    // SAFETY: null or the caller's OVERLAPPED, which outlives the call.
    let ov = unsafe { overlapped.as_mut() };
    if ov.is_none() && file.overlapped() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let buf: &mut [u8] = if size == 0 || buffer.is_null() {
        &mut []
    } else {
        // SAFETY: the caller's buffer of `size` bytes.
        unsafe { std::slice::from_raw_parts_mut(buffer, size as usize) }
    };
    let Some(ov) = ov else {
        return match file.transfer(write, buf, None) {
            Ok(n) => {
                if !done_out.is_null() {
                    // SAFETY: the caller's out pointer.
                    unsafe { done_out.write(n as u32) };
                }
                1
            }
            Err(e) => fail(win32_error(&e)),
        };
    };
    ov.internal = STATUS_PENDING as usize;
    ov.internal_high = 0;
    signal(ov.event, false);
    let mut offset = ov.offset();
    if write && offset == u64::MAX {
        // "Write at the end of the file."
        offset = file.file.metadata().map_or(0, |m| m.len());
    }
    let result = file.transfer(write, buf, Some(offset));
    let at_end = matches!(result, Ok(0)) && !write && size != 0;
    let (status, error) = match &result {
        Ok(_) if at_end => (STATUS_END_OF_FILE, ERROR_HANDLE_EOF),
        Ok(_) => (STATUS_SUCCESS, ERROR_SUCCESS),
        Err(e) => (STATUS_UNSUCCESSFUL, win32_error(e)),
    };
    let n = *result.as_ref().unwrap_or(&0);
    if !file.overlapped() && error == ERROR_SUCCESS {
        // A synchronous handle's pointer moves past the transfer.
        // SAFETY: our fd.
        unsafe {
            libc::lseek(
                file.fd(),
                (offset + n as u64) as libc::off_t,
                libc::SEEK_SET,
            )
        };
    }
    ov.internal_high = n;
    ov.internal = status as usize;
    signal(ov.event, true);
    if error != ERROR_SUCCESS {
        return fail(error);
    }
    if ov.event & 1 == 0 {
        let port = file.port.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((port, key)) = port {
            port.post(n as u32, key, overlapped as usize, STATUS_SUCCESS);
        }
    }
    if !done_out.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { done_out.write(n as u32) };
    }
    1
}

pub extern "win64" fn ReadFile(
    handle: *mut c_void,
    buffer: *mut c_void,
    size: u32,
    read: *mut u32,
    overlapped: *mut Overlapped,
) -> i32 {
    read_write(false, handle, buffer.cast(), size, read, overlapped)
}

pub extern "win64" fn WriteFile(
    handle: *mut c_void,
    buffer: *const c_void,
    size: u32,
    written: *mut u32,
    overlapped: *mut Overlapped,
) -> i32 {
    read_write(true, handle, buffer as *mut u8, size, written, overlapped)
}

// ---- the file pointer and the size ----------------------------------------------------------

/// `FILE_BEGIN`, `FILE_CURRENT`, `FILE_END` are `SEEK_SET`, `SEEK_CUR`,
/// `SEEK_END`.
fn seek(handle: *mut c_void, distance: i64, method: u32) -> Option<i64> {
    let file = file(handle)?;
    if method > 2 {
        set_last_error(ERROR_INVALID_PARAMETER);
        return None;
    }
    // SAFETY: our fd.
    let at = unsafe { libc::lseek(file.fd(), distance, method as i32) };
    if at < 0 {
        let e = std::io::Error::last_os_error();
        set_last_error(if e.raw_os_error() == Some(libc::EINVAL) {
            ERROR_NEGATIVE_SEEK
        } else {
            win32_error(&e)
        });
        return None;
    }
    Some(at)
}

pub extern "win64" fn SetFilePointerEx(
    handle: *mut c_void,
    distance: i64,
    new_position: *mut i64,
    method: u32,
) -> i32 {
    let Some(at) = seek(handle, distance, method) else {
        return 0;
    };
    if !new_position.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { new_position.write_unaligned(at) };
    }
    1
}

const INVALID_SET_FILE_POINTER: u32 = u32::MAX;

/// Without `high` the distance is signed 32 bits and the new position must
/// fit in 32 bits, else the pointer stays where it was.
pub extern "win64" fn SetFilePointer(
    handle: *mut c_void,
    distance: i32,
    high: *mut i32,
    method: u32,
) -> u32 {
    let wide = if high.is_null() {
        i64::from(distance)
    } else {
        // SAFETY: the caller's in-out pointer.
        (i64::from(unsafe { high.read() }) << 32) | i64::from(distance as u32)
    };
    let before = if high.is_null() {
        match seek(handle, 0, 1) {
            Some(b) => b,
            None => return INVALID_SET_FILE_POINTER,
        }
    } else {
        0
    };
    let Some(at) = seek(handle, wide, method) else {
        return INVALID_SET_FILE_POINTER;
    };
    if !high.is_null() {
        // SAFETY: as above.
        unsafe { high.write((at >> 32) as i32) };
    } else if at > 0xFFFF_FFFE {
        seek(handle, before, 0);
        set_last_error(ERROR_INVALID_PARAMETER);
        return INVALID_SET_FILE_POINTER;
    }
    // 0xFFFFFFFF can be a valid low half: callers tell by the last error.
    set_last_error(ERROR_SUCCESS);
    at as u32
}

pub extern "win64" fn SetEndOfFile(handle: *mut c_void) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    if !file.can_write() {
        return fail(ERROR_ACCESS_DENIED);
    }
    // SAFETY: our fd.
    let at = unsafe { libc::lseek(file.fd(), 0, libc::SEEK_CUR) };
    match file.file.set_len(at.max(0) as u64) {
        Ok(()) => 1,
        Err(e) => fail(win32_error(&e)),
    }
}

pub extern "win64" fn FlushFileBuffers(handle: *mut c_void) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    if !file.can_write() {
        return fail(ERROR_ACCESS_DENIED);
    }
    match file.file.sync_all() {
        Ok(()) => 1,
        // A pipe or terminal has nothing to flush.
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EROFS)) => 1,
        Err(e) => fail(win32_error(&e)),
    }
}

fn metadata(handle: *mut c_void) -> Option<(Arc<File>, Metadata)> {
    let file = file(handle)?;
    match file.file.metadata() {
        Ok(m) => Some((file, m)),
        Err(e) => {
            set_last_error(win32_error(&e));
            None
        }
    }
}

pub extern "win64" fn GetFileSizeEx(handle: *mut c_void, size: *mut i64) -> i32 {
    if size.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let Some((_, m)) = metadata(handle) else {
        return 0;
    };
    // SAFETY: the caller's out pointer.
    unsafe { size.write_unaligned(if m.is_dir() { 0 } else { m.len() as i64 }) };
    1
}

pub extern "win64" fn GetFileSize(handle: *mut c_void, high: *mut u32) -> u32 {
    let Some((_, m)) = metadata(handle) else {
        return u32::MAX;
    };
    let len = if m.is_dir() { 0 } else { m.len() };
    if !high.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { high.write((len >> 32) as u32) };
    }
    set_last_error(ERROR_SUCCESS);
    len as u32
}

pub extern "win64" fn GetFileType(handle: *mut c_void) -> u32 {
    match file(handle) {
        Some(file) => {
            set_last_error(ERROR_SUCCESS);
            file.kind
        }
        None => FILE_TYPE_UNKNOWN,
    }
}

// ---- information and times ------------------------------------------------------------------

/// `BY_HANDLE_FILE_INFORMATION`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct ByHandleFileInformation {
    pub attributes: u32,
    pub creation: FileTime,
    pub access: FileTime,
    pub write: FileTime,
    pub volume_serial: u32,
    pub size_high: u32,
    pub size_low: u32,
    pub links: u32,
    pub index_high: u32,
    pub index_low: u32,
}

pub extern "win64" fn GetFileInformationByHandle(
    handle: *mut c_void,
    out: *mut ByHandleFileInformation,
) -> i32 {
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let Some((_, m)) = metadata(handle) else {
        return 0;
    };
    let [creation, access, write] = times(&m);
    let size = if m.is_dir() { 0 } else { m.len() };
    let info = ByHandleFileInformation {
        attributes: attributes(&m),
        creation,
        access,
        write,
        volume_serial: m.dev() as u32,
        size_high: (size >> 32) as u32,
        size_low: size as u32,
        links: m.nlink() as u32,
        index_high: (m.ino() >> 32) as u32,
        index_low: m.ino() as u32,
    };
    // SAFETY: the caller's out pointer.
    unsafe { out.write_unaligned(info) };
    1
}

const FILE_BASIC_INFO: u32 = 0;
const FILE_STANDARD_INFO: u32 = 1;
const FILE_NAME_INFO: u32 = 2;
const FILE_DISPOSITION_INFO: u32 = 4;
const FILE_ALLOCATION_INFO: u32 = 5;
const FILE_END_OF_FILE_INFO: u32 = 6;

/// `FILE_BASIC_INFO`: times as 100 ns ticks.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FileBasicInfo {
    pub creation: i64,
    pub access: i64,
    pub write: i64,
    pub change: i64,
    pub attributes: u32,
}

/// `FILE_STANDARD_INFO`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct FileStandardInfo {
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub links: u32,
    pub delete_pending: u8,
    pub directory: u8,
}

/// Classes `FileBasicInfo`, `FileStandardInfo` and `FileNameInfo`; the
/// others are `ERROR_INVALID_PARAMETER`.
pub extern "win64" fn GetFileInformationByHandleEx(
    handle: *mut c_void,
    class: u32,
    out: *mut c_void,
    size: u32,
) -> i32 {
    let Some((file, m)) = metadata(handle) else {
        return 0;
    };
    let size = size as usize;
    match class {
        FILE_BASIC_INFO if size >= size_of::<FileBasicInfo>() => {
            let [c, a, w] = times(&m).map(|t| t.ticks() as i64);
            let ctime = FileTime::from_system(
                UNIX_EPOCH + Duration::new(m.ctime() as u64, m.ctime_nsec() as u32),
            );
            let info = FileBasicInfo {
                creation: c,
                access: a,
                write: w,
                change: ctime.ticks() as i64,
                attributes: attributes(&m),
            };
            // SAFETY: the caller's buffer, large enough.
            unsafe { out.cast::<FileBasicInfo>().write_unaligned(info) };
        }
        FILE_STANDARD_INFO if size >= size_of::<FileStandardInfo>() => {
            let len = if m.is_dir() { 0 } else { m.len() as i64 };
            let info = FileStandardInfo {
                allocation_size: (m.blocks() * 512) as i64,
                end_of_file: len,
                links: m.nlink() as u32,
                delete_pending: u8::from(file.delete_on_close.load(Ordering::Relaxed)),
                directory: u8::from(m.is_dir()),
            };
            // SAFETY: as above.
            unsafe { out.cast::<FileStandardInfo>().write_unaligned(info) };
        }
        FILE_NAME_INFO if size >= 4 => {
            // The path without a volume, as Windows gives it.
            let name: Vec<u16> = guest_path(&file.path).encode_utf16().collect();
            let bytes = name.len() * 2;
            // SAFETY: the caller's buffer of `size` bytes, at least 4.
            unsafe { out.cast::<u32>().write_unaligned(bytes as u32) };
            if 4 + bytes > size {
                return fail(234); // ERROR_MORE_DATA
            }
            // SAFETY: as above; the name fits.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    name.as_ptr().cast::<u8>(),
                    out.cast::<u8>().add(4),
                    bytes,
                )
            };
        }
        FILE_BASIC_INFO | FILE_STANDARD_INFO | FILE_NAME_INFO => {
            return fail(ERROR_INSUFFICIENT_BUFFER);
        }
        _ => return fail(ERROR_INVALID_PARAMETER),
    }
    1
}

/// Classes `FileBasicInfo` (times; `READONLY`), `FileDispositionInfo`,
/// `FileAllocationInfo` (accepted, nothing to do) and `FileEndOfFileInfo`.
pub extern "win64" fn SetFileInformationByHandle(
    handle: *mut c_void,
    class: u32,
    data: *const c_void,
    size: u32,
) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    let size = size as usize;
    match class {
        FILE_BASIC_INFO if size >= size_of::<FileBasicInfo>() => {
            // SAFETY: the caller's buffer, large enough.
            let info = unsafe { data.cast::<FileBasicInfo>().read_unaligned() };
            let t = |v: i64| FileTime::from_ticks(v as u64);
            if !set_times(&file, t(info.access), t(info.write)) {
                return 0;
            }
            if info.attributes != 0 && !file.directory {
                let result = file.file.metadata().and_then(|m| {
                    let mut p = m.permissions();
                    readonly(&mut p, info.attributes & FILE_ATTRIBUTE_READONLY != 0);
                    file.file.set_permissions(p)
                });
                if let Err(e) = result {
                    return fail(win32_error(&e));
                }
            }
        }
        FILE_DISPOSITION_INFO if size >= 1 => {
            // SAFETY: the caller's buffer, at least one byte.
            let delete = unsafe { data.cast::<u8>().read() } != 0;
            file.delete_on_close.store(delete, Ordering::Relaxed);
        }
        FILE_ALLOCATION_INFO if size >= 8 => {}
        FILE_END_OF_FILE_INFO if size >= 8 => {
            // SAFETY: the caller's buffer, at least 8 bytes.
            let len = unsafe { data.cast::<i64>().read_unaligned() };
            if !file.can_write() {
                return fail(ERROR_ACCESS_DENIED);
            }
            if let Err(e) = file.file.set_len(len.max(0) as u64) {
                return fail(win32_error(&e));
            }
        }
        _ => return fail(ERROR_INVALID_PARAMETER),
    }
    1
}

/// Sets or clears the write bits as `FILE_ATTRIBUTE_READONLY` says.
pub(crate) fn readonly(p: &mut std::fs::Permissions, on: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = p.mode();
    p.set_mode(if on { mode & !0o222 } else { mode | 0o200 });
}

/// Null, 0 and all ones leave a time alone. Creation cannot be set on Linux.
fn set_times(file: &File, access: FileTime, write: FileTime) -> bool {
    let spec = |t: FileTime| {
        let ticks = t.ticks();
        if ticks == 0 || ticks == u64::MAX {
            return libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            };
        }
        let d = t.to_system().duration_since(UNIX_EPOCH).unwrap_or_default();
        libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos().into(),
        }
    };
    let times = [spec(access), spec(write)];
    // SAFETY: our fd and two timespecs.
    if unsafe { libc::futimens(file.fd(), times.as_ptr()) } != 0 {
        set_last_error(win32_error(&std::io::Error::last_os_error()));
        return false;
    }
    true
}

pub extern "win64" fn GetFileTime(
    handle: *mut c_void,
    creation: *mut FileTime,
    access: *mut FileTime,
    write: *mut FileTime,
) -> i32 {
    let Some((_, m)) = metadata(handle) else {
        return 0;
    };
    for (out, value) in [creation, access, write].into_iter().zip(times(&m)) {
        if !out.is_null() {
            // SAFETY: the caller's out pointer.
            unsafe { out.write_unaligned(value) };
        }
    }
    1
}

pub extern "win64" fn SetFileTime(
    handle: *mut c_void,
    _creation: *const FileTime,
    access: *const FileTime,
    write: *const FileTime,
) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    if !file.can_write() && file.access & FILE_WRITE_ATTRIBUTES == 0 {
        return fail(ERROR_ACCESS_DENIED);
    }
    // SAFETY: null or the caller's FILETIMEs.
    let read = |p: *const FileTime| unsafe { p.as_ref() }.copied().unwrap_or_default();
    i32::from(set_times(&file, read(access), read(write)))
}

/// The host's offset from UTC, in seconds, at the Unix time `at`.
fn utc_offset(at: i64) -> i64 {
    let t = at as libc::time_t;
    // SAFETY: an all-zero tm is valid; localtime_r fills it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointers.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

fn unix_seconds(t: FileTime) -> i64 {
    (t.ticks() as i64 - EPOCH_1601 as i64) / 10_000_000
}

fn shift(t: FileTime, seconds: i64) -> FileTime {
    FileTime::from_ticks((t.ticks() as i64 + seconds * 10_000_000) as u64)
}

pub extern "win64" fn FileTimeToLocalFileTime(utc: *const FileTime, local: *mut FileTime) -> i32 {
    if utc.is_null() || local.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: the caller's FILETIMEs.
    unsafe {
        let t = utc.read_unaligned();
        local.write_unaligned(shift(t, utc_offset(unix_seconds(t))));
    }
    1
}

pub extern "win64" fn LocalFileTimeToFileTime(local: *const FileTime, utc: *mut FileTime) -> i32 {
    if utc.is_null() || local.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    // SAFETY: the caller's FILETIMEs.
    unsafe {
        let t = local.read_unaligned();
        utc.write_unaligned(shift(t, -utc_offset(unix_seconds(t))));
    }
    1
}

const VOLUME_NAME_NONE: u32 = 0x4;

/// The path the fd has now (renames included): `\\?\` and the host path
/// with `\`, or without the prefix for `VOLUME_NAME_NONE`.
pub extern "win64" fn GetFinalPathNameByHandleW(
    handle: *mut c_void,
    buffer: *mut u16,
    capacity: u32,
    flags: u32,
) -> u32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    let path =
        std::fs::read_link(format!("/proc/self/fd/{}", file.fd())).unwrap_or(file.path.clone());
    let mut text = guest_path(&path);
    if flags & VOLUME_NAME_NONE == 0 {
        text.insert_str(0, "\\\\?\\");
    }
    let text: Vec<u16> = text.encode_utf16().collect();
    deliver(&text, buffer, capacity)
}

// ---- standard handles and the console -------------------------------------------------------

const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
const STD_ERROR_HANDLE: u32 = -12i32 as u32;

/// Handles to fds 0, 1 and 2, lent, made on first use.
pub extern "win64" fn GetStdHandle(which: u32) -> *mut c_void {
    static STD: [OnceLock<usize>; 3] = [OnceLock::new(), OnceLock::new(), OnceLock::new()];
    let (fd, access) = match which {
        STD_INPUT_HANDLE => (0, GENERIC_READ),
        STD_OUTPUT_HANDLE => (1, GENERIC_WRITE),
        STD_ERROR_HANDLE => (2, GENERIC_WRITE),
        _ => {
            set_last_error(ERROR_INVALID_HANDLE);
            return INVALID_HANDLE_VALUE;
        }
    };
    *STD[fd as usize].get_or_init(|| handle_from_fd(fd, access, false) as usize) as *mut c_void
}

pub extern "win64" fn WriteConsoleW(
    handle: *mut c_void,
    buffer: *const u16,
    chars: u32,
    written: *mut u32,
    _reserved: *mut c_void,
) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    let text = if buffer.is_null() {
        String::new()
    } else {
        // SAFETY: the caller's `chars` UTF-16 units.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(buffer, chars as usize) })
    };
    let mut bytes = text.into_bytes();
    if let Err(e) = file.transfer(true, &mut bytes, None) {
        return fail(win32_error(&e));
    }
    if !written.is_null() {
        // SAFETY: the caller's out pointer.
        unsafe { written.write(chars) };
    }
    1
}

pub extern "win64" fn WriteConsoleA(
    handle: *mut c_void,
    buffer: *const u8,
    chars: u32,
    written: *mut u32,
    _reserved: *mut c_void,
) -> i32 {
    WriteFile(handle, buffer.cast(), chars, written, std::ptr::null_mut())
}

/// `CONSOLE_SCREEN_BUFFER_INFO`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct ConsoleScreenBufferInfo {
    pub size: [i16; 2],
    pub cursor: [i16; 2],
    pub attributes: u16,
    pub window: [i16; 4],
    pub maximum_window: [i16; 2],
}

/// For a terminal: its size, as the window and buffer. Anything else is
/// `ERROR_INVALID_HANDLE`, as Windows answers for redirected output.
pub extern "win64" fn GetConsoleScreenBufferInfo(
    handle: *mut c_void,
    out: *mut ConsoleScreenBufferInfo,
) -> i32 {
    let Some(file) = file(handle) else {
        return 0;
    };
    // SAFETY: an all-zero winsize is valid; the ioctl fills it.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: our fd and a winsize.
    if out.is_null() || unsafe { libc::ioctl(file.fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
        return fail(ERROR_INVALID_HANDLE);
    }
    let (w, h) = (size.ws_col as i16, size.ws_row as i16);
    let info = ConsoleScreenBufferInfo {
        size: [w, h],
        cursor: [0, 0],
        attributes: 0x07,
        window: [0, 0, w - 1, h - 1],
        maximum_window: [w, h],
    };
    // SAFETY: the caller's out pointer.
    unsafe { out.write_unaligned(info) };
    1
}

/// No console window.
pub extern "win64" fn GetConsoleWindow() -> *mut c_void {
    std::ptr::null_mut()
}

// ---- named pipes ----------------------------------------------------------------------------

/// No process serves a pipe here: none is there to wait for.
pub extern "win64" fn WaitNamedPipeW(_name: *const u16, _timeout: u32) -> i32 {
    fail(ERROR_FILE_NOT_FOUND)
}

/// A host pipe (a redirected standard handle) takes any mode; a file is
/// `ERROR_INVALID_FUNCTION`, as on Windows.
pub extern "win64" fn SetNamedPipeHandleState(
    handle: *mut c_void,
    _mode: *mut u32,
    _max: *mut u32,
    _timeout: *mut u32,
) -> i32 {
    match file(handle) {
        Some(file) if file.kind == FILE_TYPE_PIPE => 1,
        Some(_) => fail(ERROR_INVALID_FUNCTION),
        None => 0,
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-file",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-file-*",
            "api-ms-win-core-console-*",
            "api-ms-win-core-processenvironment-*",
            "api-ms-win-core-namedpipe-*",
        ],
        exports: vec![
            hle_export!(CreateFileW),
            hle_export!(CreateFileA),
            hle_export!(CreateFile2),
            hle_export!(ReadFile),
            hle_export!(WriteFile),
            hle_export!(SetFilePointerEx),
            hle_export!(SetFilePointer),
            hle_export!(SetEndOfFile),
            hle_export!(FlushFileBuffers),
            hle_export!(GetFileSizeEx),
            hle_export!(GetFileSize),
            hle_export!(GetFileType),
            hle_export!(GetFileInformationByHandle),
            hle_export!(GetFileInformationByHandleEx),
            hle_export!(SetFileInformationByHandle),
            hle_export!(GetFileTime),
            hle_export!(SetFileTime),
            hle_export!(FileTimeToLocalFileTime),
            hle_export!(LocalFileTimeToFileTime),
            hle_export!(GetFinalPathNameByHandleW),
            hle_export!(GetStdHandle),
            hle_export!(WriteConsoleW),
            hle_export!(WriteConsoleA),
            hle_export!(GetConsoleScreenBufferInfo),
            hle_export!(GetConsoleWindow),
            hle_export!(WaitNamedPipeW),
            hle_export!(SetNamedPipeHandleState),
        ],
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use victory_loader::linux::teb::last_error;

    pub(crate) fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn open_w(path: &Path, access: u32, disposition: u32, flags: u32) -> *mut c_void {
        let name = w(&guest_path(path));
        CreateFileW(
            name.as_ptr(),
            access,
            0,
            std::ptr::null(),
            disposition,
            flags,
            std::ptr::null_mut(),
        )
    }

    #[test]
    fn layouts_match_windows() {
        assert_eq!(size_of::<ByHandleFileInformation>(), 52);
        assert_eq!(std::mem::offset_of!(ByHandleFileInformation, size_high), 32);
        assert_eq!(size_of::<CreateFile2Parameters>(), 32);
        assert_eq!(size_of::<FileBasicInfo>(), 40);
        assert_eq!(size_of::<FileStandardInfo>(), 24);
        assert_eq!(size_of::<ConsoleScreenBufferInfo>(), 22);
        assert_eq!(size_of::<FileTime>(), 8);
        assert_eq!(align_of::<FileTime>(), 4);
    }

    #[test]
    fn paths_are_read_the_windows_way() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("Data")).unwrap();
        std::fs::write(root.join("Data/Level.BIN"), b"x").unwrap();
        let spelled = format!("{}\\data\\.\\x\\..\\level.bin. ", guest_path(root));
        assert_eq!(host_path(&spelled).unwrap(), root.join("Data/Level.BIN"));
        let unix = format!("{}/DATA/new.txt", root.display());
        assert_eq!(host_path(&unix).unwrap(), root.join("Data/new.txt"));
        assert_eq!(host_path(&format!("Z:{}", guest_path(root))).unwrap(), root);
        assert_eq!(host_path("\\\\server\\share\\x"), Err(ERROR_BAD_NETPATH));
        assert_eq!(host_path(""), Err(ERROR_PATH_NOT_FOUND));
        assert!(host_path("relative").unwrap().is_absolute());
    }

    #[test]
    fn create_dispositions_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let rw = GENERIC_READ | GENERIC_WRITE;
        assert_eq!(open_w(&path, rw, OPEN_EXISTING, 0), INVALID_HANDLE_VALUE);
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);
        assert_eq!(
            open_w(&dir.path().join("no/a.txt"), rw, OPEN_EXISTING, 0),
            INVALID_HANDLE_VALUE
        );
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);

        let h = open_w(&path, rw, CREATE_ALWAYS, 0);
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(last_error(), ERROR_SUCCESS);
        let mut n = 0;
        assert_eq!(
            WriteFile(h, b"hello".as_ptr().cast(), 5, &mut n, std::ptr::null_mut()),
            1
        );
        assert_eq!(n, 5);
        assert!(handles::close(h as Handle));

        assert_eq!(open_w(&path, rw, CREATE_NEW, 0), INVALID_HANDLE_VALUE);
        assert_eq!(last_error(), ERROR_FILE_EXISTS);
        let h = open_w(&path, GENERIC_READ, OPEN_ALWAYS, 0);
        assert_eq!(last_error(), ERROR_ALREADY_EXISTS);
        let mut size = 0i64;
        assert_eq!(GetFileSizeEx(h, &mut size), 1);
        assert_eq!(size, 5);
        assert_eq!(GetFileType(h), FILE_TYPE_DISK);
        assert_eq!(
            WriteFile(h, b"x".as_ptr().cast(), 1, &mut n, std::ptr::null_mut()),
            0
        );
        assert_eq!(last_error(), ERROR_ACCESS_DENIED);
        let mut buf = [0u8; 8];
        assert_eq!(
            ReadFile(h, buf.as_mut_ptr().cast(), 8, &mut n, std::ptr::null_mut()),
            1
        );
        assert_eq!(&buf[..n as usize], b"hello");
        // At the end a synchronous read is TRUE with 0 bytes.
        assert_eq!(
            ReadFile(h, buf.as_mut_ptr().cast(), 8, &mut n, std::ptr::null_mut()),
            1
        );
        assert_eq!(n, 0);
        assert_eq!(SetFilePointer(h, 1, std::ptr::null_mut(), 0), 1);
        let mut at = 0i64;
        assert_eq!(SetFilePointerEx(h, -1, &mut at, 2), 1);
        assert_eq!(at, 4);
        assert_eq!(SetFilePointerEx(h, -10, &mut at, 0), 0);
        assert_eq!(last_error(), ERROR_NEGATIVE_SEEK);

        let mut info = ByHandleFileInformation::default();
        assert_eq!(GetFileInformationByHandle(h, &mut info), 1);
        assert_eq!(
            (info.size_low, info.links, info.attributes),
            (5, 1, FILE_ATTRIBUTE_ARCHIVE)
        );
        let mut name = [0u16; 512];
        let len = GetFinalPathNameByHandleW(h, name.as_mut_ptr(), 512, 0) as usize;
        let name = String::from_utf16_lossy(&name[..len]);
        assert!(
            name.starts_with("\\\\?\\") && name.ends_with("\\a.txt"),
            "{name}"
        );
        assert_eq!(host_path(&name).unwrap(), path.canonicalize().unwrap());
        assert_eq!(
            GetFinalPathNameByHandleW(h, std::ptr::null_mut(), 0, 0) as usize,
            len + 1
        );
        handles::close(h as Handle);

        // Truncation, end of file, delete on close.
        let h = open_w(&path, rw, TRUNCATE_EXISTING, FILE_FLAG_DELETE_ON_CLOSE);
        assert_eq!(GetFileSize(h, std::ptr::null_mut()), 0);
        assert_eq!(SetFilePointer(h, 100, std::ptr::null_mut(), 0), 100);
        assert_eq!(SetEndOfFile(h), 1);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
        let mut std_info = FileStandardInfo::default();
        assert_eq!(
            GetFileInformationByHandleEx(h, FILE_STANDARD_INFO, (&raw mut std_info).cast(), 24),
            1
        );
        assert_eq!((std_info.end_of_file, std_info.delete_pending), (100, 1));
        handles::close(h as Handle);
        assert!(!path.exists());

        // Folders open only with backup semantics.
        assert_eq!(
            open_w(dir.path(), GENERIC_READ, OPEN_EXISTING, 0),
            INVALID_HANDLE_VALUE
        );
        assert_eq!(last_error(), ERROR_ACCESS_DENIED);
        let h = open_w(
            dir.path(),
            GENERIC_READ,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
        );
        assert_ne!(h, INVALID_HANDLE_VALUE);
        assert_eq!(
            ReadFile(h, buf.as_mut_ptr().cast(), 1, &mut n, std::ptr::null_mut()),
            0
        );
        handles::close(h as Handle);

        let pipe = w("\\\\.\\pipe\\nobody");
        let h = CreateFileW(
            pipe.as_ptr(),
            rw,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        assert_eq!(
            (h, last_error()),
            (INVALID_HANDLE_VALUE, ERROR_FILE_NOT_FOUND)
        );
        assert_eq!(WaitNamedPipeW(pipe.as_ptr(), 0), 0);
        assert_eq!(GetFileType(12345 as *mut c_void), FILE_TYPE_UNKNOWN);
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
    }

    #[test]
    fn file_times_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t");
        let h = open_w(&path, GENERIC_WRITE, CREATE_NEW, 0);
        let when = FileTime::from_ticks(EPOCH_1601 + 1_700_000_000 * 10_000_000);
        assert_eq!(SetFileTime(h, std::ptr::null(), std::ptr::null(), &when), 1);
        let mut got = FileTime::default();
        assert_eq!(
            GetFileTime(h, std::ptr::null_mut(), std::ptr::null_mut(), &mut got),
            1
        );
        assert_eq!(got, when);
        let (mut local, mut back) = (FileTime::default(), FileTime::default());
        assert_eq!(FileTimeToLocalFileTime(&when, &mut local), 1);
        assert_eq!(LocalFileTimeToFileTime(&local, &mut back), 1);
        assert_eq!(back, when);
        assert_eq!(FileTime::from_system(when.to_system()), when);
        handles::close(h as Handle);
    }

    #[test]
    fn standard_handles_are_files() {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        assert_eq!(out, GetStdHandle(STD_OUTPUT_HANDLE));
        assert_ne!(GetFileType(out), FILE_TYPE_UNKNOWN);
        let text = w("");
        let mut n = 1;
        assert_eq!(
            WriteConsoleW(out, text.as_ptr(), 0, &mut n, std::ptr::null_mut()),
            1
        );
        assert_eq!(n, 0);
        assert_eq!(GetStdHandle(5), INVALID_HANDLE_VALUE);
        assert!(GetConsoleWindow().is_null());
    }
}
