//! Virtual drives over host folders: on the console, title storage is a
//! drive of its own, and titles build paths on that assumption.
//!
//! Every title import that takes a path is bound here. A path on a virtual
//! drive (`X:\..`, `X:/..`, `X:`, `\\?\X:\..`) is rewritten to the host
//! folder behind it and handed to the host's own function; any other path
//! goes to the host unchanged. A rewritten narrow path goes to the host's
//! wide twin, so host folders outside the ANSI code page work; never to our
//! wide wrapper, which would rewrite the host path a second time when its
//! drive letter is mounted too. Letters are chosen among those the host
//! does not use, so no host drive is hidden.
//!
//! On Linux the host function is ours (`crate::linux`), which reads a host
//! path spelled the Windows way: the rewritten path is the Linux folder and
//! the title's tail, with `/` separators. There are no host drives, narrow
//! paths are UTF-8, and the title's own `msvcp140` is mapped, so its
//! `_Fiopen` reaches the CRT functions here through its imports.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::{CStr, c_char, c_void};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

#[cfg(windows)]
use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

#[cfg(windows)]
use crate::HleExport;
use crate::{HleModule, hle_export};

/// The host's own function: what the import would have been bound to.
macro_rules! host {
    ($dll:literal, $name:literal: $ty:ty) => {{
        static ADDRESS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let address = *ADDRESS.get_or_init(|| {
            crate::host::function($dll, $name)
                .unwrap_or_else(|| panic!("the host lacks {}!{}", $dll, $name))
        });
        // SAFETY: the host exports this function with this signature.
        unsafe { std::mem::transmute::<usize, $ty>(address) }
    }};
}
// Also for other modules' host calls.
#[cfg_attr(not(windows), allow(unused_imports))]
pub(crate) use host;

/// Mounted drives: upper-case letter and host root (UTF-16, no trailing
/// separator, no NUL).
static DRIVES: Mutex<Vec<(u16, Vec<u16>)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u16, Vec<u16>)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it already
/// has one): the highest letter neither the host nor another mount uses.
pub fn mount(root: &Path) -> Option<char> {
    let wide = root_wide(root);
    let mut drives = drives();
    if let Some(&(letter, _)) = drives.iter().find(|(_, r)| *r == wide) {
        return char::from_u32(letter.into());
    }
    let host = host_drives();
    let letter = (b'D'..=b'Z').rev().find(|&l| {
        host >> (l - b'A') & 1 == 0 && !drives.iter().any(|(m, _)| *m == u16::from(l))
    })?;
    tracing::info!(drive = %char::from(letter), root = %root.display(), "virtual drive");
    drives.push((letter.into(), wide));
    Some(letter.into())
}

/// Gives the host folder `root` a drive letter the console fixes (titles
/// open their package as `G:\..` and temporary files as `T:\..`). A host
/// drive of that letter is hidden from the title.
pub fn mount_at(letter: char, root: &Path) {
    let letter = letter.to_ascii_uppercase();
    let Some(index) = u32::from(letter)
        .checked_sub(u32::from('A'))
        .filter(|i| *i < 26)
    else {
        return;
    };
    let hides_host_drive = host_drives() >> index & 1 != 0;
    tracing::info!(drive = %letter, root = %root.display(), hides_host_drive, "virtual drive");
    let mut drives = drives();
    drives.retain(|(l, _)| u32::from(*l) != u32::from(letter));
    drives.push((letter as u16, root_wide(root)));
}

/// The host's drive letters, a bit each from `A:`; Linux has none.
fn host_drives() -> u32 {
    // SAFETY: no arguments.
    #[cfg(windows)]
    return unsafe { GetLogicalDrives() };
    #[cfg(not(windows))]
    0
}

/// `root` as UTF-16, absolute, without trailing separators (a drive root,
/// or `/`, keeps its own).
fn root_wide(root: &Path) -> Vec<u16> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_owned());
    #[cfg(windows)]
    let (mut wide, keep): (Vec<u16>, _) = (root.as_os_str().encode_wide().collect(), 3);
    #[cfg(not(windows))]
    let (mut wide, keep): (Vec<u16>, _) = (root.to_string_lossy().encode_utf16().collect(), 1);
    while wide.len() > keep && wide.last().is_some_and(|c| SEPARATORS.contains(c)) {
        wide.pop();
    }
    wide
}

const SEPARATORS: [u16; 2] = [b'\\' as u16, b'/' as u16];
const VERBATIM: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];

/// Host paths from this length on take the `\\?\` form, which lifts the
/// host's MAX_PATH limit (248 characters for a directory, 260 for a file).
#[cfg(windows)]
const LONG: usize = 248;

/// `root` and the title's `tail` as a `\\?\` path. That form turns off the
/// host's own clean-up, so `/` becomes `\` and `.` and `..` are resolved
/// here (`..` stops at the root).
#[cfg(windows)]
fn verbatim(root: &[u16], tail: &[u16]) -> Vec<u16> {
    let backslash = u16::from(b'\\');
    let mut out = if root.starts_with(&VERBATIM) {
        root.to_vec()
    } else if let Some(unc) = root.strip_prefix(&[backslash; 2][..]) {
        let mut out = VERBATIM.to_vec();
        out.extend("UNC\\".encode_utf16());
        out.extend_from_slice(unc);
        out
    } else {
        [&VERBATIM[..], root].concat()
    };
    while out.last() == Some(&backslash) {
        out.pop();
    }
    let dot = u16::from(b'.');
    let mut parts: Vec<&[u16]> = Vec::new();
    for part in tail.split(|c| SEPARATORS.contains(c)) {
        match part {
            [] => {}
            [d] if *d == dot => {}
            [d, e] if [*d, *e] == [dot; 2] => drop(parts.pop()),
            _ => parts.push(part),
        }
    }
    for part in parts {
        out.push(backslash);
        out.extend_from_slice(part);
    }
    out
}

/// The host path, NUL-terminated, for `path` (without its NUL) if it is on
/// a virtual drive.
pub(crate) fn translate(path: &[u16]) -> Option<Vec<u16>> {
    let (prefix, rest) = match path.strip_prefix(&VERBATIM[..]) {
        Some(rest) => (&VERBATIM[..], rest),
        None => (&[][..], path),
    };
    let &[letter, colon, ..] = rest else {
        return None;
    };
    if colon != u16::from(b':') {
        return None;
    }
    let letter = u8::try_from(letter).ok().filter(u8::is_ascii_alphabetic)?;
    let letter = u16::from(letter.to_ascii_uppercase());
    let drives = drives();
    let (_, root) = drives.iter().find(|(l, _)| *l == letter)?;
    Some(join(prefix, root, &rest[2..]))
}

/// The host path, NUL-terminated, of `tail` (what follows `X:`) on the
/// drive over `root`; `prefix` is the title's `\\?\` or nothing.
#[cfg(windows)]
fn join(prefix: &[u16], root: &[u16], tail: &[u16]) -> Vec<u16> {
    if prefix.is_empty() && root.len() + tail.len() >= LONG {
        let mut out = verbatim(root, tail);
        out.push(0);
        return out;
    }
    let mut out = prefix.to_vec();
    out.extend_from_slice(root);
    // `X:` and `X:name` are relative to the root.
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(b'\\'.into());
    }
    let verbatim = !prefix.is_empty();
    out.extend(tail.iter().map(|&c| {
        if verbatim && c == u16::from(b'/') {
            b'\\'.into()
        } else {
            c
        }
    }));
    out.push(0);
    out
}

/// See the Windows `join`: our Linux layer reads `/` and `\` and has no
/// length limit, so the path is the root, `/` and the tail, `/`-separated.
#[cfg(not(windows))]
fn join(_prefix: &[u16], root: &[u16], tail: &[u16]) -> Vec<u16> {
    let slash = u16::from(b'/');
    let mut out = root.to_vec();
    if !tail.first().is_some_and(|c| SEPARATORS.contains(c)) {
        out.push(slash);
    }
    out.extend(tail.iter().map(|&c| if c == u16::from(b'\\') { slash } else { c }));
    out.push(0);
    out
}

/// A title's wide path: the host path when it is on a virtual drive, else
/// the title's own pointer.
struct Wide(*const u16, Option<Vec<u16>>);

impl Wide {
    fn ptr(&self) -> *const u16 {
        self.1.as_ref().map_or(self.0, |p| p.as_ptr())
    }
}

fn wide(path: *const u16) -> Wide {
    if path.is_null() {
        return Wide(path, None);
    }
    // SAFETY: the title passes a NUL-terminated path.
    let path_slice = unsafe {
        let len = (0..).take_while(|&i| *path.add(i) != 0).count();
        std::slice::from_raw_parts(path, len)
    };
    let host = translate(path_slice);
    trace(path_slice, host.as_deref());
    Wide(path, host)
}

/// Every title path at debug level, with the host path if rewritten.
fn trace(path: &[u16], host: Option<&[u16]>) {
    tracing::debug!(
        path = %String::from_utf16_lossy(path),
        host = host.map(|h| String::from_utf16_lossy(&h[..h.len() - 1])),
        "title path"
    );
}

/// A narrow path (ANSI code page, as the host decodes it) on a virtual
/// drive, as a wide host path.
fn narrow(path: *const c_char) -> Option<Vec<u16>> {
    if path.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
    // The drive part is ASCII in every code page; decode only candidates.
    match bytes.strip_prefix(br"\\?\").unwrap_or(bytes) {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => {}
        _ => {
            tracing::debug!(path = %String::from_utf8_lossy(bytes), "title path");
            return None;
        }
    }
    #[cfg(not(windows))]
    let wide: Vec<u16> = String::from_utf8_lossy(bytes).encode_utf16().collect();
    #[cfg(windows)]
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: sizing call, then a call with room for `n` units.
    #[cfg(windows)]
    let wide = unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        let mut wide = vec![0u16; usize::try_from(n).ok()?];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n);
        wide
    };
    let host = translate(&wide);
    trace(&wide, host.as_deref());
    host
}

/// An ASCII mode string (`"rb"`, `"w, ccs=UTF-8"`) for a wide CRT call.
fn widen(mode: *const c_char) -> Vec<u16> {
    if mode.is_null() {
        return vec![0];
    }
    // SAFETY: a NUL-terminated mode string.
    let bytes = unsafe { CStr::from_ptr(mode) }.to_bytes();
    bytes.iter().map(|&b| u16::from(b)).chain([0]).collect()
}

type Handle = *mut c_void;
type File = *mut c_void;

// kernel32 and its file API sets.

pub extern "win64" fn CreateFileW(
    path: *const u16,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFileW": extern "win64" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
        p.ptr(),
        access,
        share,
        security,
        disposition,
        flags,
        template,
    )
}

pub extern "win64" fn CreateFileA(
    path: *const c_char,
    access: u32,
    share: u32,
    security: *const c_void,
    disposition: u32,
    flags: u32,
    template: Handle,
) -> Handle {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "CreateFileW": extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                p.as_ptr(),
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
        None => {
            host!("kernel32.dll", "CreateFileA": extern "win64" fn(*const c_char, u32, u32, *const c_void, u32, u32, Handle) -> Handle)(
                path,
                access,
                share,
                security,
                disposition,
                flags,
                template,
            )
        }
    }
}

/// Used by Bink.
pub extern "win64" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    params: *const c_void,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "CreateFile2": extern "win64" fn(*const u16, u32, u32, u32, *const c_void) -> Handle)(
        p.ptr(),
        access,
        share,
        disposition,
        params,
    )
}

pub extern "win64" fn DeleteFileA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "DeleteFileW": extern "win64" fn(*const u16) -> i32)(p.as_ptr())
        }
        None => {
            host!("kernel32.dll", "DeleteFileA": extern "win64" fn(*const c_char) -> i32)(path)
        }
    }
}

pub extern "win64" fn DeleteFileW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "DeleteFileW": extern "win64" fn(*const u16) -> i32)(p.ptr())
}

pub extern "win64" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "RemoveDirectoryW": extern "win64" fn(*const u16) -> i32)(p.ptr())
}

pub extern "win64" fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "SetFileAttributesW": extern "win64" fn(*const u16, u32) -> i32)(
        p.ptr(),
        attributes,
    )
}

pub extern "win64" fn GetFileAttributesA(path: *const c_char) -> u32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "GetFileAttributesW": extern "system" fn(*const u16) -> u32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "GetFileAttributesA": extern "win64" fn(*const c_char) -> u32)(
                path,
            )
        }
    }
}

pub extern "win64" fn CreateDirectoryW(path: *const u16, security: *const c_void) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "CreateDirectoryW": extern "win64" fn(*const u16, *const c_void) -> i32)(
        p.ptr(),
        security,
    )
}

pub extern "win64" fn GetFileAttributesExW(
    path: *const u16,
    level: i32,
    info: *mut c_void,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesExW": extern "win64" fn(*const u16, i32, *mut c_void) -> i32)(
        p.ptr(),
        level,
        info,
    )
}

pub extern "win64" fn GetFileAttributesW(path: *const u16) -> u32 {
    let p = wide(path);
    host!("kernel32.dll", "GetFileAttributesW": extern "win64" fn(*const u16) -> u32)(p.ptr())
}

pub extern "win64" fn GetDiskFreeSpaceExW(
    path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    let p = wide(path);
    host!("kernel32.dll", "GetDiskFreeSpaceExW": extern "win64" fn(*const u16, *mut u64, *mut u64, *mut u64) -> i32)(
        p.ptr(),
        available,
        total,
        free,
    )
}

/// Wants a volume root: a virtual drive answers for the host volume its
/// folder is on.
pub extern "win64" fn GetDiskFreeSpaceW(
    root: *const u16,
    sectors_per_cluster: *mut u32,
    bytes_per_sector: *mut u32,
    free_clusters: *mut u32,
    clusters: *mut u32,
) -> i32 {
    let mut p = wide(root);
    if let Some(host) = &mut p.1
        && let [letter, colon, sep, ..] = host[..]
        && colon == u16::from(b':')
        && SEPARATORS.contains(&sep)
    {
        *host = vec![letter, colon, sep, 0];
    }
    host!("kernel32.dll", "GetDiskFreeSpaceW": extern "win64" fn(*const u16, *mut u32, *mut u32, *mut u32, *mut u32) -> i32)(
        p.ptr(),
        sectors_per_cluster,
        bytes_per_sector,
        free_clusters,
        clusters,
    )
}

pub extern "win64" fn FindFirstFileW(path: *const u16, data: *mut c_void) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileW": extern "win64" fn(*const u16, *mut c_void) -> Handle)(
        p.ptr(),
        data,
    )
}

pub extern "win64" fn FindFirstFileExW(
    path: *const u16,
    level: i32,
    data: *mut c_void,
    search: i32,
    filter: *mut c_void,
    flags: u32,
) -> Handle {
    let p = wide(path);
    host!("kernel32.dll", "FindFirstFileExW": extern "win64" fn(*const u16, i32, *mut c_void, i32, *mut c_void, u32) -> Handle)(
        p.ptr(),
        level,
        data,
        search,
        filter,
        flags,
    )
}

pub extern "win64" fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "MoveFileExW": extern "win64" fn(*const u16, *const u16, u32) -> i32)(
        from.ptr(),
        to.ptr(),
        flags,
    )
}

pub extern "win64" fn SetCurrentDirectoryA(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("kernel32.dll", "SetCurrentDirectoryW": extern "win64" fn(*const u16) -> i32)(
                p.as_ptr(),
            )
        }
        None => {
            host!("kernel32.dll", "SetCurrentDirectoryA": extern "win64" fn(*const c_char) -> i32)(
                path,
            )
        }
    }
}

// The C runtime (ucrtbase, which the title's CRT imports bind to).

pub extern "win64" fn _wfopen(path: *const u16, mode: *const u16) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen": extern "win64" fn(*const u16, *const u16) -> File)(p.ptr(), mode)
}

#[cfg(windows)]
pub extern "win64" fn _wfopen_s(out: *mut File, path: *const u16, mode: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfopen_s": extern "win64" fn(*mut File, *const u16, *const u16) -> i32)(
        out,
        p.ptr(),
        mode,
    )
}

pub extern "win64" fn _wfsopen(path: *const u16, mode: *const u16, share: i32) -> File {
    let p = wide(path);
    host!("ucrtbase.dll", "_wfsopen": extern "win64" fn(*const u16, *const u16, i32) -> File)(
        p.ptr(),
        mode,
        share,
    )
}

pub extern "win64" fn _fsopen(path: *const c_char, mode: *const c_char, share: i32) -> File {
    match narrow(path) {
        Some(p) => _wfsopen(p.as_ptr(), widen(mode).as_ptr(), share),
        None => {
            host!("ucrtbase.dll", "_fsopen": extern "win64" fn(*const c_char, *const c_char, i32) -> File)(
                path, mode, share,
            )
        }
    }
}

// What `std::filesystem` (msvcp140) takes paths with: on Linux the title's
// own copy is mapped and its imports come here.

pub extern "win64" fn _wremove(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wremove": extern "win64" fn(*const u16) -> i32)(p.ptr())
}

pub extern "win64" fn _wrmdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wrmdir": extern "win64" fn(*const u16) -> i32)(p.ptr())
}

pub extern "win64" fn _wchdir(path: *const u16) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wchdir": extern "win64" fn(*const u16) -> i32)(p.ptr())
}

pub extern "win64" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("ucrtbase.dll", "_wrename": extern "win64" fn(*const u16, *const u16) -> i32)(
        from.ptr(),
        to.ptr(),
    )
}

pub extern "win64" fn CopyFile2(from: *const u16, to: *const u16, params: *const c_void) -> i32 {
    let (from, to) = (wide(from), wide(to));
    host!("kernel32.dll", "CopyFile2": extern "win64" fn(*const u16, *const u16, *const c_void) -> i32)(
        from.ptr(),
        to.ptr(),
        params,
    )
}

pub extern "win64" fn CreateHardLinkW(link: *const u16, target: *const u16, security: *const c_void) -> i32 {
    let (link, target) = (wide(link), wide(target));
    host!("kernel32.dll", "CreateHardLinkW": extern "win64" fn(*const u16, *const u16, *const c_void) -> i32)(
        link.ptr(),
        target.ptr(),
        security,
    )
}

/// A relative target stays relative to the link, as the host reads it.
pub extern "win64" fn CreateSymbolicLinkW(link: *const u16, target: *const u16, flags: u32) -> u8 {
    let (link, target) = (wide(link), wide(target));
    host!("kernel32.dll", "CreateSymbolicLinkW": extern "win64" fn(*const u16, *const u16, u32) -> u8)(
        link.ptr(),
        target.ptr(),
        flags,
    )
}

// Kernel reads into title memory. The GPU side leaves blocks of title GPU
// buffers read-only to see the title's next write, and the kernel's writes
// fail there instead of faulting into its handler; Grand Theft Auto VI's
// synchronous reads (title+0x313fc0) read straight into 4 KiB-aligned
// destinations and retry a failed `ReadFile` forever. So the destination
// is opened first (on Linux by the host layer's own reads, which these
// forward to).

pub extern "win64" fn ReadFile(
    file: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut c_void,
) -> i32 {
    #[cfg(windows)]
    victory_gpu::kernel_will_write(buffer as usize, len as usize);
    host!("kernel32.dll", "ReadFile": extern "win64" fn(*mut c_void, *mut u8, u32, *mut u32, *mut c_void) -> i32)(
        file, buffer, len, read, overlapped,
    )
}

/// The host CRT reads large requests straight into the caller's buffer.
pub extern "win64" fn fread(buffer: *mut u8, size: usize, count: usize, stream: File) -> usize {
    #[cfg(windows)]
    victory_gpu::kernel_will_write(buffer as usize, size.saturating_mul(count));
    host!("ucrtbase.dll", "fread": extern "win64" fn(*mut u8, usize, usize, File) -> usize)(
        buffer, size, count, stream,
    )
}

pub extern "win64" fn fopen(path: *const c_char, mode: *const c_char) -> File {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wfopen": extern "win64" fn(*const u16, *const u16) -> File)(
            p.as_ptr(),
            widen(mode).as_ptr(),
        ),
        None => {
            host!("ucrtbase.dll", "fopen": extern "win64" fn(*const c_char, *const c_char) -> File)(
                path, mode,
            )
        }
    }
}

#[cfg(windows)]
pub extern "win64" fn fopen_s(out: *mut File, path: *const c_char, mode: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wfopen_s": extern "win64" fn(*mut File, *const u16, *const u16) -> i32)(
                out,
                p.as_ptr(),
                widen(mode).as_ptr(),
            )
        }
        None => {
            host!("ucrtbase.dll", "fopen_s": extern "win64" fn(*mut File, *const c_char, *const c_char) -> i32)(
                out, path, mode,
            )
        }
    }
}

/// `_open(path, flags[, mode])` is variadic; on x64 the optional integer
/// travels in the same register either way, so it is passed on as is.
pub extern "win64" fn _open(path: *const c_char, flags: i32, mode: i32) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wopen": extern "win64" fn(*const u16, i32, i32) -> i32)(
            p.as_ptr(),
            flags,
            mode,
        ),
        None => host!("ucrtbase.dll", "_open": extern "win64" fn(*const c_char, i32, i32) -> i32)(
            path, flags, mode,
        ),
    }
}

/// See [`_open`]; used by libHttpClient.
pub extern "win64" fn _wopen(path: *const u16, flags: i32, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wopen": extern "win64" fn(*const u16, i32, i32) -> i32)(
        p.ptr(),
        flags,
        mode,
    )
}

pub extern "win64" fn _wstat64(path: *const u16, stat: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wstat64": extern "win64" fn(*const u16, *mut c_void) -> i32)(p.ptr(), stat)
}

#[cfg(windows)]
pub extern "win64" fn _stat64i32(path: *const c_char, stat: *mut c_void) -> i32 {
    match narrow(path) {
        Some(p) => {
            host!("ucrtbase.dll", "_wstat64i32": extern "win64" fn(*const u16, *mut c_void) -> i32)(
                p.as_ptr(),
                stat,
            )
        }
        None => {
            host!("ucrtbase.dll", "_stat64i32": extern "win64" fn(*const c_char, *mut c_void) -> i32)(
                path, stat,
            )
        }
    }
}

pub extern "win64" fn remove(path: *const c_char) -> i32 {
    match narrow(path) {
        Some(p) => host!("ucrtbase.dll", "_wremove": extern "win64" fn(*const u16) -> i32)(p.as_ptr()),
        None => host!("ucrtbase.dll", "remove": extern "win64" fn(*const c_char) -> i32)(path),
    }
}

#[cfg(windows)]
pub extern "win64" fn _wchmod(path: *const u16, mode: i32) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wchmod": extern "win64" fn(*const u16, i32) -> i32)(p.ptr(), mode)
}

#[cfg(windows)]
pub extern "win64" fn _wutime64(path: *const u16, times: *mut c_void) -> i32 {
    let p = wide(path);
    host!("ucrtbase.dll", "_wutime64": extern "win64" fn(*const u16, *mut c_void) -> i32)(
        p.ptr(),
        times,
    )
}

// The C++ library: `std::basic_filebuf::open` goes through `_Fiopen`.

#[cfg(windows)]
const FIOPEN: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z";
#[cfg(windows)]
const FIOPEN_WIDE: &str = "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z";

#[cfg(windows)]
/// `_Fiopen(const wchar_t* path, ios_base::openmode, int protection)`.
extern "win64" fn fiopen_wide(path: *const u16, mode: i32, protection: i32) -> File {
    let p = wide(path);
    host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "win64" fn(*const u16, i32, i32) -> File)(
        p.ptr(),
        mode,
        protection,
    )
}

#[cfg(windows)]
/// `_Fiopen(const char* path, ios_base::openmode, int protection)`.
extern "win64" fn fiopen(path: *const c_char, mode: i32, protection: i32) -> File {
    match narrow(path) {
        Some(p) => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEB_WHH@Z": extern "win64" fn(*const u16, i32, i32) -> File)(
                p.as_ptr(),
                mode,
                protection,
            )
        }
        None => {
            host!("msvcp140.dll", "?_Fiopen@std@@YAPEAU_iobuf@@PEBDHH@Z": extern "win64" fn(*const c_char, i32, i32) -> File)(
                path, mode, protection,
            )
        }
    }
}

const DLLS: &[&str] = &[
    "kernel32.dll",
    "kernelbase.dll",
    "api-ms-win-core-file-*",
    "api-ms-win-core-processenvironment-*",
    "ucrtbase.dll",
    "api-ms-win-crt-stdio-*",
    "api-ms-win-crt-filesystem-*",
    "api-ms-win-crt-time-*",
    "msvcp140.dll",
];

pub(crate) fn module() -> HleModule {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut module = HleModule {
        name: "files",
        // On Linux the title's `msvcp140` is mapped, not served.
        dlls: if cfg!(windows) { DLLS } else { &DLLS[..DLLS.len() - 1] },
        exports: vec![
            hle_export!(ReadFile),
            hle_export!(fread),
            hle_export!(CreateFileW),
            hle_export!(CreateFileA),
            hle_export!(CreateFile2),
            hle_export!(DeleteFileA),
            hle_export!(DeleteFileW),
            hle_export!(RemoveDirectoryW),
            hle_export!(SetFileAttributesW),
            hle_export!(GetFileAttributesA),
            hle_export!(CreateDirectoryW),
            hle_export!(GetFileAttributesW),
            hle_export!(GetFileAttributesExW),
            hle_export!(GetDiskFreeSpaceExW),
            hle_export!(GetDiskFreeSpaceW),
            hle_export!(FindFirstFileW),
            hle_export!(FindFirstFileExW),
            hle_export!(MoveFileExW),
            hle_export!(SetCurrentDirectoryA),
            hle_export!(CopyFile2),
            hle_export!(CreateHardLinkW),
            hle_export!(CreateSymbolicLinkW),
            hle_export!(_wfopen),
            hle_export!(_wfsopen),
            hle_export!(_fsopen),
            hle_export!(fopen),
            hle_export!(_open),
            hle_export!(_wopen),
            hle_export!(_wstat64),
            hle_export!(remove),
            hle_export!(_wremove),
            hle_export!(_wrmdir),
            hle_export!(_wchdir),
            hle_export!(_wrename),
        ],
    };
    // Our Linux CRT lacks these (Grand Theft Auto VI imports none), and on
    // Linux `msvcp140` is the title's own.
    #[cfg(windows)]
    module.exports.extend([
        hle_export!(_wfopen_s),
        hle_export!(fopen_s),
        hle_export!(_stat64i32),
        hle_export!(_wchmod),
        hle_export!(_wutime64),
        HleExport {
            name: FIOPEN,
            address: fiopen as *const () as usize,
        },
        HleExport {
            name: FIOPEN_WIDE,
            address: fiopen_wide as *const () as usize,
        },
    ]);
    module
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn kernel_reads_reach_the_buffer() {
        const GENERIC_READ: u32 = 0x8000_0000;
        const OPEN_EXISTING: u32 = 3;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        let null = std::ptr::null_mut();
        let name = w(&path.to_string_lossy());
        let handle = CreateFileW(name.as_ptr(), GENERIC_READ, 1, null, OPEN_EXISTING, 0, null);
        assert_ne!(handle as isize, -1);
        let (mut out, mut n) = ([0u8; 10], 0);
        assert_ne!(ReadFile(handle, out.as_mut_ptr(), 10, &mut n, null), 0);
        assert_eq!((&out, n), (b"0123456789", 10));
        host!("kernel32.dll", "CloseHandle": extern "win64" fn(Handle) -> i32)(handle);
    }

    fn text(v: &[u16]) -> String {
        String::from_utf16_lossy(&v[..v.len() - 1])
    }

    #[test]
    fn rewrites_only_paths_on_mounted_drives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().into_owned();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(
            mount(dir.path()),
            Some(letter),
            "mounting twice reuses the letter"
        );
        let tr = |s: String| translate(&w(&s)[..s.encode_utf16().count()]).map(|v| text(&v));
        assert_eq!(
            tr(format!("{letter}:/a/b.txt")),
            Some(format!("{root}/a/b.txt"))
        );
        let lower = letter.to_ascii_lowercase();
        // Windows keeps the title's separators; ours take `/`.
        let sep = if cfg!(windows) { '\\' } else { '/' };
        assert_eq!(tr(format!("{lower}:\\a")), Some(format!("{root}{sep}a")));
        assert_eq!(tr(format!("{letter}:")), Some(format!("{root}{sep}")));
        let verbatim_host = if cfg!(windows) {
            format!(r"\\?\{root}\a\b")
        } else {
            format!("{root}/a/b")
        };
        assert_eq!(tr(format!(r"\\?\{letter}:/a/b")), Some(verbatim_host));
        assert_eq!(tr(r"C:\Windows".into()), None);
        assert_eq!(tr("relative/path".into()), None);
        assert_eq!(tr(String::new()), None);

        // Fixed letters, as the console's package drive; a later mount of
        // the same letter replaces the earlier one.
        let package = tempfile::tempdir().unwrap();
        mount_at('g', dir.path());
        mount_at('G', package.path());
        assert_eq!(
            tr(r"G:\appdata0.rpf".into()),
            Some(format!(r"{}{sep}appdata0.rpf", package.path().display()))
        );
        assert_ne!(mount(dir.path()), Some('G'), "free letters skip fixed ones");
        #[cfg(not(windows))]
        assert_eq!(
            tr(format!(r"G:\a\{}\f", "d".repeat(250))),
            Some(format!("{}/a/{}/f", package.path().display(), "d".repeat(250))),
            "no length limit"
        );
        #[cfg(windows)]
        long_paths_take_the_verbatim_form(&tr, package.path());
    }

    #[cfg(windows)]
    fn long_paths_take_the_verbatim_form(tr: &dyn Fn(String) -> Option<String>, package: &Path) {
        // Long ones take the `\\?\` form, cleaned up as the host would.
        let long = "d".repeat(250);
        assert_eq!(
            tr(format!(r"G:/a/./b/../{long}//f")),
            Some(format!(r"\\?\{}\a\{long}\f", package.display()))
        );
        assert_eq!(
            verbatim(&w(r"\\server\share\")[..15], &w("/x/../../y")[..10]),
            w(r"\\?\UNC\server\share\y")[..22].to_vec()
        );
    }

    #[test]
    fn long_title_paths_work_on_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let deep = format!("{letter}:/{}/{}", "a".repeat(200), "b".repeat(100));
        let null = std::ptr::null_mut();
        let first = w(&deep[..203]);
        assert_ne!(CreateDirectoryW(first.as_ptr(), null), 0);
        assert_ne!(CreateDirectoryW(w(&deep).as_ptr(), null), 0);
        let file = format!("{deep}/../{}/f.txt", "b".repeat(100));
        let f = _wfopen(w(&file).as_ptr(), w("wb").as_ptr());
        assert!(!f.is_null(), "{file}");
        host!("ucrtbase.dll", "fclose": extern "win64" fn(File) -> i32)(f);
        assert_ne!(DeleteFileW(w(&format!("{deep}/f.txt")).as_ptr()), 0);
        assert_ne!(RemoveDirectoryW(w(&deep).as_ptr()), 0);
    }

    #[test]
    fn title_file_calls_land_in_the_host_folder() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let close = host!("kernel32.dll", "CloseHandle": extern "win64" fn(Handle) -> i32);
        let fclose = host!("ucrtbase.dll", "fclose": extern "win64" fn(File) -> i32);

        let path = w(&format!(r"{letter}:\created.bin"));
        let null = std::ptr::null_mut();
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_ne!(h as isize, -1);
        close(h);
        assert!(dir.path().join("created.bin").is_file());

        let path = format!("{letter}:/opened.txt\0");
        let f = fopen(path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("opened.txt").is_file());
        // Found whatever the case the title spells it in, as on the console.
        let f = fopen(format!("{letter}:\\OPENED.Txt\0").as_ptr().cast(), c"rb".as_ptr());
        assert!(!f.is_null());
        fclose(f);

        let path = format!("{letter}:/stream.txt\0");
        // `std::basic_filebuf::open`: our override on Windows, the title's
        // msvcp140 calling `_fsopen` and `_wfsopen` on Linux.
        #[cfg(windows)]
        let (narrow_open, wide_open) = {
            const OUT: i32 = 2;
            (
                |p: *const c_char| fiopen(p, OUT, 0x40),
                |p: *const u16| fiopen_wide(p, OUT, 0x40),
            )
        };
        #[cfg(not(windows))]
        let (narrow_open, wide_open) = (
            |p: *const c_char| _fsopen(p, c"w".as_ptr(), 0x40),
            |p: *const u16| _wfsopen(p, w("w").as_ptr(), 0x40),
        );
        let f = narrow_open(path.as_ptr().cast());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("stream.txt").is_file());

        assert_eq!(remove(path.as_ptr().cast()), 0);
        assert!(!dir.path().join("stream.txt").exists());
        let f = wide_open(w(&format!("{letter}:/wide.txt")).as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("wide.txt").is_file());
        // `std::filesystem`'s.
        let (from, to) = (w(&format!("{letter}:/wide.txt")), w(&format!(r"{letter}:\moved.txt")));
        assert_eq!(_wrename(from.as_ptr(), to.as_ptr()), 0);
        assert!(dir.path().join("moved.txt").is_file());
        assert_eq!(_wremove(to.as_ptr()), 0);
        assert!(!dir.path().join("moved.txt").exists());

        const OPEN_EXISTING: u32 = 3;
        let path = w(&format!(r"{letter}:\created.bin"));
        let h = CreateFile2(path.as_ptr(), GENERIC_WRITE, 0, OPEN_EXISTING, null);
        assert_ne!(h as isize, -1);
        close(h);
        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        assert_ne!(GetFileAttributesW(path.as_ptr()), INVALID_FILE_ATTRIBUTES);

        const O_WRONLY: i32 = 1;
        const O_CREAT: i32 = 0x100;
        const S_IWRITE: i32 = 0x80;
        let path = format!("{letter}:/low.bin\0");
        let fd = _open(path.as_ptr().cast(), O_WRONLY | O_CREAT, S_IWRITE);
        assert!(fd >= 0);
        host!("ucrtbase.dll", "_close": extern "win64" fn(i32) -> i32)(fd);
        assert!(dir.path().join("low.bin").is_file());

        // Other paths pass through untouched.
        let host_path = format!("{}\0", dir.path().join("plain.txt").display());
        let f = fopen(host_path.as_ptr().cast(), c"wb".as_ptr());
        assert!(!f.is_null());
        fclose(f);
        assert!(dir.path().join("plain.txt").is_file());
    }

    /// A narrow path is rewritten once, as its wide twin is. The inner
    /// drive's folder lies on the outer virtual drive, as the title folder
    /// does when it sits on a host `G:` drive and is mounted as `G:`. The
    /// outer letter is no host drive, so the host finds nothing there; a
    /// narrow call that reaches the outer folder rewrote the path twice.
    #[cfg(windows)]
    #[test]
    fn narrow_paths_are_rewritten_once() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("inner");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("present.txt"), b"").unwrap();
        let outer = mount(dir.path()).unwrap();
        let inner = mount(Path::new(&format!(r"{outer}:\inner"))).unwrap();
        let path = |name: &str| format!("{inner}:/{name}\0");

        const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;
        let wide = GetFileAttributesW(w(&format!("{inner}:/present.txt")).as_ptr());
        assert_eq!(wide, INVALID_FILE_ATTRIBUTES);
        let narrow = GetFileAttributesA(path("present.txt").as_ptr().cast());
        assert_eq!(narrow, wide, "GetFileAttributesA agrees with its wide twin");

        const GENERIC_WRITE: u32 = 0x4000_0000;
        const CREATE_ALWAYS: u32 = 2;
        let null = std::ptr::null_mut();
        let h = CreateFileA(
            path("a.bin").as_ptr().cast(),
            GENERIC_WRITE,
            0,
            null,
            CREATE_ALWAYS,
            0,
            null,
        );
        assert_eq!(h as isize, -1);
        assert!(fopen(path("b.txt").as_ptr().cast(), c"wb".as_ptr()).is_null());
        let mut f: File = std::ptr::null_mut();
        let error = fopen_s(&mut f, path("c.txt").as_ptr().cast(), c"wb".as_ptr());
        assert_ne!(error, 0);
        const OUT: i32 = 2;
        assert!(fiopen(path("d.txt").as_ptr().cast(), OUT, 0x40).is_null());

        // Nothing was made behind a path rewritten twice.
        let names: Vec<_> = std::fs::read_dir(&folder)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["present.txt"]);
    }
}
