//! `bcrypt`: the random number generator. The title opens the `RNG`
//! provider and fills buffers from it; other algorithms are not imported
//! and are not found.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::{HleModule, hle_export};

type Ntstatus = i32;

const STATUS_SUCCESS: Ntstatus = 0;
const STATUS_INVALID_HANDLE: Ntstatus = 0xC000_0008_u32 as i32;
const STATUS_INVALID_PARAMETER: Ntstatus = 0xC000_000D_u32 as i32;
const STATUS_NOT_FOUND: Ntstatus = 0xC000_0225_u32 as i32;
const STATUS_UNSUCCESSFUL: Ntstatus = 0xC000_0001_u32 as i32;

const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;

/// The one algorithm handle there is.
static RNG: u8 = 0;

fn rng() -> *mut c_void {
    (&raw const RNG).cast_mut().cast()
}

fn wide(text: *const u16) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!text.is_null()).then(|| unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    })
}

pub extern "win64" fn BCryptOpenAlgorithmProvider(
    out: *mut *mut c_void,
    algorithm: *const u16,
    _implementation: *const u16,
    _flags: u32,
) -> Ntstatus {
    if out.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    let name = wide(algorithm).unwrap_or_default();
    if !name.eq_ignore_ascii_case("RNG") {
        tracing::warn!(algorithm = %name, "BCryptOpenAlgorithmProvider: not implemented");
        return STATUS_NOT_FOUND;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(rng()) };
    STATUS_SUCCESS
}

pub extern "win64" fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, _flags: u32) -> Ntstatus {
    if algorithm == rng() { STATUS_SUCCESS } else { STATUS_INVALID_HANDLE }
}

pub extern "win64" fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, len: u32, flags: u32) -> Ntstatus {
    let system = flags & BCRYPT_USE_SYSTEM_PREFERRED_RNG != 0;
    if (system && !algorithm.is_null()) || (!system && algorithm != rng()) {
        return if system { STATUS_INVALID_PARAMETER } else { STATUS_INVALID_HANDLE };
    }
    if buffer.is_null() && len != 0 {
        return STATUS_INVALID_PARAMETER;
    }
    let mut done = 0usize;
    while done < len as usize {
        // SAFETY: the title's buffer of `len` bytes, filled from `done` on.
        let n = unsafe { libc::getrandom(buffer.add(done).cast(), len as usize - done, 0) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return STATUS_UNSUCCESSFUL;
        }
        done += n as usize;
    }
    STATUS_SUCCESS
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "bcrypt",
        dlls: &["bcrypt.dll", "bcryptprimitives.dll"],
        exports: vec![
            hle_export!(BCryptOpenAlgorithmProvider),
            hle_export!(BCryptCloseAlgorithmProvider),
            hle_export!(BCryptGenRandom),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn the_rng_fills_buffers() {
        let mut h = std::ptr::null_mut();
        assert_eq!(BCryptOpenAlgorithmProvider(&mut h, w("RNG").as_ptr(), std::ptr::null(), 0), STATUS_SUCCESS);
        let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
        assert_eq!(BCryptGenRandom(h, a.as_mut_ptr(), 64, 0), STATUS_SUCCESS);
        let none = std::ptr::null_mut();
        assert_eq!(BCryptGenRandom(none, b.as_mut_ptr(), 64, BCRYPT_USE_SYSTEM_PREFERRED_RNG), STATUS_SUCCESS);
        assert_ne!(a, b);
        assert_ne!(a, [0; 64]);
        assert_eq!(BCryptGenRandom(none, b.as_mut_ptr(), 64, 0), STATUS_INVALID_HANDLE);
        assert_eq!(BCryptCloseAlgorithmProvider(h, 0), STATUS_SUCCESS);
        let mut sha = std::ptr::null_mut();
        assert_eq!(BCryptOpenAlgorithmProvider(&mut sha, w("SHA256").as_ptr(), std::ptr::null(), 0), STATUS_NOT_FOUND);
    }
}
