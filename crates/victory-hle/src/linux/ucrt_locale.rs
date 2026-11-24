//! ucrtbase: the locale (`api-ms-win-crt-locale`). There is one locale,
//! "C": `setlocale` accepts the names that mean it (or the user default)
//! and installs nothing else. The character class table is the UCRT's,
//! not glibc's: other bits, entries for -1 (EOF) to 255, and no class for
//! a byte above 0x7F. msvcp140 copies it into its `ctype<char>` facet.
#![allow(non_upper_case_globals)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_char;
use std::sync::atomic::{AtomicI32, Ordering};

use super::ucrt_runtime::{EINVAL, RecursiveLock, invalid};
use crate::{HleModule, hle_export};

// ctype.h's bits.
pub(crate) const UPPER: u16 = 0x1;
pub(crate) const LOWER: u16 = 0x2;
pub(crate) const DIGIT: u16 = 0x4;
pub(crate) const SPACE: u16 = 0x8;
const PUNCT: u16 = 0x10;
const CONTROL: u16 = 0x20;
const BLANK: u16 = 0x40;
pub(crate) const HEX: u16 = 0x80;
pub(crate) const ALPHA: u16 = 0x100 | UPPER | LOWER;

/// The C locale's table from character -128 (a negative `char` indexes
/// it; those and EOF are 0).
static CLASSES: [u16; 384] = {
    let mut t = [0u16; 384];
    let mut c = 0;
    while c < 128 {
        let b = c as u8;
        t[128 + c] = if b == b'\t' {
            SPACE | CONTROL | BLANK
        } else if b >= 0x0A && b <= 0x0D {
            SPACE | CONTROL
        } else if b < 0x20 || b == 0x7F {
            CONTROL
        } else if b == b' ' {
            SPACE | BLANK
        } else if b.is_ascii_digit() {
            DIGIT | HEX
        } else if b.is_ascii_uppercase() {
            0x100 | UPPER | if b <= b'F' { HEX } else { 0 }
        } else if b.is_ascii_lowercase() {
            0x100 | LOWER | if b <= b'f' { HEX } else { 0 }
        } else {
            PUNCT
        };
        c += 1;
    }
    t
};

/// The class bits of `c` (-1..=255; anything else has none).
pub(crate) fn ctype(c: i32) -> u16 {
    if (-1..=255).contains(&c) { CLASSES[(128 + c) as usize] } else { 0 }
}

pub extern "win64" fn __pctype_func() -> *const u16 {
    CLASSES[128..].as_ptr()
}

fn means_c_locale(name: &[u8]) -> bool {
    let lower: Vec<u8> = name.to_ascii_lowercase();
    lower.is_empty()
        || name == b"C"
        || lower.starts_with(b"english")
        || (lower.starts_with(b"en") && matches!(lower.get(2), None | Some(b'-' | b'_' | b'.')))
}

/// A query and every accepted name answer "C"; other locales do not
/// exist (null). Categories LC_ALL 0 to LC_TIME 5.
pub extern "win64" fn setlocale(category: i32, name: *const c_char) -> *const c_char {
    if !(0..=5).contains(&category) {
        invalid(EINVAL);
        return std::ptr::null();
    }
    // SAFETY: null or NUL-terminated, per the C contract.
    if name.is_null() || means_c_locale(unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes()) {
        c"C".as_ptr()
    } else {
        std::ptr::null()
    }
}

/// `struct lconv` of the UCRT.
#[repr(C)]
pub struct Lconv {
    narrow: [*const c_char; 10],
    numbers: [c_char; 8],
    wide: [*const u16; 8],
}

// SAFETY: points to immutable statics only.
unsafe impl Sync for Lconv {}

pub extern "win64" fn localeconv() -> *const Lconv {
    static POINT: [u16; 2] = [b'.' as u16, 0];
    static NONE: [u16; 1] = [0];
    static C: Lconv = {
        let none: *const c_char = c"".as_ptr();
        let mut narrow = [none; 10];
        narrow[0] = c".".as_ptr();
        let mut wide = [NONE.as_ptr(); 8];
        wide[0] = POINT.as_ptr();
        Lconv {
            narrow,
            numbers: [127; 8], // CHAR_MAX: not available
            wide,
        }
    };
    &C
}

/// In the C locale: code page 0 (CP_ACP), single bytes, and no locale name
/// for any category, which is how msvcp140 recognises it.
pub extern "win64" fn ___lc_codepage_func() -> u32 {
    0
}

pub extern "win64" fn ___lc_collate_cp_func() -> u32 {
    0
}

pub extern "win64" fn ___mb_cur_max_func() -> i32 {
    1
}

pub extern "win64" fn ___lc_locale_name_func() -> *const *const u16 {
    static NAMES: [usize; 6] = [0; 6];
    NAMES.as_ptr().cast()
}

/// The lock msvcp140 holds while it builds a locale.
static LOCALE_LOCK: RecursiveLock = RecursiveLock::new();

pub extern "win64" fn _lock_locales() {
    LOCALE_LOCK.lock();
}

pub extern "win64" fn _unlock_locales() {
    LOCALE_LOCK.unlock();
}

/// Per-thread or global `setlocale` (1, 2): remembered and answered, with
/// one locale to set. 0 asks; -1 is the start-up's "leave it alone".
pub extern "win64" fn _configthreadlocale(flag: i32) -> i32 {
    thread_local! {
        static MODE: AtomicI32 = const { AtomicI32::new(2) };
    }
    MODE.with(|mode| {
        let previous = mode.load(Ordering::Relaxed);
        match flag {
            1 | 2 => mode.store(flag, Ordering::Relaxed),
            0 | -1 => {}
            _ => {
                invalid(EINVAL);
                return -1;
            }
        }
        previous
    })
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-locale",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-locale-*"],
        exports: vec![
            hle_export!(__pctype_func),
            hle_export!(setlocale),
            hle_export!(localeconv),
            hle_export!(___lc_codepage_func),
            hle_export!(___lc_collate_cp_func),
            hle_export!(___mb_cur_max_func),
            hle_export!(___lc_locale_name_func),
            hle_export!(_lock_locales),
            hle_export!(_unlock_locales),
            hle_export!(_configthreadlocale),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_class_table_is_the_ucrts() {
        let table = __pctype_func();
        // SAFETY: the table covers -128..=255.
        let at = |c: isize| unsafe { *table.offset(c) };
        assert_eq!(at(b'a' as isize), 0x182);
        assert_eq!(at(b'G' as isize), 0x101);
        assert_eq!(at(b'7' as isize), 0x84);
        assert_eq!(at(b'\t' as isize), 0x68);
        assert_eq!(at(b' ' as isize), 0x48);
        assert_eq!(at(b'!' as isize), 0x10);
        assert_eq!((at(-1), at(0xE9), at(-100)), (0, 0, 0));
    }

    #[test]
    fn only_the_c_locale_exists() {
        let name = |s: &std::ffi::CStr| {
            let p = setlocale(0, s.as_ptr());
            // SAFETY: a NUL-terminated static.
            (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_owned())
        };
        assert_eq!(name(c"").as_deref(), Some(c"C"));
        assert_eq!(name(c"en-US").as_deref(), Some(c"C"));
        assert_eq!(name(c"de-DE"), None);
        assert!(setlocale(9, std::ptr::null()).is_null());
        assert_eq!(std::mem::size_of::<Lconv>(), 152);
        // SAFETY: a static lconv.
        let point = unsafe { *(*localeconv()).narrow[0] };
        assert_eq!(point as u8, b'.');
        assert_eq!(_configthreadlocale(1), 2);
        assert_eq!(_configthreadlocale(0), 1);
    }
}
