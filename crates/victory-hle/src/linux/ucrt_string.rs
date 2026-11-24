//! ucrtbase: strings (`api-ms-win-crt-string`), narrow and 16-bit wide,
//! in the C locale: case is ASCII, a byte above 0x7F is just a byte.
//!
//! Where the UCRT differs from glibc it is the UCRT here: `strcmp` and
//! `wcscmp` answer -1, 0 or 1, `strncmp`, `wcsncmp` and the `_stricmp`
//! family the difference; the `_s` functions validate, empty the
//! destination on failure and report through the invalid-parameter path,
//! except `_TRUNCATE`'s STRUNCATE.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_char;

use super::ucrt_locale::{ALPHA, DIGIT, HEX, LOWER, SPACE, UPPER, ctype};
use super::ucrt_runtime::{EINVAL, ERANGE, STRUNCATE, invalid};
use crate::{HleModule, hle_export};

/// A narrow (`u8`) or wide (`u16`) code unit.
pub(crate) trait Unit: Copy + Eq + Default + Into<u32> + 'static {
    fn from_u32(value: u32) -> Self;
}

impl Unit for u8 {
    fn from_u32(value: u32) -> Self {
        value as u8
    }
}

impl Unit for u16 {
    fn from_u32(value: u32) -> Self {
        value as u16
    }
}

/// The length of a NUL-terminated string.
///
/// # Safety
/// `s` is NUL-terminated.
pub(crate) unsafe fn len<T: Unit>(s: *const T) -> usize {
    let mut n = 0;
    // SAFETY: NUL-terminated, per the caller.
    while unsafe { *s.add(n) } != T::default() {
        n += 1;
    }
    n
}

/// A NUL-terminated string without its terminator.
///
/// # Safety
/// `s` is NUL-terminated and outlives the slice.
pub(crate) unsafe fn units<'a, T: Unit>(s: *const T) -> &'a [T] {
    // SAFETY: per the caller.
    unsafe { std::slice::from_raw_parts(s, len(s)) }
}

/// At most `max` units of `s`, up to its terminator.
///
/// # Safety
/// `s` is readable up to its terminator or `max` units.
pub(crate) unsafe fn units_max<'a, T: Unit>(s: *const T, max: usize) -> &'a [T] {
    let mut n = 0;
    // SAFETY: per the caller.
    while n < max && unsafe { *s.add(n) } != T::default() {
        n += 1;
    }
    // SAFETY: the n units just read.
    unsafe { std::slice::from_raw_parts(s, n) }
}

fn ascii_lower<T: Unit>(c: T) -> u32 {
    let c = c.into();
    if (u32::from(b'A')..=u32::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

fn sign(v: i64) -> i32 {
    v.signum() as i32
}

// ---- the shared bodies ---------------------------------------------------

/// `strcmp`/`wcscmp`: -1, 0 or 1.
unsafe fn compare<T: Unit>(a: *const T, b: *const T, count: usize, difference: bool) -> i32 {
    for i in 0..count {
        // SAFETY: both strings are readable up to their terminators.
        let (x, y): (u32, u32) = unsafe { ((*a.add(i)).into(), (*b.add(i)).into()) };
        if x != y || x == 0 {
            let d = i64::from(x) - i64::from(y);
            return if difference { d as i32 } else { sign(d) };
        }
    }
    0
}

/// `_stricmp` family: the difference of the ASCII-folded units.
unsafe fn compare_folded<T: Unit>(a: *const T, b: *const T, count: usize) -> i32 {
    for i in 0..count {
        // SAFETY: both strings are readable up to their terminators.
        let (x, y) = unsafe { (ascii_lower(*a.add(i)), ascii_lower(*b.add(i))) };
        if x != y || x == 0 {
            return x as i32 - y as i32;
        }
    }
    0
}

/// `strcpy_s`/`wcscpy_s`.
unsafe fn copy_s<T: Unit>(dst: *mut T, size: usize, src: *const T) -> i32 {
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if src.is_null() {
        // SAFETY: size > 0.
        unsafe { *dst = T::default() };
        return invalid(EINVAL);
    }
    for i in 0..size {
        // SAFETY: i < size; src is NUL-terminated.
        unsafe {
            let c = *src.add(i);
            *dst.add(i) = c;
            if c == T::default() {
                return 0;
            }
        }
    }
    // SAFETY: size > 0.
    unsafe { *dst = T::default() };
    invalid(ERANGE)
}

/// `strcat_s`/`wcscat_s`; `count` limits the units taken from `src`
/// (`strncat_s`), `usize::MAX` being `_TRUNCATE`.
unsafe fn cat_s<T: Unit>(dst: *mut T, size: usize, src: *const T, count: Option<usize>) -> i32 {
    if count == Some(0) && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if src.is_null() && count != Some(0) {
        // SAFETY: size > 0.
        unsafe { *dst = T::default() };
        return invalid(EINVAL);
    }
    let mut at = 0;
    // SAFETY: at < size.
    while at < size && unsafe { *dst.add(at) } != T::default() {
        at += 1;
    }
    if at == size {
        // SAFETY: size > 0.
        unsafe { *dst = T::default() };
        return invalid(EINVAL);
    }
    let truncate = count == Some(usize::MAX);
    let mut left = count.filter(|_| !truncate);
    let mut from = 0;
    while at < size {
        if left == Some(0) {
            // SAFETY: at < size.
            unsafe { *dst.add(at) = T::default() };
            return 0;
        }
        // SAFETY: at < size; src is readable up to its terminator or count.
        let c = unsafe { *src.add(from) };
        // SAFETY: at < size.
        unsafe { *dst.add(at) = c };
        if c == T::default() {
            return 0;
        }
        at += 1;
        from += 1;
        left = left.map(|n| n - 1);
    }
    if truncate {
        // SAFETY: size > 0.
        unsafe { *dst.add(size - 1) = T::default() };
        return STRUNCATE;
    }
    // SAFETY: size > 0.
    unsafe { *dst = T::default() };
    invalid(ERANGE)
}

/// `strncpy_s`/`wcsncpy_s`: `count` may be `_TRUNCATE`.
unsafe fn ncopy_s<T: Unit>(dst: *mut T, size: usize, src: *const T, count: usize) -> i32 {
    if count == 0 && dst.is_null() && size == 0 {
        return 0;
    }
    if dst.is_null() || size == 0 {
        return invalid(EINVAL);
    }
    if count == 0 {
        // SAFETY: size > 0.
        unsafe { *dst = T::default() };
        return 0;
    }
    if src.is_null() {
        // SAFETY: size > 0.
        unsafe { *dst = T::default() };
        return invalid(EINVAL);
    }
    let mut i = 0;
    while i < size && i < count {
        // SAFETY: i < size; src is readable up to its terminator or count.
        unsafe {
            let c = *src.add(i);
            *dst.add(i) = c;
            if c == T::default() {
                return 0;
            }
        }
        i += 1;
    }
    if i < size {
        // SAFETY: i < size.
        unsafe { *dst.add(i) = T::default() };
        return 0;
    }
    if count == usize::MAX {
        // SAFETY: size > 0.
        unsafe { *dst.add(size - 1) = T::default() };
        return STRUNCATE;
    }
    // SAFETY: size > 0.
    unsafe { *dst = T::default() };
    invalid(ERANGE)
}

/// `strtok_s`/`wcstok_s`/`strtok`.
unsafe fn tok<T: Unit>(string: *mut T, delimiters: *const T, context: *mut *mut T) -> *mut T {
    // SAFETY: checked for null below before the read.
    if context.is_null() || delimiters.is_null() || (string.is_null() && unsafe { *context }.is_null()) {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: NUL-terminated, per the C contract.
    let delimiters = unsafe { units(delimiters) };
    // SAFETY: context checked above.
    let mut p = if string.is_null() { unsafe { *context } } else { string };
    // SAFETY: p walks a NUL-terminated string.
    unsafe {
        while *p != T::default() && delimiters.contains(&*p) {
            p = p.add(1);
        }
        let token = p;
        while *p != T::default() && !delimiters.contains(&*p) {
            p = p.add(1);
        }
        let found = p != token;
        if *p != T::default() {
            *p = T::default();
            p = p.add(1);
        }
        *context = p;
        if found { token } else { std::ptr::null_mut() }
    }
}

// ---- narrow --------------------------------------------------------------

pub extern "win64" fn strlen(s: *const c_char) -> usize {
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { len(s.cast::<u8>()) }
}

pub extern "win64" fn __strncnt(s: *const c_char, count: usize) -> usize {
    // SAFETY: readable up to its terminator or count.
    unsafe { units_max(s.cast::<u8>(), count) }.len()
}

pub extern "win64" fn strcmp(a: *const c_char, b: *const c_char) -> i32 {
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { compare(a.cast::<u8>(), b.cast::<u8>(), usize::MAX, false) }
}

pub extern "win64" fn strncmp(a: *const c_char, b: *const c_char, count: usize) -> i32 {
    // SAFETY: readable up to their terminators or count.
    unsafe { compare(a.cast::<u8>(), b.cast::<u8>(), count, true) }
}

/// `_NLSCMPERROR`.
const NLS_ERROR: i32 = 0x7FFF_FFFF;

pub extern "win64" fn _stricmp(a: *const c_char, b: *const c_char) -> i32 {
    if a.is_null() || b.is_null() {
        invalid(EINVAL);
        return NLS_ERROR;
    }
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { compare_folded(a.cast::<u8>(), b.cast::<u8>(), usize::MAX) }
}

pub extern "win64" fn _strnicmp(a: *const c_char, b: *const c_char, count: usize) -> i32 {
    if count == 0 {
        return 0;
    }
    if a.is_null() || b.is_null() {
        invalid(EINVAL);
        return NLS_ERROR;
    }
    // SAFETY: readable up to their terminators or count.
    unsafe { compare_folded(a.cast::<u8>(), b.cast::<u8>(), count) }
}

pub extern "win64" fn strcpy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    // SAFETY: the C contract: src fits dst.
    unsafe { std::ptr::copy(src, dst, strlen(src) + 1) };
    dst
}

pub extern "win64" fn strcat(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    // SAFETY: dst is NUL-terminated.
    strcpy(unsafe { dst.add(strlen(dst)) }, src);
    dst
}

/// Copies at most `count` bytes and pads the rest of them with zeros.
pub extern "win64" fn strncpy(dst: *mut c_char, src: *const c_char, count: usize) -> *mut c_char {
    let n = __strncnt(src, count);
    // SAFETY: the C contract: dst holds count bytes.
    unsafe {
        std::ptr::copy(src, dst, n);
        std::ptr::write_bytes(dst.add(n), 0, count - n);
    }
    dst
}

pub extern "win64" fn strncat(dst: *mut c_char, src: *const c_char, count: usize) -> *mut c_char {
    let n = __strncnt(src, count);
    // SAFETY: the C contract: dst has room for n more bytes and a NUL.
    unsafe {
        let end = dst.add(strlen(dst));
        std::ptr::copy(src, end, n);
        *end.add(n) = 0;
    }
    dst
}

pub extern "win64" fn strcpy_s(dst: *mut c_char, size: usize, src: *const c_char) -> i32 {
    // SAFETY: validated inside; dst holds size bytes.
    unsafe { copy_s(dst.cast::<u8>(), size, src.cast::<u8>()) }
}

pub extern "win64" fn strcat_s(dst: *mut c_char, size: usize, src: *const c_char) -> i32 {
    // SAFETY: validated inside; dst holds size bytes.
    unsafe { cat_s(dst.cast::<u8>(), size, src.cast::<u8>(), None) }
}

pub extern "win64" fn strncpy_s(dst: *mut c_char, size: usize, src: *const c_char, count: usize) -> i32 {
    // SAFETY: validated inside; dst holds size bytes.
    unsafe { ncopy_s(dst.cast::<u8>(), size, src.cast::<u8>(), count) }
}

fn byte_set(set: *const c_char) -> [bool; 256] {
    let mut table = [false; 256];
    // SAFETY: NUL-terminated, per the C contract.
    for &c in unsafe { units(set.cast::<u8>()) } {
        table[c as usize] = true;
    }
    table
}

pub extern "win64" fn strspn(s: *const c_char, set: *const c_char) -> usize {
    let table = byte_set(set);
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { units(s.cast::<u8>()) }.iter().take_while(|&&c| table[c as usize]).count()
}

pub extern "win64" fn strcspn(s: *const c_char, set: *const c_char) -> usize {
    let table = byte_set(set);
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { units(s.cast::<u8>()) }.iter().take_while(|&&c| !table[c as usize]).count()
}

pub extern "win64" fn strpbrk(s: *const c_char, set: *const c_char) -> *mut c_char {
    let n = strcspn(s, set);
    // SAFETY: n <= strlen(s).
    if unsafe { *s.add(n) } == 0 {
        std::ptr::null_mut()
    } else {
        s.wrapping_add(n).cast_mut()
    }
}

pub extern "win64" fn strtok_s(s: *mut c_char, delimiters: *const c_char, context: *mut *mut c_char) -> *mut c_char {
    // SAFETY: validated inside.
    unsafe { tok(s.cast::<u8>(), delimiters.cast::<u8>(), context.cast::<*mut u8>()) }.cast()
}

/// `strtok`'s context lives in the calling thread, as in the UCRT.
pub extern "win64" fn strtok(s: *mut c_char, delimiters: *const c_char) -> *mut c_char {
    thread_local! {
        static CONTEXT: std::cell::Cell<*mut c_char> = const { std::cell::Cell::new(std::ptr::null_mut()) };
    }
    if s.is_null() && CONTEXT.get().is_null() {
        return std::ptr::null_mut();
    }
    CONTEXT.with(|c| strtok_s(s, delimiters, c.as_ptr()))
}

// ---- wide ----------------------------------------------------------------

pub extern "win64" fn wcslen(s: *const u16) -> usize {
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { len(s) }
}

pub extern "win64" fn wcsnlen(s: *const u16, count: usize) -> usize {
    // SAFETY: readable up to its terminator or count.
    unsafe { units_max(s, count) }.len()
}

pub extern "win64" fn wcscmp(a: *const u16, b: *const u16) -> i32 {
    // SAFETY: NUL-terminated, per the C contract.
    unsafe { compare(a, b, usize::MAX, false) }
}

pub extern "win64" fn wcsncmp(a: *const u16, b: *const u16, count: usize) -> i32 {
    // SAFETY: readable up to their terminators or count.
    unsafe { compare(a, b, count, true) }
}

/// The C locale collates by code unit.
pub extern "win64" fn wcscoll(a: *const u16, b: *const u16) -> i32 {
    if a.is_null() || b.is_null() {
        invalid(EINVAL);
        return NLS_ERROR;
    }
    wcscmp(a, b)
}

pub extern "win64" fn _wcsnicmp(a: *const u16, b: *const u16, count: usize) -> i32 {
    if count == 0 {
        return 0;
    }
    if a.is_null() || b.is_null() {
        invalid(EINVAL);
        return NLS_ERROR;
    }
    // SAFETY: readable up to their terminators or count.
    unsafe { compare_folded(a, b, count) }
}

pub extern "win64" fn _wcsicoll(a: *const u16, b: *const u16) -> i32 {
    _wcsnicmp(a, b, usize::MAX)
}

pub extern "win64" fn wcsncpy(dst: *mut u16, src: *const u16, count: usize) -> *mut u16 {
    let n = wcsnlen(src, count);
    // SAFETY: the C contract: dst holds count units.
    unsafe {
        std::ptr::copy(src, dst, n);
        std::ptr::write_bytes(dst.add(n), 0, count - n);
    }
    dst
}

pub extern "win64" fn wcscpy_s(dst: *mut u16, size: usize, src: *const u16) -> i32 {
    // SAFETY: validated inside; dst holds size units.
    unsafe { copy_s(dst, size, src) }
}

pub extern "win64" fn wcscat_s(dst: *mut u16, size: usize, src: *const u16) -> i32 {
    // SAFETY: validated inside; dst holds size units.
    unsafe { cat_s(dst, size, src, None) }
}

pub extern "win64" fn wcsncat_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
    // SAFETY: validated inside; dst holds size units.
    unsafe { cat_s(dst, size, src, Some(count)) }
}

pub extern "win64" fn wcsncpy_s(dst: *mut u16, size: usize, src: *const u16, count: usize) -> i32 {
    // SAFETY: validated inside; dst holds size units.
    unsafe { ncopy_s(dst, size, src, count) }
}

pub extern "win64" fn wcstok_s(s: *mut u16, delimiters: *const u16, context: *mut *mut u16) -> *mut u16 {
    // SAFETY: validated inside.
    unsafe { tok(s, delimiters, context) }
}

/// A copy in this UCRT's heap (the title frees it with `free`).
pub extern "win64" fn _wcsdup(s: *const u16) -> *mut u16 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    let n = wcslen(s) + 1;
    let copy = super::ucrt_heap::malloc(n * 2).cast::<u16>();
    if !copy.is_null() {
        // SAFETY: n units each.
        unsafe { std::ptr::copy_nonoverlapping(s, copy, n) };
    }
    copy
}

// ---- character classes ---------------------------------------------------

pub extern "win64" fn isalpha(c: i32) -> i32 {
    i32::from(ctype(c) & ALPHA)
}
pub extern "win64" fn isalnum(c: i32) -> i32 {
    i32::from(ctype(c) & (ALPHA | DIGIT))
}
pub extern "win64" fn isdigit(c: i32) -> i32 {
    i32::from(ctype(c) & DIGIT)
}
pub extern "win64" fn islower(c: i32) -> i32 {
    i32::from(ctype(c) & LOWER)
}
pub extern "win64" fn isupper(c: i32) -> i32 {
    i32::from(ctype(c) & UPPER)
}
pub extern "win64" fn isspace(c: i32) -> i32 {
    i32::from(ctype(c) & SPACE)
}
pub extern "win64" fn isxdigit(c: i32) -> i32 {
    i32::from(ctype(c) & HEX)
}
pub extern "win64" fn tolower(c: i32) -> i32 {
    if (65..=90).contains(&c) { c + 32 } else { c }
}
pub extern "win64" fn toupper(c: i32) -> i32 {
    if (97..=122).contains(&c) { c - 32 } else { c }
}

/// The wide classes are Unicode's, as on Windows; WEOF has none.
fn wide_char(c: u16) -> Option<char> {
    char::from_u32(u32::from(c)).filter(|_| c != 0xFFFF)
}

pub extern "win64" fn iswdigit(c: u16) -> i32 {
    i32::from((0x30..=0x39).contains(&c) || matches!(c, 0xB2 | 0xB3 | 0xB9 | 0xFF10..=0xFF19))
}
pub extern "win64" fn iswxdigit(c: u16) -> i32 {
    i32::from(
        (c < 0x80 && (c as u8).is_ascii_hexdigit())
            || matches!(c, 0xFF10..=0xFF19 | 0xFF21..=0xFF26 | 0xFF41..=0xFF46),
    )
}
pub extern "win64" fn iswspace(c: u16) -> i32 {
    i32::from(wide_char(c).is_some_and(char::is_whitespace))
}
pub extern "win64" fn iswalnum(c: u16) -> i32 {
    i32::from(iswdigit(c) != 0 || wide_char(c).is_some_and(char::is_alphabetic))
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-string",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-string-*"],
        exports: vec![
            hle_export!(strlen),
            hle_export!(__strncnt),
            hle_export!(strcmp),
            hle_export!(strncmp),
            hle_export!(_stricmp),
            hle_export!(_strnicmp),
            hle_export!(strcpy),
            hle_export!(strcat),
            hle_export!(strncpy),
            hle_export!(strncat),
            hle_export!(strcpy_s),
            hle_export!(strcat_s),
            hle_export!(strncpy_s),
            hle_export!(strspn),
            hle_export!(strcspn),
            hle_export!(strpbrk),
            hle_export!(strtok_s),
            hle_export!(strtok),
            hle_export!(wcslen),
            hle_export!(wcsnlen),
            hle_export!(wcscmp),
            hle_export!(wcsncmp),
            hle_export!(wcscoll),
            hle_export!(_wcsnicmp),
            hle_export!(_wcsicoll),
            hle_export!(wcsncpy),
            hle_export!(wcscpy_s),
            hle_export!(wcscat_s),
            hle_export!(wcsncat_s),
            hle_export!(wcsncpy_s),
            hle_export!(wcstok_s),
            hle_export!(_wcsdup),
            hle_export!(isalpha),
            hle_export!(isalnum),
            hle_export!(isdigit),
            hle_export!(islower),
            hle_export!(isupper),
            hle_export!(isspace),
            hle_export!(isxdigit),
            hle_export!(tolower),
            hle_export!(toupper),
            hle_export!(iswdigit),
            hle_export!(iswxdigit),
            hle_export!(iswspace),
            hle_export!(iswalnum),
        ],
    }
}

#[cfg(test)]
pub(crate) fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_runtime::errno;

    fn c(s: &str) -> Vec<c_char> {
        s.bytes().map(|b| b as c_char).chain([0]).collect()
    }
    fn text(s: &[c_char]) -> String {
        s.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect()
    }

    #[test]
    fn comparisons_answer_like_the_ucrt() {
        assert_eq!(strcmp(c("abc").as_ptr(), c("abd").as_ptr()), -1);
        assert_eq!(strcmp(c("b").as_ptr(), c("a").as_ptr()), 1);
        assert_eq!(strncmp(c("abc").as_ptr(), c("abe").as_ptr(), 3), -2);
        assert_eq!(strncmp(c("abc").as_ptr(), c("abe").as_ptr(), 2), 0);
        assert_eq!(_stricmp(c("HeLLo").as_ptr(), c("hello").as_ptr()), 0);
        assert_eq!(_stricmp(c("a").as_ptr(), c("C").as_ptr()), -2);
        assert_eq!(_strnicmp(c("ABx").as_ptr(), c("aby").as_ptr(), 2), 0);
        assert_eq!(_stricmp(std::ptr::null(), c("a").as_ptr()), NLS_ERROR);
        assert_eq!(wcscmp(w("a").as_ptr(), w("b").as_ptr()), -1);
        assert_eq!(wcsncmp(w("ad").as_ptr(), w("aa").as_ptr(), 2), 3);
        assert_eq!(_wcsnicmp(w("ABC").as_ptr(), w("abd").as_ptr(), 3), -1);
        assert_eq!(_wcsicoll(w("Zed").as_ptr(), w("zED").as_ptr()), 0);
    }

    #[test]
    fn secure_copies_follow_the_ucrts_rules() {
        let mut buf = [0x55 as c_char; 4];
        assert_eq!(strcpy_s(buf.as_mut_ptr(), 4, c("abc").as_ptr()), 0);
        assert_eq!(text(&buf), "abc");
        assert_eq!(strcpy_s(buf.as_mut_ptr(), 4, c("abcd").as_ptr()), ERANGE);
        assert_eq!(buf[0], 0);
        assert_eq!(strcpy_s(std::ptr::null_mut(), 4, c("a").as_ptr()), EINVAL);

        let mut buf = c("ab");
        buf.resize(5, 0x55);
        assert_eq!(strcat_s(buf.as_mut_ptr(), 5, c("cd").as_ptr()), 0);
        assert_eq!(text(&buf), "abcd");
        assert_eq!(strcat_s(buf.as_mut_ptr(), 5, c("e").as_ptr()), ERANGE);
        assert_eq!(buf[0], 0);
        let mut unterminated = [0x41 as c_char; 3];
        assert_eq!(strcat_s(unterminated.as_mut_ptr(), 3, c("e").as_ptr()), EINVAL);

        let mut buf = [0x55 as c_char; 4];
        assert_eq!(strncpy_s(buf.as_mut_ptr(), 4, c("abcdef").as_ptr(), 2), 0);
        assert_eq!(text(&buf), "ab");
        assert_eq!(strncpy_s(buf.as_mut_ptr(), 4, c("abcdef").as_ptr(), usize::MAX), STRUNCATE);
        assert_eq!(text(&buf), "abc");
        assert_eq!(strncpy_s(buf.as_mut_ptr(), 4, c("abcdef").as_ptr(), 4), ERANGE);
        assert_eq!(buf[0], 0);
        assert_eq!(strncpy_s(buf.as_mut_ptr(), 4, c("abc").as_ptr(), 3), 0);
        assert_eq!(text(&buf), "abc");
        assert_eq!(strncpy_s(std::ptr::null_mut(), 0, std::ptr::null(), 0), 0);

        let mut wbuf = [0x55u16; 6];
        wbuf[0] = 0;
        assert_eq!(wcsncat_s(wbuf.as_mut_ptr(), 6, w("hello world").as_ptr(), 3), 0);
        assert_eq!(&wbuf[..4], &w("hel")[..]);
        assert_eq!(wcsncat_s(wbuf.as_mut_ptr(), 6, w("lo!!").as_ptr(), usize::MAX), STRUNCATE);
        assert_eq!(String::from_utf16_lossy(&wbuf[..5]), "hello");
        assert_eq!(wcsncat_s(wbuf.as_mut_ptr(), 6, w("x").as_ptr(), 1), ERANGE);
        assert_eq!(errno(), ERANGE);
        assert_eq!(wcscpy_s(wbuf.as_mut_ptr(), 6, w("12345").as_ptr()), 0);
        assert_eq!(wcsncpy_s(wbuf.as_mut_ptr(), 6, w("abcdefgh").as_ptr(), 0), 0);
        assert_eq!(wbuf[0], 0);
    }

    #[test]
    fn tokens_spans_and_classes() {
        let mut s = c(",,a,b;;c");
        let delims = c(",;");
        let mut context = std::ptr::null_mut();
        let mut tokens = vec![];
        let mut p = strtok_s(s.as_mut_ptr(), delims.as_ptr(), &mut context);
        while !p.is_null() {
            // SAFETY: a token in s.
            tokens.push(unsafe { std::ffi::CStr::from_ptr(p) }.to_str().unwrap().to_owned());
            p = strtok_s(std::ptr::null_mut(), delims.as_ptr(), &mut context);
        }
        assert_eq!(tokens, ["a", "b", "c"]);
        assert_eq!(strspn(c("aab!").as_ptr(), c("ab").as_ptr()), 3);
        assert_eq!(strcspn(c("xyz!").as_ptr(), c("!").as_ptr()), 3);
        assert!(strpbrk(c("xyz").as_ptr(), c("!").as_ptr()).is_null());
        let mut pad = [0x55 as c_char; 5];
        strncpy(pad.as_mut_ptr(), c("ab").as_ptr(), 5);
        assert_eq!(pad, [97, 98, 0, 0, 0]);
        assert!(isalpha('a' as i32) != 0 && isalpha(0xE9) == 0 && isalpha(-1) == 0 && isalpha(1000) == 0);
        assert!(isspace('\t' as i32) != 0 && isxdigit('F' as i32) != 0 && isxdigit('g' as i32) == 0);
        assert_eq!((tolower('A' as i32), toupper('z' as i32), tolower(0xC9)), (97, 90, 0xC9));
        assert!(iswalnum(0x00E9) != 0 && iswalnum(0x4E2D) != 0 && iswalnum('-' as u16) == 0);
        assert!(iswspace(0x3000) != 0 && iswspace('a' as u16) == 0 && iswdigit(0xFF15) != 0);
        let dup = _wcsdup(w("dup").as_ptr());
        // SAFETY: a NUL-terminated copy.
        assert_eq!(unsafe { units(dup) }, &w("dup")[..3]);
        crate::linux::ucrt_heap::free(dup.cast());
    }
}
