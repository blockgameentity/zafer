//! The registry (`advapi32`, `api-ms-win-core-registry-*`): empty. The
//! predefined keys open, every subkey and value is missing.
#![allow(non_snake_case)]

use crate::{HleModule, hle_export};

type Hkey = usize;

const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;

/// `HKEY_CLASSES_ROOT` (0x80000000) to `HKEY_CURRENT_USER_LOCAL_SETTINGS`,
/// sign-extended as 64-bit code holds them.
fn predefined(key: Hkey) -> bool {
    (0xFFFF_FFFF_8000_0000..=0xFFFF_FFFF_8000_0007).contains(&(key as u64))
        || (0x8000_0000..=0x8000_0007).contains(&(key as u64))
}

fn empty(text: *const u16) -> bool {
    // SAFETY: the title passes a NUL-terminated string or null.
    text.is_null() || unsafe { *text } == 0
}

fn open(root: Hkey, sub: *const u16, out: *mut Hkey) -> u32 {
    if out.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    if !predefined(root) {
        return ERROR_INVALID_HANDLE;
    }
    if !empty(sub) {
        return ERROR_FILE_NOT_FOUND;
    }
    // Opening a predefined key itself gives it back.
    // SAFETY: checked non-null.
    unsafe { out.write(root) };
    ERROR_SUCCESS
}

pub extern "win64" fn RegOpenKeyW(root: Hkey, sub: *const u16, out: *mut Hkey) -> u32 {
    open(root, sub, out)
}

pub extern "win64" fn RegOpenKeyExW(root: Hkey, sub: *const u16, _options: u32, _access: u32, out: *mut Hkey) -> u32 {
    open(root, sub, out)
}

pub extern "win64" fn RegQueryValueExW(
    key: Hkey,
    _name: *const u16,
    _reserved: *mut u32,
    _kind: *mut u32,
    _data: *mut u8,
    _size: *mut u32,
) -> u32 {
    if predefined(key) { ERROR_FILE_NOT_FOUND } else { ERROR_INVALID_HANDLE }
}

pub extern "win64" fn RegCloseKey(key: Hkey) -> u32 {
    if predefined(key) { ERROR_SUCCESS } else { ERROR_INVALID_HANDLE }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "advapi32-registry",
        dlls: &["advapi32.dll", "api-ms-win-core-registry-*"],
        exports: vec![
            hle_export!(RegOpenKeyW),
            hle_export!(RegOpenKeyExW),
            hle_export!(RegQueryValueExW),
            hle_export!(RegCloseKey),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_MACHINE: Hkey = 0xFFFF_FFFF_8000_0002_u64 as Hkey;

    #[test]
    fn every_key_and_value_is_missing() {
        let path: Vec<u16> = "Software\\Microsoft".encode_utf16().chain([0]).collect();
        let mut key = 0;
        assert_eq!(RegOpenKeyW(LOCAL_MACHINE, path.as_ptr(), &mut key), ERROR_FILE_NOT_FOUND);
        assert_eq!(RegOpenKeyExW(LOCAL_MACHINE, std::ptr::null(), 0, 1, &mut key), ERROR_SUCCESS);
        assert_eq!(key, LOCAL_MACHINE);
        let null = std::ptr::null_mut();
        assert_eq!(RegQueryValueExW(key, path.as_ptr(), null, null, null.cast(), null), ERROR_FILE_NOT_FOUND);
        assert_eq!(RegQueryValueExW(0x1234, path.as_ptr(), null, null, null.cast(), null), ERROR_INVALID_HANDLE);
        assert_eq!(RegCloseKey(key), ERROR_SUCCESS);
        assert_eq!(RegCloseKey(0x1234), ERROR_INVALID_HANDLE);
    }
}
