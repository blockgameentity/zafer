//! ucrtbase: the environment (`api-ms-win-crt-environment`): the host
//! process's own variables and current directory.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, OsStr, c_char};
use std::os::unix::ffi::OsStrExt;

use super::ucrt_runtime::{EINVAL, ENOMEM, ERANGE, fail, fail_from_host, invalid};
use crate::{HleModule, hle_export};

/// An empty value removes the variable, as on Windows.
pub extern "win64" fn _putenv_s(name: *const c_char, value: *const c_char) -> i32 {
    if name.is_null() || value.is_null() {
        return invalid(EINVAL);
    }
    // SAFETY: NUL-terminated, per the C contract.
    let (name, value) = unsafe { (CStr::from_ptr(name).to_bytes(), CStr::from_ptr(value).to_bytes()) };
    if name.is_empty() || name.contains(&b'=') {
        return invalid(EINVAL);
    }
    let name = OsStr::from_bytes(name);
    // SAFETY: the title's own environment edit; glibc's environment is not
    // thread-safe on Windows either while another thread reads it.
    unsafe {
        if value.is_empty() {
            std::env::remove_var(name);
        } else {
            std::env::set_var(name, OsStr::from_bytes(value));
        }
    }
    0
}

/// The current directory as wide text; a null `buffer` gets a heap block
/// of at least `size` units, which the title frees.
pub extern "win64" fn _wgetcwd(buffer: *mut u16, size: i32) -> *mut u16 {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => {
            fail_from_host();
            return std::ptr::null_mut();
        }
    };
    let wide: Vec<u16> = cwd.to_string_lossy().encode_utf16().chain([0]).collect();
    let buffer = if buffer.is_null() {
        let units = wide.len().max(size.max(0) as usize);
        let block = super::ucrt_heap::malloc(units * 2).cast::<u16>();
        if block.is_null() {
            fail(ENOMEM);
            return block;
        }
        block
    } else {
        if size <= 0 {
            invalid(EINVAL);
            return std::ptr::null_mut();
        }
        if wide.len() > size as usize {
            fail(ERANGE);
            return std::ptr::null_mut();
        }
        buffer
    };
    // SAFETY: the buffer holds wide.len() units.
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer, wide.len()) };
    buffer
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-environment",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-environment-*"],
        exports: vec![hle_export!(_putenv_s), hle_export!(_wgetcwd)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_and_the_current_directory() {
        assert_eq!(_putenv_s(c"VICTORY_UCRT_TEST".as_ptr(), c"1".as_ptr()), 0);
        assert_eq!(std::env::var("VICTORY_UCRT_TEST").as_deref(), Ok("1"));
        assert_eq!(_putenv_s(c"VICTORY_UCRT_TEST".as_ptr(), c"".as_ptr()), 0);
        assert!(std::env::var("VICTORY_UCRT_TEST").is_err());
        assert_eq!(_putenv_s(c"A=B".as_ptr(), c"1".as_ptr()), EINVAL);
        let cwd = _wgetcwd(std::ptr::null_mut(), 0);
        // SAFETY: a NUL-terminated block of ours.
        let text = String::from_utf16_lossy(unsafe { crate::linux::ucrt_string::units(cwd) });
        assert_eq!(text, std::env::current_dir().unwrap().to_string_lossy());
        crate::linux::ucrt_heap::free(cwd.cast());
        let mut small = [0u16; 2];
        assert!(_wgetcwd(small.as_mut_ptr(), 2).is_null());
    }
}
