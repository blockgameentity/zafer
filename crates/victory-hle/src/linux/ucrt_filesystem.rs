//! ucrtbase: the file system (`api-ms-win-crt-filesystem`), and the one
//! way from a title path to a host path for the stdio slice too.
//!
//! Paths reach us as host paths spelled the Windows way (the `files`
//! module rewrites console paths first): `\` and `/` both separate, a
//! `\\?\` prefix is dropped, no drive letter means the current
//! directory, and names are found whatever their case. Narrow paths are
//! UTF-8.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use super::ucrt_runtime::{EINVAL, errno_from_host, invalid, set_errno};
use super::ucrt_stdio::{Stream, lock_stream, unlock_stream};
use crate::{HleModule, hle_export};

/// As kernel32 reads it (`kernel32_file::host_path`): names found
/// whatever their case, as the CRT finds them through `CreateFile`.
fn host_path(text: String) -> PathBuf {
    super::kernel32_file::host_path(&text).unwrap_or_else(|_| PathBuf::from(text))
}

/// A narrow title path as a host path; None for null.
pub(crate) fn narrow_path(path: *const c_char) -> Option<PathBuf> {
    // SAFETY: null or NUL-terminated, per the C contract.
    (!path.is_null()).then(|| host_path(unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned()))
}

/// A wide title path as a host path; None for null.
pub(crate) fn wide_path(path: *const u16) -> Option<PathBuf> {
    // SAFETY: null or NUL-terminated, per the C contract.
    (!path.is_null()).then(|| host_path(String::from_utf16_lossy(unsafe { super::ucrt_string::units(path) })))
}

/// 0, or -1 with errno from the host's error.
fn result(r: std::io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => {
            set_errno(errno_from_host(e.raw_os_error().unwrap_or(libc::EINVAL)));
            -1
        }
    }
}

fn with_path(path: Option<PathBuf>, f: impl FnOnce(PathBuf) -> std::io::Result<()>) -> i32 {
    match path {
        Some(path) => result(f(path)),
        None => {
            invalid(EINVAL);
            -1
        }
    }
}

pub extern "win64" fn remove(path: *const c_char) -> i32 {
    with_path(narrow_path(path), std::fs::remove_file)
}

pub extern "win64" fn _wremove(path: *const u16) -> i32 {
    with_path(wide_path(path), std::fs::remove_file)
}

/// As on Windows, the new name must not exist.
pub extern "win64" fn _wrename(from: *const u16, to: *const u16) -> i32 {
    let Some(to) = wide_path(to) else {
        invalid(EINVAL);
        return -1;
    };
    with_path(wide_path(from), |from| {
        if to.symlink_metadata().is_ok() {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST));
        }
        std::fs::rename(from, &to)
    })
}

pub extern "win64" fn _wrmdir(path: *const u16) -> i32 {
    with_path(wide_path(path), std::fs::remove_dir)
}

pub extern "win64" fn _wchdir(path: *const u16) -> i32 {
    with_path(wide_path(path), std::env::set_current_dir)
}

/// `struct _stat64`.
#[repr(C)]
#[derive(Default)]
pub struct Stat64 {
    dev: u32,
    ino: u16,
    mode: u16,
    nlink: i16,
    uid: i16,
    gid: i16,
    rdev: u32,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
}

/// What the UCRT reports for a path: drive C, the read and write bits
/// copied to group and other, execute for the extensions Windows runs.
fn stat_of(path: &std::path::Path, m: &std::fs::Metadata) -> Stat64 {
    let kind: u16 = if m.is_dir() { 0x4000 } else { 0x8000 };
    let mut owner: u16 = 0x100; // _S_IREAD
    if m.mode() & 0o200 != 0 {
        owner |= 0x80;
    }
    let exe = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        ["exe", "bat", "cmd", "com"].iter().any(|x| x.eq_ignore_ascii_case(e))
    });
    if m.is_dir() || exe {
        owner |= 0x40;
    }
    Stat64 {
        dev: 2,
        ino: 0,
        mode: kind | owner | (owner >> 3) | (owner >> 6),
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 2,
        size: if m.is_dir() { 0 } else { m.len() as i64 },
        atime: m.atime(),
        mtime: m.mtime(),
        ctime: m.ctime(),
    }
}

pub extern "win64" fn _wstat64(path: *const u16, out: *mut Stat64) -> i32 {
    if out.is_null() {
        invalid(EINVAL);
        return -1;
    }
    with_path(wide_path(path), |path| {
        let m = std::fs::metadata(&path)?;
        // SAFETY: checked for null; the title's struct.
        unsafe { *out = stat_of(&path, &m) };
        Ok(())
    })
}

pub extern "win64" fn _lock_file(s: *mut Stream) {
    lock_stream(s);
}

pub extern "win64" fn _unlock_file(s: *mut Stream) {
    unlock_stream(s);
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-filesystem",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-filesystem-*"],
        exports: vec![
            hle_export!(remove),
            hle_export!(_wremove),
            hle_export!(_wrename),
            hle_export!(_wrmdir),
            hle_export!(_wchdir),
            hle_export!(_wstat64),
            hle_export!(_lock_file),
            hle_export!(_unlock_file),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_runtime::{EEXIST, ENOENT, errno};
    use crate::linux::ucrt_string::w;

    #[test]
    fn stat_has_the_ucrts_layout_and_bits() {
        assert_eq!(std::mem::size_of::<Stat64>(), 56);
        assert_eq!(std::mem::offset_of!(Stat64, rdev), 16);
        assert_eq!(std::mem::offset_of!(Stat64, size), 24);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.exe");
        std::fs::write(&file, b"12345").unwrap();
        let mut st = Stat64::default();
        let wide = w(&file.to_string_lossy().replace('/', "\\"));
        assert_eq!(_wstat64(wide.as_ptr(), &mut st), 0);
        assert_eq!((st.size, st.mode), (5, 0x8000 | 0x1C0 | 0x38 | 0x7));
        assert_eq!(_wstat64(w(&dir.path().to_string_lossy()).as_ptr(), &mut st), 0);
        assert_eq!(st.mode & 0xF000, 0x4000);
        assert_eq!(_wstat64(w("/no/such/file").as_ptr(), &mut st), -1);
        assert_eq!(errno(), ENOENT);
    }

    #[test]
    fn rename_remove_and_rmdir() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let (wa, wb) = (w(&a.to_string_lossy()), w(&b.to_string_lossy()));
        assert_eq!(_wrename(wa.as_ptr(), wb.as_ptr()), -1);
        assert_eq!(errno(), EEXIST);
        assert_eq!(_wremove(wb.as_ptr()), 0);
        assert_eq!(_wrename(wa.as_ptr(), wb.as_ptr()), 0);
        let nb = std::ffi::CString::new(b.to_str().unwrap()).unwrap();
        assert_eq!(remove(nb.as_ptr()), 0);
        assert_eq!(remove(nb.as_ptr()), -1);
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(_wrmdir(w(&sub.to_string_lossy()).as_ptr()), 0);
        assert!(!sub.exists());
        assert_eq!(remove(std::ptr::null()), -1);
    }
}
