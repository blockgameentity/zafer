//! kernel32 file management by name at host level: attributes, folder
//! listings (`FindFirstFile`), folders, delete, move, copy and links, and
//! the process paths (`GetFullPathName`, `GetTempPath`, the current
//! directory). Names are read as `kernel32_file::host_path` reads them;
//! paths handed out have `\` separators and no drive.
//!
//! Listings match without regard to case, as Windows does: `*` and `?`,
//! `*.*` is everything. They are a snapshot taken by `FindFirstFile`, in
//! upper-case name order (NTFS's), `.` and `..` first in a folder that is
//! not the root.

#![allow(non_snake_case)] // named like the exports

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use victory_loader::linux::teb::set_last_error;

use super::handles::{self, Handle, Object};
use super::kernel32_file::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_DIRECTORY, ERROR_FILE_EXISTS,
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES,
    ERROR_NOT_SAME_DEVICE, ERROR_PATH_NOT_FOUND, FILE_ATTRIBUTE_READONLY, FileTime,
    INVALID_FILE_ATTRIBUTES, INVALID_HANDLE_VALUE, attributes, deliver, fail, guest_path,
    host_path, narrow_name, path_error, readonly, times, wide_name, win32_error,
};
use crate::HleModule;
use crate::hle_export;

const ERROR_REQUEST_ABORTED: u32 = 1235;

/// The host path of a guest name, or the error to fail with.
fn name(name: Option<String>) -> Result<PathBuf, u32> {
    host_path(&name.ok_or(ERROR_INVALID_PARAMETER)?)
}

/// Runs `f` on the named path: TRUE, or FALSE with its error.
fn by_name(name: Option<String>, f: impl FnOnce(&Path) -> Result<(), u32>) -> i32 {
    match self::name(name).and_then(|p| f(&p)) {
        Ok(()) => 1,
        Err(e) => fail(e),
    }
}

/// Of the entry, following a symbolic link unless it is broken.
fn meta(path: &Path) -> std::io::Result<Metadata> {
    std::fs::metadata(path).or_else(|_| std::fs::symlink_metadata(path))
}

// ---- attributes -----------------------------------------------------------------------------

fn get_attributes(name: Option<String>) -> u32 {
    match self::name(name).and_then(|p| meta(&p).map_err(|e| path_error(&e, &p))) {
        Ok(m) => attributes(&m),
        Err(e) => {
            set_last_error(e);
            INVALID_FILE_ATTRIBUTES
        }
    }
}

pub extern "win64" fn GetFileAttributesW(name: *const u16) -> u32 {
    get_attributes(wide_name(name))
}

pub extern "win64" fn GetFileAttributesA(name: *const c_char) -> u32 {
    get_attributes(narrow_name(name))
}

/// `WIN32_FILE_ATTRIBUTE_DATA`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct FileAttributeData {
    pub attributes: u32,
    pub creation: FileTime,
    pub access: FileTime,
    pub write: FileTime,
    pub size_high: u32,
    pub size_low: u32,
}

fn get_attributes_ex(name: Option<String>, level: u32, out: *mut FileAttributeData) -> i32 {
    if level != 0 || out.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    by_name(name, |p| {
        let m = meta(p).map_err(|e| path_error(&e, p))?;
        let [creation, access, write] = times(&m);
        let size = if m.is_dir() { 0 } else { m.len() };
        let data = FileAttributeData {
            attributes: attributes(&m),
            creation,
            access,
            write,
            size_high: (size >> 32) as u32,
            size_low: size as u32,
        };
        // SAFETY: the caller's out pointer, checked non-null.
        unsafe { out.write_unaligned(data) };
        Ok(())
    })
}

pub extern "win64" fn GetFileAttributesExW(
    name: *const u16,
    level: u32,
    out: *mut FileAttributeData,
) -> i32 {
    get_attributes_ex(wide_name(name), level, out)
}

pub extern "win64" fn GetFileAttributesExA(
    name: *const c_char,
    level: u32,
    out: *mut FileAttributeData,
) -> i32 {
    get_attributes_ex(narrow_name(name), level, out)
}

/// Only `READONLY` means anything on the host, and only for files.
fn set_attributes(name: Option<String>, value: u32) -> i32 {
    by_name(name, |p| {
        let m = std::fs::metadata(p).map_err(|e| path_error(&e, p))?;
        if m.is_dir() {
            return Ok(());
        }
        let mut perms = m.permissions();
        readonly(&mut perms, value & FILE_ATTRIBUTE_READONLY != 0);
        std::fs::set_permissions(p, perms).map_err(|e| win32_error(&e))
    })
}

pub extern "win64" fn SetFileAttributesW(name: *const u16, value: u32) -> i32 {
    set_attributes(wide_name(name), value)
}

pub extern "win64" fn SetFileAttributesA(name: *const c_char, value: u32) -> i32 {
    set_attributes(narrow_name(name), value)
}

// ---- listings -------------------------------------------------------------------------------

/// `WIN32_FIND_DATAW`.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct FindDataW {
    pub attributes: u32,
    pub creation: FileTime,
    pub access: FileTime,
    pub write: FileTime,
    pub size_high: u32,
    pub size_low: u32,
    pub reserved: [u32; 2],
    pub name: [u16; 260],
    pub alternate_name: [u16; 14],
}

impl FindDataW {
    fn new(name: &str, m: &Metadata) -> FindDataW {
        let [creation, access, write] = times(m);
        let size = if m.is_dir() { 0 } else { m.len() };
        let mut data = FindDataW {
            attributes: attributes(m),
            creation,
            access,
            write,
            size_high: (size >> 32) as u32,
            size_low: size as u32,
            reserved: [0; 2],
            name: [0; 260],
            alternate_name: [0; 14],
        };
        for (slot, unit) in data.name[..259].iter_mut().zip(name.encode_utf16()) {
            *slot = unit;
        }
        data
    }
}

/// A `FindFirstFile` handle: what is left of its listing.
struct Find(Mutex<VecDeque<FindDataW>>);

impl Object for Find {}

/// DOS wildcards without regard to case: `*` any run, `?` one character.
pub(crate) fn matches(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (
        pattern.to_lowercase().chars().collect(),
        name.to_lowercase().chars().collect(),
    );
    if p == ['*', '.', '*'] {
        return true;
    }
    // Iterative glob with one backtrack point.
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while j < n.len() {
        if i < p.len() && (p[i] == '?' || p[i] == n[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    p[i..].iter().all(|&c| c == '*')
}

/// The listing of `spec` (a folder and a pattern), folders only when
/// `folders_only`.
fn list(spec: &str, folders_only: bool) -> Result<VecDeque<FindDataW>, u32> {
    let cut = spec.rfind(['\\', '/']);
    let (folder, pattern) = match cut {
        Some(i) => (&spec[..=i], &spec[i + 1..]),
        None => (".", spec),
    };
    if pattern.is_empty() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    let folder = host_path(folder)?;
    let entries = std::fs::read_dir(&folder).map_err(|e| match win32_error(&e) {
        ERROR_FILE_NOT_FOUND => ERROR_PATH_NOT_FOUND,
        e => e,
    })?;
    let mut found: Vec<(String, Metadata)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let m = meta(&e.path()).ok()?;
            matches(pattern, &name).then_some((name, m))
        })
        .filter(|(_, m)| !folders_only || m.is_dir())
        .collect();
    found.sort_by_cached_key(|(name, _)| name.to_uppercase());
    let mut out = VecDeque::new();
    if folder != Path::new("/") {
        for dot in [".", ".."] {
            if matches(pattern, dot)
                && let Ok(m) = std::fs::metadata(folder.join(dot))
            {
                out.push_back(FindDataW::new(dot, &m));
            }
        }
    }
    out.extend(found.iter().map(|(name, m)| FindDataW::new(name, m)));
    if out.is_empty() {
        return Err(ERROR_FILE_NOT_FOUND);
    }
    Ok(out)
}

fn find_first(spec: Option<String>, out: *mut FindDataW, folders_only: bool) -> *mut c_void {
    let result = spec
        .ok_or(ERROR_INVALID_PARAMETER)
        .and_then(|s| list(&s, folders_only));
    match result {
        Ok(mut entries) if !out.is_null() => {
            let first = entries.pop_front().expect("not empty");
            // SAFETY: the caller's WIN32_FIND_DATAW.
            unsafe { out.write_unaligned(first) };
            handles::insert(Arc::new(Find(Mutex::new(entries)))) as *mut c_void
        }
        Ok(_) => {
            set_last_error(ERROR_INVALID_PARAMETER);
            INVALID_HANDLE_VALUE
        }
        Err(e) => {
            set_last_error(e);
            INVALID_HANDLE_VALUE
        }
    }
}

pub extern "win64" fn FindFirstFileW(name: *const u16, out: *mut FindDataW) -> *mut c_void {
    find_first(wide_name(name), out, false)
}

const FIND_EX_SEARCH_LIMIT_TO_DIRECTORIES: u32 = 1;

/// Both info levels fill the same data (the short name stays empty); the
/// folders-only search is honoured.
pub extern "win64" fn FindFirstFileExW(
    name: *const u16,
    _level: u32,
    out: *mut FindDataW,
    search: u32,
    _filter: *mut c_void,
    _flags: u32,
) -> *mut c_void {
    find_first(
        wide_name(name),
        out,
        search == FIND_EX_SEARCH_LIMIT_TO_DIRECTORIES,
    )
}

pub extern "win64" fn FindNextFileW(handle: *mut c_void, out: *mut FindDataW) -> i32 {
    let Some(find) = handles::get::<Find>(handle as Handle) else {
        return fail(ERROR_INVALID_HANDLE);
    };
    let next = find.0.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    match next {
        Some(data) if !out.is_null() => {
            // SAFETY: the caller's WIN32_FIND_DATAW.
            unsafe { out.write_unaligned(data) };
            1
        }
        Some(_) => fail(ERROR_INVALID_PARAMETER),
        None => fail(ERROR_NO_MORE_FILES),
    }
}

pub extern "win64" fn FindClose(handle: *mut c_void) -> i32 {
    if handles::get::<Find>(handle as Handle).is_none() {
        return fail(ERROR_INVALID_HANDLE);
    }
    i32::from(handles::close(handle as Handle))
}

// ---- folders, delete, move, copy, links -----------------------------------------------------

fn create_directory(p: &Path) -> Result<(), u32> {
    std::fs::create_dir(p).map_err(|e| match win32_error(&e) {
        ERROR_FILE_EXISTS => ERROR_ALREADY_EXISTS,
        ERROR_FILE_NOT_FOUND => ERROR_PATH_NOT_FOUND,
        e => e,
    })
}

pub extern "win64" fn CreateDirectoryW(name: *const u16, _security: *const c_void) -> i32 {
    by_name(wide_name(name), create_directory)
}

pub extern "win64" fn CreateDirectoryA(name: *const c_char, _security: *const c_void) -> i32 {
    by_name(narrow_name(name), create_directory)
}

fn remove_directory(p: &Path) -> Result<(), u32> {
    std::fs::remove_dir(p).map_err(|e| match e.raw_os_error() {
        Some(libc::ENOTDIR) if p.exists() => ERROR_DIRECTORY,
        _ => path_error(&e, p),
    })
}

pub extern "win64" fn RemoveDirectoryW(name: *const u16) -> i32 {
    by_name(wide_name(name), remove_directory)
}

pub extern "win64" fn RemoveDirectoryA(name: *const c_char) -> i32 {
    by_name(narrow_name(name), remove_directory)
}

fn delete_file(p: &Path) -> Result<(), u32> {
    if std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
        return Err(ERROR_ACCESS_DENIED);
    }
    std::fs::remove_file(p).map_err(|e| path_error(&e, p))
}

pub extern "win64" fn DeleteFileW(name: *const u16) -> i32 {
    by_name(wide_name(name), delete_file)
}

pub extern "win64" fn DeleteFileA(name: *const c_char) -> i32 {
    by_name(narrow_name(name), delete_file)
}

const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
const MOVEFILE_COPY_ALLOWED: u32 = 0x2;

fn rename(from: &Path, to: &Path, replace: bool) -> std::io::Result<()> {
    if replace {
        return std::fs::rename(from, to);
    }
    use std::os::unix::ffi::OsStrExt;
    let (a, b) = (
        std::ffi::CString::new(from.as_os_str().as_bytes())?,
        std::ffi::CString::new(to.as_os_str().as_bytes())?,
    );
    // SAFETY: two NUL-terminated paths.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
        // A file system without RENAME_NOREPLACE.
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }
        return std::fs::rename(from, to);
    }
    Err(e)
}

fn move_file(from: Option<String>, to: Option<String>, flags: u32) -> i32 {
    let (from, to) = match (name(from), name(to)) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return fail(e),
    };
    let m = match std::fs::symlink_metadata(&from) {
        Ok(m) => m,
        Err(e) => return fail(path_error(&e, &from)),
    };
    let replace = flags & MOVEFILE_REPLACE_EXISTING != 0;
    match rename(&from, &to, replace) {
        Ok(()) => 1,
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            if flags & MOVEFILE_COPY_ALLOWED == 0 || m.is_dir() {
                return fail(ERROR_NOT_SAME_DEVICE);
            }
            match copy(&from, &to, !replace)
                .and_then(|()| std::fs::remove_file(&from).map_err(|e| win32_error(&e)))
            {
                Ok(()) => 1,
                Err(e) => fail(e),
            }
        }
        Err(e) => fail(match win32_error(&e) {
            ERROR_FILE_EXISTS => ERROR_ALREADY_EXISTS,
            _ => path_error(&e, &to),
        }),
    }
}

pub extern "win64" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    move_file(wide_name(from), wide_name(to), flags)
}

pub extern "win64" fn MoveFileW(from: *const u16, to: *const u16) -> i32 {
    move_file(wide_name(from), wide_name(to), 0)
}

/// A copy that keeps the write time, as Windows does.
fn copy(from: &Path, to: &Path, fail_if_exists: bool) -> Result<(), u32> {
    let m = std::fs::metadata(from).map_err(|e| path_error(&e, from))?;
    if m.is_dir() {
        return Err(ERROR_ACCESS_DENIED);
    }
    if fail_if_exists && std::fs::symlink_metadata(to).is_ok() {
        return Err(ERROR_FILE_EXISTS);
    }
    std::fs::copy(from, to).map_err(|e| path_error(&e, to))?;
    if let (Ok(file), Ok(written)) = (std::fs::File::options().write(true).open(to), m.modified()) {
        let _ = file.set_modified(written);
    }
    Ok(())
}

pub extern "win64" fn CopyFileW(from: *const u16, to: *const u16, fail_if_exists: i32) -> i32 {
    match (name(wide_name(from)), name(wide_name(to))) {
        (Ok(f), Ok(t)) => match copy(&f, &t, fail_if_exists != 0) {
            Ok(()) => 1,
            Err(e) => fail(e),
        },
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

/// `COPYFILE2_EXTENDED_PARAMETERS`.
#[repr(C)]
pub struct CopyFile2Parameters {
    pub size: u32,
    pub flags: u32,
    pub cancel: *const i32,
    pub progress: *const c_void,
    pub context: *mut c_void,
}

const COPY_FILE_FAIL_IF_EXISTS: u32 = 0x1;

fn hresult(error: u32) -> i32 {
    if error == 0 {
        0
    } else {
        (0x8007_0000 | (error & 0xFFFF)) as i32
    }
}

/// An HRESULT; the progress routine is not called.
pub extern "win64" fn CopyFile2(
    from: *const u16,
    to: *const u16,
    parameters: *const CopyFile2Parameters,
) -> i32 {
    // SAFETY: null or the caller's parameter block.
    let p = unsafe { parameters.as_ref() };
    let flags = p.map_or(0, |p| p.flags);
    let result = name(wide_name(from)).and_then(|f| {
        let t = name(wide_name(to))?;
        // SAFETY: null or the caller's cancel flag.
        if p.is_some_and(|p| unsafe { p.cancel.as_ref() }.is_some_and(|&c| c != 0)) {
            return Err(ERROR_REQUEST_ABORTED);
        }
        copy(&f, &t, flags & COPY_FILE_FAIL_IF_EXISTS != 0)
    });
    hresult(result.err().unwrap_or(0))
}

pub extern "win64" fn CreateHardLinkW(
    link: *const u16,
    existing: *const u16,
    _security: *const c_void,
) -> i32 {
    match (name(wide_name(link)), name(wide_name(existing))) {
        (Ok(l), Ok(e)) => match std::fs::hard_link(&e, &l) {
            Ok(()) => 1,
            Err(err) => fail(match win32_error(&err) {
                ERROR_FILE_EXISTS => ERROR_ALREADY_EXISTS,
                _ => path_error(&err, &e),
            }),
        },
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

/// A relative target stays relative to the link's folder.
pub extern "win64" fn CreateSymbolicLinkW(link: *const u16, target: *const u16, _flags: u32) -> u8 {
    let (Ok(link), Some(target)) = (name(wide_name(link)), wide_name(target)) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let relative = !target.starts_with(['\\', '/']) && target.as_bytes().get(1) != Some(&b':');
    let target = if relative {
        Ok(PathBuf::from(target.replace('\\', "/")))
    } else {
        host_path(&target)
    };
    match target.map(|t| std::os::unix::fs::symlink(t, &link)) {
        Ok(Ok(())) => 1,
        Ok(Err(e)) => fail(match win32_error(&e) {
            ERROR_FILE_EXISTS => ERROR_ALREADY_EXISTS,
            _ => path_error(&e, &link),
        }),
        Err(e) => fail(e),
    }
}

// ---- process paths --------------------------------------------------------------------------

/// The absolute name, lexically resolved, with `\`.
fn full_path(name: Option<String>) -> Result<String, u32> {
    let name = name.ok_or(ERROR_INVALID_PARAMETER)?;
    let trailing = name.ends_with(['\\', '/']);
    let mut text = guest_path(&host_path(&name)?);
    if trailing && !text.ends_with('\\') {
        text.push('\\');
    }
    Ok(text)
}

/// `GetFullPathName`'s file part: the last component, none after a
/// trailing separator.
fn file_part(text: &[u16]) -> Option<usize> {
    let cut = text
        .iter()
        .rposition(|&c| c == u16::from(b'\\'))
        .map_or(0, |i| i + 1);
    (cut < text.len()).then_some(cut)
}

pub extern "win64" fn GetFullPathNameW(
    name: *const u16,
    capacity: u32,
    buffer: *mut u16,
    part: *mut *mut u16,
) -> u32 {
    let text: Vec<u16> = match full_path(wide_name(name)) {
        Ok(t) => t.encode_utf16().collect(),
        Err(e) => return fail(e),
    };
    let n = deliver(&text, buffer, capacity);
    if !part.is_null() && (n as usize) == text.len() {
        // SAFETY: the caller's out pointer; the part lies in the buffer.
        unsafe { part.write(file_part(&text).map_or(std::ptr::null_mut(), |i| buffer.add(i))) };
    }
    n
}

pub extern "win64" fn GetFullPathNameA(
    name: *const c_char,
    capacity: u32,
    buffer: *mut u8,
    part: *mut *mut u8,
) -> u32 {
    let text = match full_path(narrow_name(name)) {
        Ok(t) => t.into_bytes(),
        Err(e) => return fail(e),
    };
    let n = deliver(&text, buffer, capacity);
    if !part.is_null() && (n as usize) == text.len() {
        let cut = text.iter().rposition(|&c| c == b'\\').map_or(0, |i| i + 1);
        // SAFETY: the caller's out pointer; the part lies in the buffer.
        unsafe {
            part.write(if cut < text.len() {
                buffer.add(cut)
            } else {
                std::ptr::null_mut()
            })
        };
    }
    n
}

/// The host's temporary folder, with a trailing `\`.
fn temp_path() -> String {
    let mut text = guest_path(&std::env::temp_dir());
    if !text.ends_with('\\') {
        text.push('\\');
    }
    text
}

pub extern "win64" fn GetTempPathW(capacity: u32, buffer: *mut u16) -> u32 {
    let text: Vec<u16> = temp_path().encode_utf16().collect();
    deliver(&text, buffer, capacity)
}

pub extern "win64" fn GetTempPathA(capacity: u32, buffer: *mut u8) -> u32 {
    deliver(temp_path().as_bytes(), buffer, capacity)
}

fn current_directory() -> Result<String, u32> {
    std::env::current_dir()
        .map(|p| guest_path(&p))
        .map_err(|e| win32_error(&e))
}

pub extern "win64" fn GetCurrentDirectoryW(capacity: u32, buffer: *mut u16) -> u32 {
    match current_directory() {
        Ok(t) => deliver(&t.encode_utf16().collect::<Vec<_>>(), buffer, capacity),
        Err(e) => fail(e),
    }
}

pub extern "win64" fn GetCurrentDirectoryA(capacity: u32, buffer: *mut u8) -> u32 {
    match current_directory() {
        Ok(t) => deliver(t.as_bytes(), buffer, capacity),
        Err(e) => fail(e),
    }
}

fn set_current_directory(p: &Path) -> Result<(), u32> {
    std::env::set_current_dir(p).map_err(|e| match e.raw_os_error() {
        Some(libc::ENOTDIR) if p.exists() => ERROR_DIRECTORY,
        _ => path_error(&e, p),
    })
}

pub extern "win64" fn SetCurrentDirectoryW(name: *const u16) -> i32 {
    by_name(wide_name(name), set_current_directory)
}

pub extern "win64" fn SetCurrentDirectoryA(name: *const c_char) -> i32 {
    by_name(narrow_name(name), set_current_directory)
}

/// `statvfs` of the folder (null: the current one).
fn disk(name: Option<String>) -> Result<libc::statvfs, u32> {
    let path = match name {
        Some(n) => host_path(&n)?,
        None => std::env::current_dir().map_err(|e| win32_error(&e))?,
    };
    use std::os::unix::ffi::OsStrExt;
    let c =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| ERROR_INVALID_PARAMETER)?;
    // SAFETY: an all-zero statvfs is valid; the call fills it.
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return Err(path_error(&std::io::Error::last_os_error(), &path));
    }
    Ok(s)
}

pub extern "win64" fn GetDiskFreeSpaceExW(
    name: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let s = match disk(wide_name(name)) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let unit = s.f_frsize;
    for (out, value) in [
        (available, s.f_bavail * unit),
        (total, s.f_blocks * unit),
        (free, s.f_bfree * unit),
    ] {
        if !out.is_null() {
            // SAFETY: the caller's out pointer.
            unsafe { out.write_unaligned(value) };
        }
    }
    1
}

/// 512-byte sectors, clusters of the file system's fragment size; counts
/// cap at 32 bits, as Windows caps them.
pub extern "win64" fn GetDiskFreeSpaceW(
    name: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let s = match disk(wide_name(name)) {
        Ok(s) => s,
        Err(e) => return fail(e),
    };
    let cap = |v: u64| v.min(u64::from(u32::MAX)) as u32;
    let values = [
        cap((s.f_frsize / 512).max(1)),
        512,
        cap(s.f_bavail),
        cap(s.f_blocks),
    ];
    for (out, value) in [
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    ]
    .into_iter()
    .zip(values)
    {
        if !out.is_null() {
            // SAFETY: the caller's out pointer.
            unsafe { out.write(value) };
        }
    }
    1
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-path",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-file-*",
            "api-ms-win-core-processenvironment-*",
        ],
        exports: vec![
            hle_export!(GetFileAttributesW),
            hle_export!(GetFileAttributesA),
            hle_export!(GetFileAttributesExW),
            hle_export!(GetFileAttributesExA),
            hle_export!(SetFileAttributesW),
            hle_export!(SetFileAttributesA),
            hle_export!(FindFirstFileW),
            hle_export!(FindFirstFileExW),
            hle_export!(FindNextFileW),
            hle_export!(FindClose),
            hle_export!(CreateDirectoryW),
            hle_export!(CreateDirectoryA),
            hle_export!(RemoveDirectoryW),
            hle_export!(RemoveDirectoryA),
            hle_export!(DeleteFileW),
            hle_export!(DeleteFileA),
            hle_export!(MoveFileExW),
            hle_export!(MoveFileW),
            hle_export!(CopyFileW),
            hle_export!(CopyFile2),
            hle_export!(CreateHardLinkW),
            hle_export!(CreateSymbolicLinkW),
            hle_export!(GetFullPathNameW),
            hle_export!(GetFullPathNameA),
            hle_export!(GetTempPathW),
            hle_export!(GetTempPathA),
            hle_export!(GetCurrentDirectoryW),
            hle_export!(GetCurrentDirectoryA),
            hle_export!(SetCurrentDirectoryW),
            hle_export!(SetCurrentDirectoryA),
            hle_export!(GetDiskFreeSpaceExW),
            hle_export!(GetDiskFreeSpaceW),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::kernel32_file::FILE_ATTRIBUTE_DIRECTORY;
    use crate::linux::kernel32_file::tests::w;
    use victory_loader::linux::teb::last_error;

    fn wp(p: &Path) -> Vec<u16> {
        w(&guest_path(p))
    }

    fn found(data: &FindDataW) -> String {
        let n = data.name.iter().position(|&c| c == 0).unwrap();
        String::from_utf16_lossy(&data.name[..n])
    }

    #[test]
    fn layouts_match_windows() {
        assert_eq!(size_of::<FindDataW>(), 592);
        assert_eq!(std::mem::offset_of!(FindDataW, name), 44);
        assert_eq!(size_of::<FileAttributeData>(), 36);
        assert_eq!(size_of::<CopyFile2Parameters>(), 32);
    }

    #[test]
    fn wildcards() {
        assert!(matches("*", "anything") && matches("*.*", "noext") && matches("*.RPF", "a.rpf"));
        assert!(matches("a?c", "ABC") && !matches("a?c", "ac") && matches("*a*b", "xaxxb"));
        assert!(!matches("*.txt", "a.txt.bak") && matches("x*", "x") && !matches("b", "a"));
    }

    #[test]
    fn folders_listings_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("Sub");
        assert_eq!(CreateDirectoryW(wp(&sub).as_ptr(), std::ptr::null()), 1);
        assert_eq!(CreateDirectoryW(wp(&sub).as_ptr(), std::ptr::null()), 0);
        assert_eq!(last_error(), ERROR_ALREADY_EXISTS);
        assert_eq!(
            CreateDirectoryW(wp(&root.join("a/b")).as_ptr(), std::ptr::null()),
            0
        );
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);
        for name in ["b.TXT", "a.txt", "c.bin"] {
            std::fs::write(sub.join(name), name).unwrap();
        }
        assert_eq!(
            GetFileAttributesW(wp(&root.join("sub")).as_ptr()),
            FILE_ATTRIBUTE_DIRECTORY
        );
        assert_eq!(
            GetFileAttributesW(wp(&root.join("SUB/A.TXT")).as_ptr()),
            0x20
        );
        assert_eq!(
            GetFileAttributesW(wp(&root.join("sub/nope")).as_ptr()),
            INVALID_FILE_ATTRIBUTES
        );
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);
        assert_eq!(
            GetFileAttributesW(wp(&root.join("nope/nope")).as_ptr()),
            INVALID_FILE_ATTRIBUTES
        );
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);
        let mut data = FileAttributeData::default();
        assert_eq!(
            GetFileAttributesExW(wp(&sub.join("c.bin")).as_ptr(), 0, &mut data),
            1
        );
        assert_eq!(data.size_low, 5);
        assert_eq!(
            SetFileAttributesW(wp(&sub.join("c.bin")).as_ptr(), FILE_ATTRIBUTE_READONLY),
            1
        );
        assert_eq!(GetFileAttributesW(wp(&sub.join("c.bin")).as_ptr()), 0x21);
        assert_eq!(SetFileAttributesW(wp(&sub.join("c.bin")).as_ptr(), 0x80), 1);

        // A listing: dots first, then names in upper-case order, case-blind.
        // SAFETY: an all-zero WIN32_FIND_DATAW is valid.
        let mut fd: FindDataW = unsafe { std::mem::zeroed() };
        let h = FindFirstFileW(
            w(&format!("{}\\SUB\\*.txt", guest_path(root))).as_ptr(),
            &mut fd,
        );
        assert_ne!(h, INVALID_HANDLE_VALUE);
        let mut names = vec![found(&fd)];
        while FindNextFileW(h, &mut fd) == 1 {
            names.push(found(&fd));
        }
        assert_eq!(last_error(), ERROR_NO_MORE_FILES);
        assert_eq!(names, ["a.txt", "b.TXT"]);
        assert_eq!(FindClose(h), 1);
        let h = FindFirstFileExW(
            w(&format!("{}\\*", guest_path(&sub))).as_ptr(),
            1,
            &mut fd,
            0,
            std::ptr::null_mut(),
            0,
        );
        assert_eq!(found(&fd), ".");
        FindClose(h);
        let h = FindFirstFileExW(
            w(&format!("{}\\*", guest_path(root))).as_ptr(),
            0,
            &mut fd,
            1,
            std::ptr::null_mut(),
            0,
        );
        let mut names = vec![found(&fd)];
        while FindNextFileW(h, &mut fd) == 1 {
            names.push(found(&fd));
        }
        assert_eq!(names, [".", "..", "Sub"]);
        FindClose(h);
        assert_eq!(
            FindFirstFileW(w(&format!("{}\\*.zip", guest_path(&sub))).as_ptr(), &mut fd),
            INVALID_HANDLE_VALUE
        );
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);
        assert_eq!(
            FindFirstFileW(w(&format!("{}\\no\\*", guest_path(&sub))).as_ptr(), &mut fd),
            INVALID_HANDLE_VALUE
        );
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);
        assert_eq!(FindClose(h), 0);

        // Move, copy, delete. "b.txt" is the folder's "b.TXT", as on Windows.
        let (a, b) = (sub.join("a.txt"), sub.join("b.txt"));
        assert_eq!(MoveFileExW(wp(&a).as_ptr(), wp(&b).as_ptr(), 0), 0);
        assert_eq!(last_error(), ERROR_ALREADY_EXISTS);
        assert_eq!(CopyFileW(wp(&a).as_ptr(), wp(&b).as_ptr(), 1), 0);
        assert_eq!(last_error(), ERROR_FILE_EXISTS);
        assert_eq!(
            CopyFile2(
                wp(&a).as_ptr(),
                wp(&sub.join("d.txt")).as_ptr(),
                std::ptr::null()
            ),
            0
        );
        assert_eq!(std::fs::read(sub.join("d.txt")).unwrap(), b"a.txt");
        let params = CopyFile2Parameters {
            size: 32,
            flags: COPY_FILE_FAIL_IF_EXISTS,
            cancel: std::ptr::null(),
            progress: std::ptr::null(),
            context: std::ptr::null_mut(),
        };
        assert_eq!(
            CopyFile2(wp(&a).as_ptr(), wp(&b).as_ptr(), &params) as u32,
            0x8007_0050
        );
        assert_eq!(
            MoveFileExW(wp(&a).as_ptr(), wp(&b).as_ptr(), MOVEFILE_REPLACE_EXISTING),
            1
        );
        assert_eq!(std::fs::read(sub.join("b.TXT")).unwrap(), b"a.txt");
        assert_eq!(MoveFileW(wp(&a).as_ptr(), wp(&b).as_ptr()), 0);
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);
        assert_eq!(
            CreateHardLinkW(
                wp(&sub.join("h")).as_ptr(),
                wp(&b).as_ptr(),
                std::ptr::null()
            ),
            1
        );
        assert_eq!(
            CreateSymbolicLinkW(wp(&sub.join("s")).as_ptr(), w("b.TXT").as_ptr(), 0),
            1
        );
        assert_eq!(std::fs::read(sub.join("s")).unwrap(), b"a.txt");
        assert_eq!(DeleteFileW(wp(&sub).as_ptr()), 0);
        assert_eq!(last_error(), ERROR_ACCESS_DENIED);
        assert_eq!(RemoveDirectoryW(wp(&sub).as_ptr()), 0);
        assert_eq!(last_error(), 145);
        assert_eq!(RemoveDirectoryW(wp(&b).as_ptr()), 0);
        assert_eq!(last_error(), ERROR_DIRECTORY);
        for name in ["b.txt", "c.bin", "d.txt", "h", "s"] {
            assert_eq!(DeleteFileW(wp(&sub.join(name)).as_ptr()), 1, "{name}");
        }
        assert_eq!(DeleteFileW(wp(&sub.join("b.txt")).as_ptr()), 0);
        assert_eq!(last_error(), ERROR_FILE_NOT_FOUND);
        assert_eq!(RemoveDirectoryW(wp(&sub).as_ptr()), 1);
    }

    #[test]
    fn process_paths() {
        let mut buf = [0u16; 512];
        let mut part = std::ptr::null_mut();
        let n = GetFullPathNameW(
            w("\\a\\b\\..\\c.txt").as_ptr(),
            512,
            buf.as_mut_ptr(),
            &mut part,
        );
        assert_eq!(String::from_utf16_lossy(&buf[..n as usize]), "\\a\\c.txt");
        assert_eq!(part, buf[3..].as_mut_ptr());
        assert_eq!(
            GetFullPathNameW(w("\\a\\b\\").as_ptr(), 512, buf.as_mut_ptr(), &mut part),
            5
        );
        assert!(part.is_null());
        assert_eq!(
            GetFullPathNameW(w("\\a\\c.txt").as_ptr(), 3, buf.as_mut_ptr(), &mut part),
            9
        );
        let mut abuf = [0u8; 64];
        let mut apart = std::ptr::null_mut();
        assert_eq!(
            GetFullPathNameA(c"/x/y".as_ptr(), 64, abuf.as_mut_ptr(), &mut apart),
            4
        );
        assert_eq!(&abuf[..5], b"\\x\\y\0");

        let n = GetTempPathW(512, buf.as_mut_ptr()) as usize;
        let temp = String::from_utf16_lossy(&buf[..n]);
        assert!(temp.ends_with('\\'));
        assert_eq!(host_path(&temp).unwrap(), std::env::temp_dir());
        assert_eq!(GetTempPathA(0, std::ptr::null_mut()) as usize, n + 1);

        let n = GetCurrentDirectoryW(512, buf.as_mut_ptr()) as usize;
        let here = String::from_utf16_lossy(&buf[..n]);
        assert_eq!(host_path(&here).unwrap(), std::env::current_dir().unwrap());
        // Setting it to where it is keeps other tests' relative paths working.
        assert_eq!(SetCurrentDirectoryW(w(&here).as_ptr()), 1);
        assert_eq!(SetCurrentDirectoryW(w("\\no\\such\\folder").as_ptr()), 0);
        assert_eq!(last_error(), ERROR_PATH_NOT_FOUND);

        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        assert_eq!(
            GetDiskFreeSpaceExW(std::ptr::null(), &mut avail, &mut total, &mut free),
            1
        );
        assert!(total > 0 && free >= avail);
        let mut bps = 0;
        assert_eq!(
            GetDiskFreeSpaceW(
                w(&temp).as_ptr(),
                std::ptr::null_mut(),
                &mut bps,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            1
        );
        assert_eq!(bps, 512);
    }
}
