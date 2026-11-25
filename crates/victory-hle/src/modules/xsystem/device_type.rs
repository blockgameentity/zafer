//! The device type is not a runtime call: code linked into the title reads
//! `ConsoleMode` from this key (`RegOpenKeyW`, `RegQueryValueExW`) and
//! takes a missing value for a PC. Other keys go to the host's registry
//! (on Linux, our empty one).
//!
//! Split out of [`super`] so that file stays under 400 lines.
    #![allow(non_snake_case)]

#[allow(unused_imports)]
use super::*;

    use std::ffi::c_void;

    use victory_gpu::console::{self, Console};

    use crate::host::host;
    use crate::{HleModule, hle_export};

    type Key = *mut c_void;
    const LOCAL_MACHINE: Key = 0xffff_ffff_8000_0002_u64 as Key;
    const KEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\OEM";
    const VALUE: &str = "ConsoleMode";
    const SUCCESS: u32 = 0;
    const FILE_NOT_FOUND: u32 = 2;
    const INVALID_PARAMETER: u32 = 87;
    const MORE_DATA: u32 = 234;
    const DWORD: u32 = 4;

    /// Stands for the key; never a host handle value.
    static OEM: u8 = 0;
    fn oem() -> Key {
        (&raw const OEM).cast_mut().cast()
    }

    /// What the title's code maps to each model's device type.
    pub(super) fn console_mode(console: Console) -> u32 {
        match console {
            Console::SeriesX => 0x64,
            Console::SeriesS => 0x60,
        }
    }

    fn is(text: *const u16, expected: &str) -> bool {
        if text.is_null() {
            return false;
        }
        // SAFETY: the title passes a NUL-terminated string.
        let text = unsafe {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
        };
        text.eq_ignore_ascii_case(expected)
    }

    /// Opens our key for its path; the key has no subkeys. `None`: not
    /// ours, ask the host.
    fn open(root: Key, sub: *const u16, out: *mut Key) -> Option<u32> {
        if root == oem() {
            return Some(FILE_NOT_FOUND);
        }
        if root == LOCAL_MACHINE && is(sub, KEY) && !out.is_null() {
            // SAFETY: checked non-null; the title passes room for a key.
            unsafe { out.write(oem()) };
            return Some(SUCCESS);
        }
        None
    }

    // Our titles import only these registry functions (and `RegOpenKeyExW`
    // below), so no other call can reach the host with our key.
    pub extern "win64" fn RegOpenKeyW(root: Key, sub: *const u16, out: *mut Key) -> u32 {
        open(root, sub, out).unwrap_or_else(|| {
            host!("advapi32.dll", "RegOpenKeyW": extern "win64" fn(Key, *const u16, *mut Key) -> u32)(
                root, sub, out,
            )
        })
    }

    pub extern "win64" fn RegOpenKeyExW(
        root: Key,
        sub: *const u16,
        options: u32,
        access: u32,
        out: *mut Key,
    ) -> u32 {
        open(root, sub, out).unwrap_or_else(|| {
            host!("advapi32.dll", "RegOpenKeyExW": extern "win64" fn(
                Key, *const u16, u32, u32, *mut Key,
            ) -> u32)(root, sub, options, access, out)
        })
    }

    pub extern "win64" fn RegQueryValueExW(
        key: Key,
        name: *const u16,
        reserved: *mut u32,
        kind: *mut u32,
        data: *mut u8,
        size: *mut u32,
    ) -> u32 {
        if key != oem() {
            return host!("advapi32.dll", "RegQueryValueExW": extern "win64" fn(
                Key, *const u16, *mut u32, *mut u32, *mut u8, *mut u32,
            ) -> u32)(key, name, reserved, kind, data, size);
        }
        if !is(name, VALUE) {
            return FILE_NOT_FOUND;
        }
        if !data.is_null() && size.is_null() {
            return INVALID_PARAMETER;
        }
        // SAFETY: the title passes valid or null pointers, sized by `size`.
        unsafe {
            if !kind.is_null() {
                kind.write(DWORD);
            }
            if size.is_null() {
                return SUCCESS;
            }
            let room = size.read();
            size.write(4);
            if data.is_null() {
                return SUCCESS;
            }
            if room < 4 {
                return MORE_DATA;
            }
            data.cast::<u32>()
                .write_unaligned(console_mode(console::current()));
        }
        tracing::debug!(console = console::current().name(), "device type read");
        SUCCESS
    }

    pub extern "win64" fn RegCloseKey(key: Key) -> u32 {
        if key == oem() {
            return SUCCESS;
        }
        host!("advapi32.dll", "RegCloseKey": extern "win64" fn(Key) -> u32)(key)
    }

    pub(crate) fn module() -> HleModule {
        HleModule {
            name: "device type",
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

        fn w(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        #[test]
        fn titles_read_the_chosen_model() {
            assert_eq!(console_mode(Console::SeriesX), 0x64);
            assert_eq!(console_mode(Console::SeriesS), 0x60);
            let mut key = std::ptr::null_mut();
            let path = w(&KEY.to_ascii_lowercase());
            assert_eq!(RegOpenKeyW(LOCAL_MACHINE, path.as_ptr(), &mut key), SUCCESS);
            let (mut kind, mut value, mut size) = (0, 0u32, 4);
            let name = w(VALUE);
            let query = |data: *mut u32, size: *mut u32, kind: *mut u32| {
                RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    kind,
                    data.cast(),
                    size,
                )
            };
            assert_eq!(query(&mut value, &mut size, &mut kind), SUCCESS);
            assert_eq!((kind, size), (DWORD, 4));
            assert_eq!(value, console_mode(console::current()));
            let mut small = 2;
            assert_eq!(query(&mut value, &mut small, &mut kind), MORE_DATA);
            assert_eq!(small, 4);
            let other = w("Other");
            assert_eq!(
                RegQueryValueExW(
                    key,
                    other.as_ptr(),
                    std::ptr::null_mut(),
                    &mut kind,
                    std::ptr::null_mut(),
                    &mut size
                ),
                FILE_NOT_FOUND
            );
            let mut sub = std::ptr::null_mut();
            assert_eq!(
                RegOpenKeyExW(key, other.as_ptr(), 0, 1, &mut sub),
                FILE_NOT_FOUND
            );
            assert_eq!(RegCloseKey(key), SUCCESS);
            let mut again = std::ptr::null_mut();
            assert_eq!(
                RegOpenKeyExW(LOCAL_MACHINE, path.as_ptr(), 0, 1, &mut again),
                SUCCESS
            );
            assert_eq!(again, oem());
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn other_keys_reach_the_empty_registry() {
            let mut key = std::ptr::null_mut();
            let path = w(r"Software\Microsoft\Windows NT\CurrentVersion");
            assert_eq!(RegOpenKeyW(LOCAL_MACHINE, path.as_ptr(), &mut key), FILE_NOT_FOUND);
            assert_eq!(RegCloseKey(LOCAL_MACHINE), SUCCESS);
        }

        #[cfg(windows)]
        #[test]
        fn other_keys_reach_the_host() {
            let mut key = std::ptr::null_mut();
            let path = w(r"Software\Microsoft\Windows NT\CurrentVersion");
            assert_eq!(RegOpenKeyW(LOCAL_MACHINE, path.as_ptr(), &mut key), SUCCESS);
            assert_ne!(key, oem());
            let (mut size, name) = (0, w("ProductName"));
            assert_eq!(
                RegQueryValueExW(
                    key,
                    name.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size
                ),
                SUCCESS
            );
            assert!(size > 0);
            assert_eq!(RegCloseKey(key), SUCCESS);
        }
    }
