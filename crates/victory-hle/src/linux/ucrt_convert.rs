//! ucrtbase: text to number and back (`api-ms-win-crt-convert`). The
//! guest's `long` is 32 bits, so `strtol`/`strtoul` clamp at 32 bits and
//! `atol` is `atoi`. Out of range is the clamped value and ERANGE, never
//! undefined. Floating point is glibc's parser in the C locale (our process
//! never calls `setlocale`); the multibyte conversions are the C locale's:
//! a code unit up to 255 is that byte.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_char;

use super::ucrt_runtime::{EILSEQ, EINVAL, ERANGE, STRUNCATE, errno, fail, invalid, set_errno};
use super::ucrt_string::Unit;
use crate::{HleModule, hle_export};

fn is_space(c: u32) -> bool {
    c == 0x20 || (9..=13).contains(&c)
}

/// The UCRT's integer parser: white space, a sign, `0x` for base 16 or 0,
/// a leading 0 meaning octal for base 0. No digit at all (also a bare
/// "0x") gives 0 with `*end` the start. A minus sign negates an unsigned
/// result in range. Out of range: ERANGE and the limit by sign.
///
/// # Safety
/// `string` is null or NUL-terminated; `end` is null or writable.
pub(crate) unsafe fn parse_integer<T: Unit>(
    string: *const T,
    end: *mut *mut T,
    base: i32,
    bits: u32,
    signed: bool,
) -> u64 {
    let set_end = |p: *const T| {
        if !end.is_null() {
            // SAFETY: writable, per the caller.
            unsafe { *end = p.cast_mut() };
        }
    };
    set_end(string);
    if string.is_null() || !(base == 0 || (2..=36).contains(&base)) {
        invalid(EINVAL);
        return 0;
    }
    // SAFETY: NUL-terminated, per the caller.
    let at = |i: usize| -> u32 { unsafe { *string.add(i) }.into() };
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let negative = at(i) == u32::from(b'-');
    if negative || at(i) == u32::from(b'+') {
        i += 1;
    }
    let mut base = base as u64;
    if (base == 0 || base == 16) && at(i) == u32::from(b'0') && (at(i + 1) | 0x20) == u32::from(b'x') {
        base = 16;
        i += 2;
    } else if base == 0 {
        base = if at(i) == u32::from(b'0') { 8 } else { 10 };
    }
    let digit = |c: u32| -> u64 {
        match char::from_u32(c).and_then(|c| c.to_digit(36)) {
            Some(d) => d.into(),
            None => 99,
        }
    };
    let max = if bits == 64 { u64::MAX } else { u64::from(u32::MAX) };
    let (mut value, mut any, mut overflow) = (0u64, false, false);
    loop {
        let d = digit(at(i));
        if d >= base {
            break;
        }
        any = true;
        match value.checked_mul(base).and_then(|v| v.checked_add(d)).filter(|&v| v <= max) {
            Some(v) => value = v,
            None => overflow = true, // the rest of the digits are still consumed
        }
        i += 1;
    }
    if !any {
        return 0;
    }
    // SAFETY: i is inside the string.
    set_end(unsafe { string.add(i) });
    let signed_max = max >> 1;
    if overflow || (signed && value > signed_max + u64::from(negative)) {
        set_errno(ERANGE);
        return match (signed, negative) {
            (false, _) => max,
            (true, true) => (signed_max + 1).wrapping_neg() & max,
            (true, false) => signed_max,
        };
    }
    if negative { value.wrapping_neg() & max } else { value }
}

macro_rules! integer {
    ($name:ident, $unit:ty, $out:ty, $bits:expr, $signed:expr) => {
        pub extern "win64" fn $name(s: *const $unit, end: *mut *mut $unit, base: i32) -> $out {
            // SAFETY: the C contract: s is NUL-terminated, end writable.
            unsafe { parse_integer(s.cast::<u8>(), end.cast(), base, $bits, $signed) as $out }
        }
    };
}
integer!(strtol, c_char, i32, 32, true);
integer!(strtoul, c_char, u32, 32, false);
integer!(strtoll, c_char, i64, 64, true);
integer!(strtoull, c_char, u64, 64, false);
integer!(_strtoi64, c_char, i64, 64, true);
integer!(_strtoui64, c_char, u64, 64, false);

pub extern "win64" fn wcstoul(s: *const u16, end: *mut *mut u16, base: i32) -> u32 {
    // SAFETY: the C contract: s is NUL-terminated, end writable.
    unsafe { parse_integer(s, end, base, 32, false) as u32 }
}

pub extern "win64" fn wcstoull(s: *const u16, end: *mut *mut u16, base: i32) -> u64 {
    // SAFETY: the C contract: s is NUL-terminated, end writable.
    unsafe { parse_integer(s, end, base, 64, false) }
}

pub extern "win64" fn atoi(s: *const c_char) -> i32 {
    strtol(s, std::ptr::null_mut(), 10)
}

pub extern "win64" fn atoll(s: *const c_char) -> i64 {
    strtoll(s, std::ptr::null_mut(), 10)
}

pub extern "win64" fn _wtoi(s: *const u16) -> i32 {
    // SAFETY: the C contract: s is NUL-terminated.
    unsafe { parse_integer(s, std::ptr::null_mut(), 10, 32, true) as i32 }
}

/// glibc's `strtod`/`strtof` with the guest's errno.
fn parse_float<T>(s: *const c_char, end: *mut *mut c_char, parse: impl FnOnce() -> T, zero: T) -> T {
    if !end.is_null() {
        // SAFETY: writable, per the C contract.
        unsafe { *end = s.cast_mut() };
    }
    if s.is_null() {
        invalid(EINVAL);
        return zero;
    }
    // SAFETY: glibc's errno cell of this thread.
    unsafe { *libc::__errno_location() = 0 };
    let value = parse();
    // SAFETY: as above.
    if unsafe { *libc::__errno_location() } == libc::ERANGE {
        set_errno(ERANGE);
    }
    value
}

pub extern "win64" fn strtod(s: *const c_char, end: *mut *mut c_char) -> f64 {
    // SAFETY: s is NUL-terminated, end null or writable.
    parse_float(s, end, || unsafe { libc::strtod(s, end) }, 0.0)
}

pub extern "win64" fn strtof(s: *const c_char, end: *mut *mut c_char) -> f32 {
    // SAFETY: s is NUL-terminated, end null or writable.
    parse_float(s, end, || unsafe { libc::strtof(s, end) }, 0.0)
}

pub extern "win64" fn atof(s: *const c_char) -> f64 {
    strtod(s, std::ptr::null_mut())
}

/// The digits of `value` (lower case); a minus sign only in base 10.
fn integer_text(value: i32, radix: u32) -> Vec<u8> {
    let negative = radix == 10 && value < 0;
    let mut rest = if negative { value.unsigned_abs() } else { value as u32 };
    let mut digits = vec![];
    loop {
        digits.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(rest % radix) as usize]);
        rest /= radix;
        if rest == 0 {
            break;
        }
    }
    if negative {
        digits.push(b'-');
    }
    digits.reverse();
    digits
}

pub extern "win64" fn _itoa(value: i32, buffer: *mut c_char, radix: i32) -> *mut c_char {
    if buffer.is_null() || !(2..=36).contains(&radix) {
        invalid(EINVAL);
        return buffer;
    }
    let text = integer_text(value, radix as u32);
    // SAFETY: the C contract: the buffer holds 33 bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer.cast(), text.len());
        *buffer.add(text.len()) = 0;
    }
    buffer
}

pub extern "win64" fn _itow_s(value: i32, buffer: *mut u16, size: usize, radix: i32) -> i32 {
    if buffer.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    // SAFETY: size > 0.
    unsafe { *buffer = 0 };
    if size <= if value < 0 && radix == 10 { 2 } else { 1 } {
        return invalid(ERANGE);
    }
    if !(2..=36).contains(&radix) {
        return invalid(EINVAL);
    }
    let text = integer_text(value, radix as u32);
    if text.len() + 1 > size {
        return invalid(ERANGE);
    }
    for (i, &c) in text.iter().chain(&[0]).enumerate() {
        // SAFETY: i < text.len() + 1 <= size.
        unsafe { *buffer.add(i) = c.into() };
    }
    0
}

/// The byte as a wide character; only EOF is WEOF.
pub extern "win64" fn btowc(c: i32) -> u16 {
    if c == -1 { 0xFFFF } else { c as u8 as u16 }
}

/// The C locale's `wcstombs`: at most `count` bytes (no limit for a null
/// `dst`), the terminator only if it fits; the bytes before it, or -1 and
/// EILSEQ for a unit above 255.
unsafe fn to_bytes(dst: *mut u8, src: *const u16, count: usize) -> usize {
    let mut n = 0;
    while dst.is_null() || n < count {
        // SAFETY: NUL-terminated, per the caller.
        let c = unsafe { *src.add(n) };
        if c > 255 {
            fail(EILSEQ);
            return usize::MAX;
        }
        if !dst.is_null() {
            // SAFETY: n < count.
            unsafe { *dst.add(n) = c as u8 };
        }
        if c == 0 {
            break;
        }
        n += 1;
    }
    n
}

pub extern "win64" fn wcstombs(dst: *mut c_char, src: *const u16, count: usize) -> usize {
    if !dst.is_null() && count == 0 {
        return 0;
    }
    if src.is_null() {
        invalid(EINVAL);
        return usize::MAX;
    }
    // SAFETY: src is NUL-terminated; dst holds count bytes.
    unsafe { to_bytes(dst.cast(), src, count) }
}

pub extern "win64" fn wcstombs_s(
    converted: *mut usize,
    dst: *mut c_char,
    size: usize,
    src: *const u16,
    count: usize,
) -> i32 {
    if dst.is_null() != (size == 0) {
        return invalid(EINVAL);
    }
    if !dst.is_null() {
        // SAFETY: size > 0.
        unsafe { *dst = 0 };
    }
    if !converted.is_null() {
        // SAFETY: the title's size_t.
        unsafe { *converted = 0 };
    }
    if src.is_null() {
        return invalid(EINVAL);
    }
    let limit = count.min(size);
    // SAFETY: src is NUL-terminated; dst holds size bytes.
    let mut n = unsafe { to_bytes(dst.cast(), src, limit) };
    if n == usize::MAX {
        if !dst.is_null() {
            // SAFETY: size > 0.
            unsafe { *dst = 0 };
        }
        return errno();
    }
    n += 1; // the terminator
    let mut result = 0;
    if !dst.is_null() {
        if n > size {
            if count != usize::MAX {
                // SAFETY: size > 0.
                unsafe { *dst = 0 };
                return invalid(ERANGE);
            }
            n = size;
            result = STRUNCATE;
        }
        // SAFETY: n <= size.
        unsafe { *dst.add(n - 1) = 0 };
    }
    if !converted.is_null() {
        // SAFETY: the title's size_t.
        unsafe { *converted = n };
    }
    result
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-convert",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-convert-*"],
        exports: vec![
            hle_export!(strtol),
            hle_export!(strtoul),
            hle_export!(strtoll),
            hle_export!(strtoull),
            hle_export!(_strtoi64),
            hle_export!(_strtoui64),
            hle_export!(wcstoul),
            hle_export!(wcstoull),
            hle_export!(atoi),
            crate::HleExport {
                name: "atol",
                address: atoi as *const () as usize,
            },
            hle_export!(atoll),
            hle_export!(_wtoi),
            hle_export!(strtod),
            hle_export!(strtof),
            hle_export!(atof),
            hle_export!(_itoa),
            hle_export!(_itow_s),
            hle_export!(btowc),
            hle_export!(wcstombs),
            hle_export!(wcstombs_s),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_string::w;

    fn c(s: &str) -> std::ffi::CString {
        std::ffi::CString::new(s).unwrap()
    }

    #[test]
    fn integers_parse_and_clamp_like_the_ucrt() {
        let mut end = std::ptr::null_mut();
        let s = c("  -0x1Fz");
        assert_eq!(strtol(s.as_ptr(), &mut end, 0), -31);
        assert_eq!(end as usize - s.as_ptr() as usize, 7);
        let s = c("0x");
        assert_eq!(strtol(s.as_ptr(), &mut end, 16), 0);
        assert_eq!(end.cast_const(), s.as_ptr());
        assert_eq!(strtol(c("017").as_ptr(), std::ptr::null_mut(), 0), 15);
        set_errno(0);
        assert_eq!(strtol(c("3000000000").as_ptr(), std::ptr::null_mut(), 10), i32::MAX);
        assert_eq!(errno(), ERANGE);
        assert_eq!(strtol(c("-3000000000").as_ptr(), std::ptr::null_mut(), 10), i32::MIN);
        set_errno(0);
        assert_eq!(strtoul(c("-1").as_ptr(), std::ptr::null_mut(), 10), u32::MAX);
        assert_eq!(errno(), 0);
        assert_eq!(strtoul(c("4294967296").as_ptr(), std::ptr::null_mut(), 10), u32::MAX);
        assert_eq!(errno(), ERANGE);
        assert_eq!(_strtoi64(c("-9223372036854775808").as_ptr(), std::ptr::null_mut(), 10), i64::MIN);
        assert_eq!(_strtoui64(c("zz").as_ptr(), std::ptr::null_mut(), 36), 1295);
        assert_eq!(atoi(c(" +42abc").as_ptr()), 42);
        assert_eq!(_wtoi(w("-17").as_ptr()), -17);
        assert_eq!(wcstoull(w("ffffffffffffffff").as_ptr(), std::ptr::null_mut(), 16), u64::MAX);
        assert_eq!(strtol(c("1").as_ptr(), std::ptr::null_mut(), 1), 0);
        assert_eq!(errno(), EINVAL);
    }

    #[test]
    fn floats_and_text() {
        let mut end = std::ptr::null_mut();
        let s = c("1.5e3x");
        assert_eq!(strtod(s.as_ptr(), &mut end), 1500.0);
        assert_eq!(end as usize - s.as_ptr() as usize, 5);
        assert_eq!(strtof(c("0x1p-2").as_ptr(), std::ptr::null_mut()), 0.25);
        set_errno(0);
        assert_eq!(atof(c("1e999").as_ptr()), f64::INFINITY);
        assert_eq!(errno(), ERANGE);
        let mut buf = [0 as c_char; 40];
        _itoa(-255, buf.as_mut_ptr(), 16);
        // SAFETY: NUL-terminated.
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }, c"ffffff01");
        _itoa(-255, buf.as_mut_ptr(), 10);
        // SAFETY: NUL-terminated.
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }, c"-255");
        let mut wbuf = [0u16; 4];
        assert_eq!(_itow_s(123, wbuf.as_mut_ptr(), 4, 10), 0);
        assert_eq!(wbuf, [49, 50, 51, 0]);
        assert_eq!(_itow_s(1234, wbuf.as_mut_ptr(), 4, 10), ERANGE);
        assert_eq!(wbuf[0], 0);
    }

    #[test]
    fn wide_to_multibyte_in_the_c_locale() {
        let mut buf = [0x55 as c_char; 4];
        assert_eq!(wcstombs(std::ptr::null_mut(), w("abc").as_ptr(), 0), 3);
        assert_eq!(wcstombs(buf.as_mut_ptr(), w("abcdef").as_ptr(), 4), 4);
        assert_eq!(wcstombs(buf.as_mut_ptr(), w("\u{e9}").as_ptr(), 4), 1);
        assert_eq!(buf[0] as u8, 0xE9);
        assert_eq!(wcstombs(buf.as_mut_ptr(), w("\u{4e2d}").as_ptr(), 4), usize::MAX);
        assert_eq!(errno(), EILSEQ);
        let mut n = 0;
        assert_eq!(wcstombs_s(&mut n, buf.as_mut_ptr(), 4, w("abc").as_ptr(), usize::MAX), 0);
        assert_eq!(n, 4);
        assert_eq!(wcstombs_s(&mut n, buf.as_mut_ptr(), 4, w("abcdef").as_ptr(), usize::MAX), STRUNCATE);
        assert_eq!((n, buf[3]), (4, 0));
        assert_eq!(wcstombs_s(&mut n, buf.as_mut_ptr(), 4, w("abcdef").as_ptr(), 5), ERANGE);
        assert_eq!(wcstombs_s(&mut n, buf.as_mut_ptr(), 4, w("abcdef").as_ptr(), 2), 0);
        assert_eq!(n, 3);
        assert_eq!(wcstombs_s(&mut n, std::ptr::null_mut(), 0, w("abcdef").as_ptr(), 0), 0);
        assert_eq!(n, 7);
        assert_eq!(btowc(-1), 0xFFFF);
    }
}
