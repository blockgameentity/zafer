//! kernel32 text: code page conversion, comparison, case mapping, sort
//! keys, character classes, the `lstr*` family and system message texts.
//!
//! Code pages: 65001 (UTF-8), 1252 and 437. `CP_ACP` and `CP_THREAD_ACP`
//! are 1252 and `CP_OEMCP` 437, whatever the host's locale, as on an
//! English console; any other is `ERROR_INVALID_PARAMETER`.
//! - UTF-8 is exact for well-formed text; ill-formed input becomes one
//!   U+FFFD per maximal ill-formed subpart (as Windows does since Vista),
//!   or `ERROR_NO_UNICODE_TRANSLATION` with `MB_ERR_INVALID_CHARS` /
//!   `WC_ERR_INVALID_CHARS`. A lone surrogate is U+FFFD.
//! - 1252 and 437 are Windows' tables. The way back has no best-fit
//!   mapping: a character the code page lacks becomes the default
//!   character and sets `*used`.
//!
//! Comparison and sort keys are one algorithm for every locale, after
//! what Windows does for en-US: level 1 symbols < digits < letters,
//! letters without case or accent (Latin-1 expansions: `æ` is `ae`, `ß`
//! `ss`); level 2 accents; level 3 case, lower first; level 4 the hyphen
//! and apostrophe, which a word sort (the default) leaves out of level 1.
// ponytail: accents and expansions are known for Latin-1 only, other
// letters sort by their lower-case code; a real collation table (UCA) if
// a title sorts text in other scripts.
//!
//! Case mapping and character classes are Rust's Unicode tables, one
//! UTF-16 unit at a time (a mapping that would change the length keeps the
//! unit, as Windows does).

#![allow(non_snake_case)] // named like the exports

use std::cmp::Ordering;
use std::ffi::c_void;

use victory_loader::linux::teb::set_last_error;

use super::kernel32_file::{ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, fail, wide};
use super::kernel32_heap;
use crate::HleModule;
use crate::hle_export;

pub(crate) const ERROR_INVALID_FLAGS: u32 = 1004;
const ERROR_NO_UNICODE_TRANSLATION: u32 = 1113;
const ERROR_MR_MID_NOT_FOUND: u32 = 317;

const CP_ACP: u32 = 0;
const CP_OEMCP: u32 = 1;
const CP_THREAD_ACP: u32 = 3;
const CP_UTF8: u32 = 65001;
const MB_ERR_INVALID_CHARS: u32 = 0x8;
const WC_ERR_INVALID_CHARS: u32 = 0x80;

/// Bytes 0x80..0xFF of code page 1252; the five it leaves unassigned map
/// to the C1 control of the same value, as Windows maps them.
const CP1252: [u16; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039,
    0x0152, 0x008D, 0x017D, 0x008F, 0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014,
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178,
];

/// Bytes 0x80..0xFF of code page 437.
const CP437: [u16; 128] = [
    0x00C7, 0x00FC, 0x00E9, 0x00E2, 0x00E4, 0x00E0, 0x00E5, 0x00E7, 0x00EA, 0x00EB, 0x00E8, 0x00EF,
    0x00EE, 0x00EC, 0x00C4, 0x00C5, 0x00C9, 0x00E6, 0x00C6, 0x00F4, 0x00F6, 0x00F2, 0x00FB, 0x00F9,
    0x00FF, 0x00D6, 0x00DC, 0x00A2, 0x00A3, 0x00A5, 0x20A7, 0x0192, 0x00E1, 0x00ED, 0x00F3, 0x00FA,
    0x00F1, 0x00D1, 0x00AA, 0x00BA, 0x00BF, 0x2310, 0x00AC, 0x00BD, 0x00BC, 0x00A1, 0x00AB, 0x00BB,
    0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, 0x2555, 0x2563, 0x2551, 0x2557,
    0x255D, 0x255C, 0x255B, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x255E, 0x255F,
    0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, 0x2568, 0x2564, 0x2565, 0x2559,
    0x2558, 0x2552, 0x2553, 0x256B, 0x256A, 0x2518, 0x250C, 0x2588, 0x2584, 0x258C, 0x2590, 0x2580,
    0x03B1, 0x00DF, 0x0393, 0x03C0, 0x03A3, 0x03C3, 0x00B5, 0x03C4, 0x03A6, 0x0398, 0x03A9, 0x03B4,
    0x221E, 0x03C6, 0x03B5, 0x2229, 0x2261, 0x00B1, 0x2265, 0x2264, 0x2320, 0x2321, 0x00F7, 0x2248,
    0x00B0, 0x2219, 0x00B7, 0x221A, 0x207F, 0x00B2, 0x25A0, 0x00A0,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodePage {
    Utf8,
    Cp1252,
    Cp437,
}

fn code_page(cp: u32) -> Option<CodePage> {
    match cp {
        CP_ACP | CP_THREAD_ACP | 1252 => Some(CodePage::Cp1252),
        CP_OEMCP | 437 => Some(CodePage::Cp437),
        CP_UTF8 => Some(CodePage::Utf8),
        _ => None,
    }
}

fn high_half(cp: CodePage) -> impl Fn(u8) -> u16 {
    move |b| match (cp, b) {
        (_, 0..=0x7F) => u16::from(b),
        (CodePage::Cp1252, 0x80..=0x9F) => CP1252[usize::from(b - 0x80)],
        (CodePage::Cp437, _) => CP437[usize::from(b - 0x80)],
        _ => u16::from(b),
    }
}

/// The single-byte code page's byte for `unit`.
fn to_byte(cp: CodePage, unit: u16) -> Option<u8> {
    if unit < 0x80 {
        return Some(unit as u8);
    }
    let table: &[u16] = if cp == CodePage::Cp437 {
        &CP437
    } else {
        &CP1252
    };
    if let Some(i) = table.iter().position(|&u| u == unit) {
        return Some(0x80 + i as u8);
    }
    (cp == CodePage::Cp1252 && (0xA0..=0xFF).contains(&unit)).then_some(unit as u8)
}

/// `bytes` as UTF-16 in code page `cp`; `None` when `strict` and ill-formed.
fn decode(cp: CodePage, bytes: &[u8], strict: bool) -> Option<Vec<u16>> {
    match cp {
        CodePage::Utf8 if strict => Some(std::str::from_utf8(bytes).ok()?.encode_utf16().collect()),
        // Rust's lossy decoding replaces each maximal ill-formed subpart.
        CodePage::Utf8 => Some(String::from_utf8_lossy(bytes).encode_utf16().collect()),
        _ => Some(bytes.iter().map(|&b| high_half(cp)(b)).collect()),
    }
}

/// The source of a conversion: `len` -1 is up to and with the NUL.
fn source<'a, T: Copy + Default + PartialEq>(p: *const T, len: i32) -> Option<&'a [T]> {
    if p.is_null() || len == 0 || len < -1 {
        return None;
    }
    let n = if len == -1 {
        let mut n = 0;
        // SAFETY: the caller passes a NUL-terminated string.
        while unsafe { p.add(n).read() } != T::default() {
            n += 1;
        }
        n + 1
    } else {
        len as usize
    };
    // SAFETY: the caller's `n` units.
    Some(unsafe { std::slice::from_raw_parts(p, n) })
}

/// The output rule of both conversions: capacity 0 asks for the size; a
/// result that does not fit is `ERROR_INSUFFICIENT_BUFFER`.
fn output<T: Copy>(result: &[T], out: *mut T, capacity: i32) -> i32 {
    if capacity == 0 {
        return result.len() as i32;
    }
    if out.is_null() || capacity < 0 {
        return fail(ERROR_INVALID_PARAMETER);
    }
    if result.len() > capacity as usize {
        return fail(ERROR_INSUFFICIENT_BUFFER);
    }
    // SAFETY: the caller's buffer of `capacity` units, enough.
    unsafe { std::ptr::copy_nonoverlapping(result.as_ptr(), out, result.len()) };
    result.len() as i32
}

pub extern "win64" fn MultiByteToWideChar(
    cp: u32,
    flags: u32,
    text: *const u8,
    len: i32,
    out: *mut u16,
    capacity: i32,
) -> i32 {
    let Some(page) = code_page(cp) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    if page == CodePage::Utf8 && flags & !MB_ERR_INVALID_CHARS != 0 {
        return fail(ERROR_INVALID_FLAGS);
    }
    let Some(text) = source(text, len) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    match decode(page, text, flags & MB_ERR_INVALID_CHARS != 0) {
        Some(wide) => output(&wide, out, capacity),
        None => fail(ERROR_NO_UNICODE_TRANSLATION),
    }
}

pub extern "win64" fn WideCharToMultiByte(
    cp: u32,
    flags: u32,
    text: *const u16,
    len: i32,
    out: *mut u8,
    capacity: i32,
    default: *const u8,
    used: *mut i32,
) -> i32 {
    let Some(page) = code_page(cp) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let Some(text) = source(text, len) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let bytes = if page == CodePage::Utf8 {
        if !default.is_null() || !used.is_null() {
            return fail(ERROR_INVALID_PARAMETER);
        }
        let strict = flags & WC_ERR_INVALID_CHARS != 0;
        let mut s = String::with_capacity(text.len());
        for c in char::decode_utf16(text.iter().copied()) {
            match c {
                Ok(c) => s.push(c),
                Err(_) if strict => return fail(ERROR_NO_UNICODE_TRANSLATION),
                Err(_) => s.push('\u{FFFD}'),
            }
        }
        s.into_bytes()
    } else {
        // SAFETY: null or the caller's default character.
        let fallback = unsafe { default.as_ref() }.copied().unwrap_or(b'?');
        let mut missed = false;
        let bytes = text
            .iter()
            .map(|&u| {
                to_byte(page, u).unwrap_or_else(|| {
                    missed = true;
                    fallback
                })
            })
            .collect::<Vec<_>>();
        if !used.is_null() {
            // SAFETY: the caller's out pointer.
            unsafe { used.write(i32::from(missed)) };
        }
        bytes
    };
    output(&bytes, out, capacity)
}

pub extern "win64" fn GetACP() -> u32 {
    1252
}

pub extern "win64" fn GetOEMCP() -> u32 {
    437
}

pub extern "win64" fn IsValidCodePage(cp: u32) -> i32 {
    i32::from(cp > CP_THREAD_ACP && code_page(cp).is_some())
}

/// `CPINFO`.
#[repr(C)]
#[derive(Debug, Default)]
pub struct CpInfo {
    pub max_char_size: u32,
    pub default_char: [u8; 2],
    pub lead_byte: [u8; 12],
}

pub extern "win64" fn GetCPInfo(cp: u32, out: *mut CpInfo) -> i32 {
    let Some(page) = code_page(cp) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    if out.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let info = CpInfo {
        max_char_size: if page == CodePage::Utf8 { 4 } else { 1 },
        default_char: [b'?', 0],
        lead_byte: [0; 12],
    };
    // SAFETY: the caller's CPINFO.
    unsafe { out.write_unaligned(info) };
    1
}

/// No code page here has lead bytes.
pub extern "win64" fn IsDBCSLeadByte(_byte: u8) -> i32 {
    0
}

pub extern "win64" fn IsDBCSLeadByteEx(_cp: u32, _byte: u8) -> i32 {
    0
}

// ---- comparison and sort keys ---------------------------------------------------------------

const NORM_IGNORECASE: u32 = 0x1;
const NORM_IGNORENONSPACE: u32 = 0x2;
const NORM_IGNORESYMBOLS: u32 = 0x4;
const SORT_DIGITSASNUMBERS: u32 = 0x8;
const LINGUISTIC_IGNORECASE: u32 = 0x10;
const LINGUISTIC_IGNOREDIACRITIC: u32 = 0x20;
const SORT_STRINGSORT: u32 = 0x1000;
const NORM_IGNOREKANATYPE: u32 = 0x1_0000;
const NORM_IGNOREWIDTH: u32 = 0x2_0000;
const NORM_LINGUISTIC_CASING: u32 = 0x800_0000;
const COMPARE_FLAGS: u32 = NORM_IGNORECASE
    | NORM_IGNORENONSPACE
    | NORM_IGNORESYMBOLS
    | SORT_DIGITSASNUMBERS
    | LINGUISTIC_IGNORECASE
    | LINGUISTIC_IGNOREDIACRITIC
    | SORT_STRINGSORT
    | NORM_IGNOREKANATYPE
    | NORM_IGNOREWIDTH
    | NORM_LINGUISTIC_CASING;

/// A Latin-1 letter's base letters and accent number (0: none).
fn latin1(c: char) -> Option<(&'static str, u8)> {
    let lower = c.to_lowercase().next()?;
    Some(match lower {
        'à' => ("a", 1),
        'á' => ("a", 2),
        'â' => ("a", 3),
        'ã' => ("a", 4),
        'ä' => ("a", 5),
        'å' => ("a", 6),
        'æ' => ("ae", 0),
        'ç' => ("c", 7),
        'è' => ("e", 1),
        'é' => ("e", 2),
        'ê' => ("e", 3),
        'ë' => ("e", 5),
        'ì' => ("i", 1),
        'í' => ("i", 2),
        'î' => ("i", 3),
        'ï' => ("i", 5),
        'ð' => ("d", 8),
        'ñ' => ("n", 4),
        'ò' => ("o", 1),
        'ó' => ("o", 2),
        'ô' => ("o", 3),
        'õ' => ("o", 4),
        'ö' => ("o", 5),
        'ø' => ("o", 9),
        'ù' => ("u", 1),
        'ú' => ("u", 2),
        'û' => ("u", 3),
        'ü' => ("u", 5),
        'ý' => ("y", 2),
        'ÿ' => ("y", 5),
        'þ' => ("th", 0),
        'ß' => ("ss", 0),
        _ => return None,
    })
}

/// Three key bytes for a level-1 weight, none of them 0 or 1, in order.
fn weight(class: u8, code: u32, key: &mut Vec<u8>) {
    key.extend([
        class + 2,
        ((code >> 14) & 0x7F) as u8 + 2,
        ((code >> 7) & 0x7F) as u8 + 2,
        (code & 0x7F) as u8 + 2,
    ]);
}

/// The sort key of `text`: levels 1 to 4, each after a 0x01, then a 0. No
/// other byte is 0 or 1, so byte order of keys is the order of texts.
pub(crate) fn sort_key(text: &[u16], flags: u32) -> Vec<u8> {
    let ignore_case = flags & (NORM_IGNORECASE | LINGUISTIC_IGNORECASE) != 0;
    let ignore_accents = flags & (NORM_IGNORENONSPACE | LINGUISTIC_IGNOREDIACRITIC) != 0;
    let (mut l1, mut l2, mut l3, mut l4) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (i, c) in char::decode_utf16(text.iter().copied()).enumerate() {
        let c = c.unwrap_or('\u{FFFD}');
        if c == '\0' {
            continue;
        }
        if flags & SORT_STRINGSORT == 0 && (c == '-' || c == '\'') {
            weight(0, i as u32, &mut l4);
            weight(0, c as u32, &mut l4);
            continue;
        }
        let letter = c.is_alphabetic();
        let class = if letter {
            2
        } else if c.is_numeric() {
            1
        } else {
            0
        };
        if class == 0 && flags & NORM_IGNORESYMBOLS != 0 {
            continue;
        }
        let lower = c.to_lowercase().next().unwrap_or(c);
        let (base, accent) = latin1(c).unwrap_or(("", 0));
        let case = u8::from(letter && c.is_uppercase()) + 2;
        // An expansion weighs as its letters, each with the accent and case.
        for b in base.chars().chain(base.is_empty().then_some(lower)) {
            weight(class, b as u32, &mut l1);
            l2.push(accent + 2);
            l3.push(case);
        }
    }
    let mut key = l1;
    key.push(1);
    if !ignore_accents {
        key.extend(l2);
    }
    key.push(1);
    if !ignore_case {
        key.extend(l3);
    }
    key.push(1);
    key.extend(l4);
    key.push(0);
    key
}

/// `CSTR_LESS_THAN` 1, `CSTR_EQUAL` 2, `CSTR_GREATER_THAN` 3.
fn cstr(order: Ordering) -> i32 {
    order as i32 + 2
}

/// A string argument: `len` negative is up to the terminator.
fn text<'a>(p: *const u16, len: i32) -> Option<&'a [u16]> {
    if p.is_null() {
        return None;
    }
    if len < 0 {
        return wide(p);
    }
    // SAFETY: the caller's `len` units.
    Some(unsafe { std::slice::from_raw_parts(p, len as usize) })
}

fn compare(flags: u32, a: *const u16, a_len: i32, b: *const u16, b_len: i32) -> i32 {
    if flags & !COMPARE_FLAGS != 0 {
        return fail(ERROR_INVALID_FLAGS);
    }
    let (Some(a), Some(b)) = (text(a, a_len), text(b, b_len)) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    cstr(sort_key(a, flags).cmp(&sort_key(b, flags)))
}

pub extern "win64" fn CompareStringEx(
    _locale: *const u16,
    flags: u32,
    a: *const u16,
    a_len: i32,
    b: *const u16,
    b_len: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    compare(flags, a, a_len, b, b_len)
}

pub extern "win64" fn CompareStringW(
    _lcid: u32,
    flags: u32,
    a: *const u16,
    a_len: i32,
    b: *const u16,
    b_len: i32,
) -> i32 {
    compare(flags, a, a_len, b, b_len)
}

/// Upper case of one UTF-16 unit, kept when that would change its length.
fn upper(u: u16) -> u16 {
    map_unit(u, |c| c.to_uppercase())
}

fn lower(u: u16) -> u16 {
    map_unit(u, |c| c.to_lowercase())
}

fn map_unit<I: Iterator<Item = char>>(u: u16, f: impl Fn(char) -> I) -> u16 {
    let Some(c) = char::from_u32(u.into()) else {
        return u; // a surrogate
    };
    let mut mapped = f(c);
    match (mapped.next(), mapped.next()) {
        (Some(m), None) if (m as u32) < 0x1_0000 => m as u16,
        _ => u,
    }
}

pub extern "win64" fn CompareStringOrdinal(
    a: *const u16,
    a_len: i32,
    b: *const u16,
    b_len: i32,
    ignore_case: i32,
) -> i32 {
    let (Some(a), Some(b)) = (text(a, a_len), text(b, b_len)) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let fold = |u: u16| if ignore_case != 0 { upper(u) } else { u };
    cstr(a.iter().map(|&u| fold(u)).cmp(b.iter().map(|&u| fold(u))))
}

/// The word sort of the user locale; a null string sorts first.
fn lstrcmp(a: *const u16, b: *const u16, flags: u32) -> i32 {
    match (wide(a), wide(b)) {
        (None, None) => 0,
        (None, _) => -1,
        (_, None) => 1,
        (Some(a), Some(b)) => sort_key(a, flags).cmp(&sort_key(b, flags)) as i32,
    }
}

pub extern "win64" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    lstrcmp(a, b, 0)
}

pub extern "win64" fn lstrcmpiW(a: *const u16, b: *const u16) -> i32 {
    lstrcmp(a, b, NORM_IGNORECASE)
}

pub extern "win64" fn lstrlenW(text: *const u16) -> i32 {
    wide(text).map_or(0, |t| t.len() as i32)
}

pub extern "win64" fn lstrlenA(text: *const i8) -> i32 {
    super::kernel32_file::narrow(text).map_or(0, |t| t.len() as i32)
}

// ---- mapping --------------------------------------------------------------------------------

const LCMAP_LOWERCASE: u32 = 0x100;
const LCMAP_UPPERCASE: u32 = 0x200;
const LCMAP_SORTKEY: u32 = 0x400;
const LCMAP_LINGUISTIC_CASING: u32 = 0x100_0000;

/// `LCMAP_LOWERCASE`, `LCMAP_UPPERCASE` (lengths in units) and
/// `LCMAP_SORTKEY` (in bytes) with the comparison flags; the other
/// transforms are `ERROR_INVALID_FLAGS`.
fn map(flags: u32, src: *const u16, len: i32, out: *mut u16, capacity: i32) -> i32 {
    let Some(src) = source(src, len) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    let case = flags & (LCMAP_LOWERCASE | LCMAP_UPPERCASE);
    if flags & LCMAP_SORTKEY != 0 {
        if case != 0 || flags & !(LCMAP_SORTKEY | COMPARE_FLAGS) != 0 {
            return fail(ERROR_INVALID_FLAGS);
        }
        let text = src.strip_suffix(&[0]).unwrap_or(src);
        return output(&sort_key(text, flags), out.cast::<u8>(), capacity);
    }
    let others = flags & !(case | LCMAP_LINGUISTIC_CASING | NORM_IGNORECASE);
    if case == 0 || case == LCMAP_LOWERCASE | LCMAP_UPPERCASE || others != 0 {
        return fail(ERROR_INVALID_FLAGS);
    }
    let f = if case == LCMAP_UPPERCASE {
        upper
    } else {
        lower
    };
    let mapped: Vec<u16> = src.iter().map(|&u| f(u)).collect();
    output(&mapped, out, capacity)
}

pub extern "win64" fn LCMapStringEx(
    _locale: *const u16,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    capacity: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    map(flags, src, len, out, capacity)
}

pub extern "win64" fn LCMapStringW(
    _lcid: u32,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    capacity: i32,
) -> i32 {
    map(flags, src, len, out, capacity)
}

// ---- character classes ----------------------------------------------------------------------

const CT_CTYPE1: u32 = 0x1;
const C1_UPPER: u16 = 0x1;
const C1_LOWER: u16 = 0x2;
const C1_DIGIT: u16 = 0x4;
const C1_SPACE: u16 = 0x8;
const C1_PUNCT: u16 = 0x10;
const C1_CNTRL: u16 = 0x20;
const C1_BLANK: u16 = 0x40;
const C1_XDIGIT: u16 = 0x80;
const C1_ALPHA: u16 = 0x100;
const C1_DEFINED: u16 = 0x200;

/// The `CT_CTYPE1` class of one unit; 0 for a surrogate.
fn ctype1(u: u16) -> u16 {
    let Some(c) = char::from_u32(u.into()) else {
        return 0;
    };
    let mut t = C1_DEFINED;
    let flag = |on: bool, bit: u16| if on { bit } else { 0 };
    t |= flag(c.is_uppercase(), C1_UPPER);
    t |= flag(c.is_lowercase(), C1_LOWER);
    t |= flag(c.is_ascii_digit(), C1_DIGIT);
    t |= flag(c.is_whitespace(), C1_SPACE);
    t |= flag(c.is_control(), C1_CNTRL);
    t |= flag(c == ' ' || c == '\t' || c == '\u{A0}', C1_BLANK);
    t |= flag(c.is_ascii_hexdigit(), C1_XDIGIT);
    t |= flag(c.is_alphabetic(), C1_ALPHA);
    t |= flag(
        !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control(),
        C1_PUNCT,
    );
    t
}

/// `CT_CTYPE1` only (what msvcp140 asks for); the other types are
/// `ERROR_INVALID_FLAGS`.
fn string_type(kind: u32, src: *const u16, len: i32, out: *mut u16) -> i32 {
    if kind != CT_CTYPE1 {
        return fail(ERROR_INVALID_FLAGS);
    }
    let (Some(src), false) = (text(src, len), out.is_null()) else {
        return fail(ERROR_INVALID_PARAMETER);
    };
    for (i, &u) in src.iter().enumerate() {
        // SAFETY: the caller's array of as many units as the source.
        unsafe { out.add(i).write(ctype1(u)) };
    }
    1
}

pub extern "win64" fn GetStringTypeW(kind: u32, src: *const u16, len: i32, out: *mut u16) -> i32 {
    string_type(kind, src, len, out)
}

pub extern "win64" fn GetStringTypeExW(
    _lcid: u32,
    kind: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
) -> i32 {
    string_type(kind, src, len, out)
}

// ---- system messages ------------------------------------------------------------------------

const FORMAT_MESSAGE_ALLOCATE_BUFFER: u32 = 0x100;
const FORMAT_MESSAGE_IGNORE_INSERTS: u32 = 0x200;
const FORMAT_MESSAGE_FROM_STRING: u32 = 0x400;
const FORMAT_MESSAGE_FROM_SYSTEM: u32 = 0x1000;
const FORMAT_MESSAGE_MAX_WIDTH_MASK: u32 = 0xFF;

/// Windows' en-US texts of the codes the runtime reports.
fn system_message(id: u32) -> Option<&'static str> {
    Some(match id {
        0 => "The operation completed successfully.",
        1 => "Incorrect function.",
        2 => "The system cannot find the file specified.",
        3 => "The system cannot find the path specified.",
        4 => "The system cannot open the file.",
        5 => "Access is denied.",
        6 => "The handle is invalid.",
        8 => "Not enough memory resources are available to process this command.",
        18 => "There are no more files.",
        32 => "The process cannot access the file because it is being used by another process.",
        38 => "Reached the end of the file.",
        50 => "The request is not supported.",
        80 => "The file exists.",
        87 => "The parameter is incorrect.",
        112 => "There is not enough space on the disk.",
        122 => "The data area passed to a system call is too small.",
        123 => "The filename, directory name, or volume label syntax is incorrect.",
        145 => "The directory is not empty.",
        183 => "Cannot create a file when that file already exists.",
        258 => "The wait operation timed out.",
        487 => "Attempt to access invalid address.",
        995 => {
            "The I/O operation has been aborted because of either a thread exit or an application request."
        }
        1004 => "Invalid flags.",
        1113 => "No mapping for the Unicode character exists in the target multi-byte code page.",
        _ => return None,
    })
}

/// The message as UTF-16; inserts stay as written: only callers passing
/// `FORMAT_MESSAGE_IGNORE_INSERTS` (or texts without inserts) are served.
fn message(flags: u32, source: Option<Vec<u16>>, id: u32) -> Result<Vec<u16>, u32> {
    let mut text: Vec<u16> = if flags & FORMAT_MESSAGE_FROM_STRING != 0 {
        source.ok_or(ERROR_INVALID_PARAMETER)?
    } else if flags & FORMAT_MESSAGE_FROM_SYSTEM != 0 {
        let code = if id >> 16 == 0x8007 { id & 0xFFFF } else { id };
        let base = system_message(code).ok_or(ERROR_MR_MID_NOT_FOUND)?;
        format!("{base}\r\n").encode_utf16().collect()
    } else {
        return Err(ERROR_MR_MID_NOT_FOUND);
    };
    if flags & FORMAT_MESSAGE_IGNORE_INSERTS == 0 && text.contains(&u16::from(b'%')) {
        return Err(ERROR_INVALID_PARAMETER);
    }
    if flags & FORMAT_MESSAGE_MAX_WIDTH_MASK == FORMAT_MESSAGE_MAX_WIDTH_MASK {
        // Line breaks of the text are ignored: they become spaces.
        let s = String::from_utf16_lossy(&text).replace("\r\n", " ");
        text = s.encode_utf16().collect();
    }
    Ok(text)
}

/// Stores `text` as `FormatMessage` does: into the caller's buffer of
/// `size` units, or, with `ALLOCATE_BUFFER`, into a new `LocalFree` block
/// of at least `size` units whose address goes to `*buffer`.
fn store<T: Copy + Default>(flags: u32, text: &[T], buffer: *mut T, size: u32) -> u32 {
    if buffer.is_null() {
        return fail(ERROR_INVALID_PARAMETER);
    }
    let target = if flags & FORMAT_MESSAGE_ALLOCATE_BUFFER != 0 {
        let units = (text.len() + 1).max(size as usize);
        let block = kernel32_heap::alloc(units * size_of::<T>(), 0, true).cast::<T>();
        if block.is_null() {
            return fail(8);
        }
        // SAFETY: with ALLOCATE_BUFFER the buffer argument is a T**.
        unsafe { buffer.cast::<*mut T>().write(block) };
        block
    } else {
        if (size as usize) < text.len() + 1 {
            return fail(ERROR_INSUFFICIENT_BUFFER);
        }
        buffer
    };
    // SAFETY: room for the text and a NUL, checked or allocated above.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), target, text.len());
        target.add(text.len()).write(T::default());
    }
    text.len() as u32
}

pub extern "win64" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    id: u32,
    _language: u32,
    buffer: *mut u16,
    size: u32,
    _arguments: *mut c_void,
) -> u32 {
    let source = wide(source.cast()).map(<[u16]>::to_vec);
    match message(flags, source, id) {
        Ok(text) => store(flags, &text, buffer, size),
        Err(e) => fail(e),
    }
}

pub extern "win64" fn FormatMessageA(
    flags: u32,
    source: *const c_void,
    id: u32,
    _language: u32,
    buffer: *mut u8,
    size: u32,
    _arguments: *mut c_void,
) -> u32 {
    let source = super::kernel32_file::narrow(source.cast())
        .map(|b| decode(CodePage::Cp1252, b, false).unwrap_or_default());
    match message(flags, source, id) {
        Ok(text) => {
            let bytes: Vec<u8> = text
                .iter()
                .map(|&u| to_byte(CodePage::Cp1252, u).unwrap_or(b'?'))
                .collect();
            store(flags, &bytes, buffer, size)
        }
        Err(e) => {
            set_last_error(e);
            0
        }
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-text",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-string-*",
            "api-ms-win-core-localization-*",
        ],
        exports: vec![
            hle_export!(MultiByteToWideChar),
            hle_export!(WideCharToMultiByte),
            hle_export!(GetACP),
            hle_export!(GetOEMCP),
            hle_export!(IsValidCodePage),
            hle_export!(GetCPInfo),
            hle_export!(IsDBCSLeadByte),
            hle_export!(IsDBCSLeadByteEx),
            hle_export!(CompareStringEx),
            hle_export!(CompareStringW),
            hle_export!(CompareStringOrdinal),
            hle_export!(lstrcmpW),
            hle_export!(lstrcmpiW),
            hle_export!(lstrlenW),
            hle_export!(lstrlenA),
            hle_export!(LCMapStringEx),
            hle_export!(LCMapStringW),
            hle_export!(GetStringTypeW),
            hle_export!(GetStringTypeExW),
            hle_export!(FormatMessageW),
            hle_export!(FormatMessageA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::kernel32_file::tests::w;
    use victory_loader::linux::teb::last_error;

    fn to_wide(cp: u32, flags: u32, bytes: &[u8]) -> Result<Vec<u16>, u32> {
        let n = MultiByteToWideChar(
            cp,
            flags,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        );
        if n == 0 {
            return Err(last_error());
        }
        let mut out = vec![0u16; n as usize];
        assert_eq!(
            MultiByteToWideChar(
                cp,
                flags,
                bytes.as_ptr(),
                bytes.len() as i32,
                out.as_mut_ptr(),
                n
            ),
            n
        );
        Ok(out)
    }

    fn to_bytes(cp: u32, flags: u32, text: &[u16]) -> Result<Vec<u8>, u32> {
        let null = std::ptr::null_mut();
        let n = WideCharToMultiByte(
            cp,
            flags,
            text.as_ptr(),
            text.len() as i32,
            null,
            0,
            std::ptr::null(),
            null.cast(),
        );
        if n == 0 {
            return Err(last_error());
        }
        let mut out = vec![0u8; n as usize];
        WideCharToMultiByte(
            cp,
            flags,
            text.as_ptr(),
            text.len() as i32,
            out.as_mut_ptr(),
            n,
            std::ptr::null(),
            null.cast(),
        );
        Ok(out)
    }

    #[test]
    fn utf8_round_trips_and_ill_formed_input() {
        let s = "héllo € 𝄞";
        let wide = to_wide(CP_UTF8, 0, s.as_bytes()).unwrap();
        assert_eq!(wide, s.encode_utf16().collect::<Vec<_>>());
        assert_eq!(to_bytes(CP_UTF8, 0, &wide).unwrap(), s.as_bytes());
        // One U+FFFD per maximal ill-formed subpart.
        let bad = b"a\xF0\x9F\x98b\xC0\xAFc\xED\xA0\x80";
        let got = to_wide(CP_UTF8, 0, bad).unwrap();
        assert_eq!(
            String::from_utf16(&got).unwrap(),
            "a\u{FFFD}b\u{FFFD}\u{FFFD}c\u{FFFD}\u{FFFD}\u{FFFD}"
        );
        assert_eq!(
            to_wide(CP_UTF8, MB_ERR_INVALID_CHARS, bad),
            Err(ERROR_NO_UNICODE_TRANSLATION)
        );
        assert_eq!(
            to_bytes(CP_UTF8, 0, &[0x61, 0xD800]).unwrap(),
            b"a\xEF\xBF\xBD"
        );
        assert_eq!(
            to_bytes(CP_UTF8, WC_ERR_INVALID_CHARS, &[0xDC00]),
            Err(ERROR_NO_UNICODE_TRANSLATION)
        );
        assert_eq!(to_wide(CP_UTF8, 0x1, b"a"), Err(ERROR_INVALID_FLAGS));
        assert_eq!(to_wide(50000, 0, b"a"), Err(ERROR_INVALID_PARAMETER));
    }

    #[test]
    fn single_byte_code_pages() {
        let bytes: Vec<u8> = (0x20..=0xFF).collect();
        let wide = to_wide(CP_ACP, 0, &bytes).unwrap();
        assert_eq!(wide[0x80 - 0x20], 0x20AC);
        assert_eq!(wide[0xE9 - 0x20], 0xE9);
        assert_eq!(to_bytes(CP_ACP, 0, &wide).unwrap(), bytes);
        let oem = to_wide(CP_OEMCP, 0, &bytes).unwrap();
        assert_eq!(oem[0xC4 - 0x20], 0x2500);
        assert_eq!(to_bytes(437, 0, &oem).unwrap(), bytes);

        let mut used = 0;
        let text = w("a\u{0100}b");
        let mut out = [0u8; 8];
        let n = WideCharToMultiByte(
            1252,
            0,
            text.as_ptr(),
            -1,
            out.as_mut_ptr(),
            8,
            c"*".as_ptr().cast(),
            &mut used,
        );
        assert_eq!((n, &out[..4], used), (4, &b"a*b\0"[..], 1));
        // A buffer that is too small.
        assert_eq!(
            WideCharToMultiByte(
                1252,
                0,
                text.as_ptr(),
                -1,
                out.as_mut_ptr(),
                2,
                std::ptr::null(),
                &mut used
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
        let mut info = CpInfo::default();
        assert_eq!(GetCPInfo(CP_UTF8, &mut info), 1);
        assert_eq!((info.max_char_size, info.default_char[0]), (4, b'?'));
        assert_eq!(size_of::<CpInfo>(), 20);
        assert_eq!(
            (
                GetACP(),
                GetOEMCP(),
                IsValidCodePage(65001),
                IsValidCodePage(932)
            ),
            (1252, 437, 1, 0)
        );
    }

    fn cmp(flags: u32, a: &str, b: &str) -> i32 {
        let (a, b) = (w(a), w(b));
        CompareStringEx(
            std::ptr::null(),
            flags,
            a.as_ptr(),
            -1,
            b.as_ptr(),
            -1,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    }

    #[test]
    fn comparison_is_a_word_sort() {
        assert_eq!(cmp(0, "apple", "Banana"), 1);
        assert_eq!(cmp(0, "a", "A"), 1); // lower first
        assert_eq!(cmp(NORM_IGNORECASE, "Straße", "STRASSE"), 2);
        assert_eq!(cmp(0, "resume", "résumé"), 1);
        assert_eq!(cmp(NORM_IGNORENONSPACE, "resume", "résumé"), 2);
        assert_eq!(cmp(0, "coop", "co-op"), 1);
        assert_eq!(cmp(0, "co-op", "cop"), 1);
        assert_eq!(cmp(0, "!", "1"), 1);
        assert_eq!(cmp(0, "9", "a"), 1);
        assert_eq!(cmp(0, "ab", "a"), 3);
        assert_eq!(cmp(NORM_IGNORESYMBOLS, "a.b", "ab"), 2);
        assert_eq!(cmp(0x4000_0000, "a", "b"), 0);
        let (a, b) = (w("abc"), w("ABD"));
        assert_eq!(CompareStringOrdinal(a.as_ptr(), 2, b.as_ptr(), 2, 1), 2);
        assert_eq!(CompareStringOrdinal(a.as_ptr(), -1, b.as_ptr(), -1, 0), 3);
        assert_eq!(lstrcmpiW(a.as_ptr(), w("ABC").as_ptr()), 0);
        assert_eq!(lstrcmpW(a.as_ptr(), w("ABC").as_ptr()), -1);
        assert_eq!(lstrcmpW(std::ptr::null(), a.as_ptr()), -1);
        assert_eq!(lstrlenW(a.as_ptr()), 3);
        assert_eq!(lstrlenW(std::ptr::null()), 0);
    }

    #[test]
    fn mapping_and_sort_keys() {
        let src = w("Größe ǅ");
        let mut out = [0u16; 16];
        let n = LCMapStringW(
            0x409,
            LCMAP_UPPERCASE,
            src.as_ptr(),
            -1,
            out.as_mut_ptr(),
            16,
        );
        assert_eq!(String::from_utf16_lossy(&out[..n as usize]), "GRÖßE Ǆ\0");
        let n = LCMapStringW(
            0x409,
            LCMAP_LOWERCASE,
            src.as_ptr(),
            3,
            out.as_mut_ptr(),
            16,
        );
        assert_eq!(String::from_utf16_lossy(&out[..n as usize]), "grö");
        assert_eq!(
            LCMapStringW(0x409, 0x0080_0000, src.as_ptr(), -1, out.as_mut_ptr(), 16),
            0
        );
        assert_eq!(last_error(), ERROR_INVALID_FLAGS);

        let key = |s: &str| {
            let s = w(s);
            let mut k = [0u8; 256];
            let n = LCMapStringEx(
                std::ptr::null(),
                LCMAP_SORTKEY,
                s.as_ptr(),
                -1,
                k.as_mut_ptr().cast(),
                256,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            assert!(n > 0);
            let k = k[..n as usize].to_vec();
            assert_eq!(k.iter().position(|&b| b == 0), Some(k.len() - 1));
            k
        };
        let words = ["!", "1", "a", "A", "b", "co-op", "coop", "é", "z"];
        let mut sorted = words;
        sorted.sort_by_key(|s| key(s));
        assert_eq!(sorted, ["!", "1", "a", "A", "b", "coop", "co-op", "é", "z"]);
    }

    #[test]
    fn character_classes() {
        let src = w("aZ5 !\t\u{e9}");
        let mut out = [0u16; 8];
        assert_eq!(
            GetStringTypeW(CT_CTYPE1, src.as_ptr(), 7, out.as_mut_ptr()),
            1
        );
        assert_eq!(out[0], C1_LOWER | C1_ALPHA | C1_XDIGIT | C1_DEFINED);
        assert_eq!(out[1], C1_UPPER | C1_ALPHA | C1_DEFINED);
        assert_eq!(out[2], C1_DIGIT | C1_XDIGIT | C1_DEFINED);
        assert_eq!(out[3], C1_SPACE | C1_BLANK | C1_DEFINED);
        assert_eq!(out[4], C1_PUNCT | C1_DEFINED);
        assert_eq!(out[5], C1_SPACE | C1_CNTRL | C1_BLANK | C1_DEFINED);
        assert_eq!(out[6], C1_LOWER | C1_ALPHA | C1_DEFINED);
        assert_eq!(GetStringTypeW(2, src.as_ptr(), 7, out.as_mut_ptr()), 0);
    }

    #[test]
    fn system_messages() {
        let mut buf = [0u16; 128];
        let flags = FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS;
        let n = FormatMessageW(
            flags,
            std::ptr::null(),
            5,
            0,
            buf.as_mut_ptr(),
            128,
            std::ptr::null_mut(),
        );
        assert_eq!(
            String::from_utf16_lossy(&buf[..n as usize]),
            "Access is denied.\r\n"
        );
        let mut block: *mut u8 = std::ptr::null_mut();
        let n = FormatMessageA(
            flags | FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_MAX_WIDTH_MASK,
            std::ptr::null(),
            0x8007_0002,
            0,
            (&raw mut block).cast(),
            0,
            std::ptr::null_mut(),
        );
        // SAFETY: the block FormatMessageA allocated, `n` bytes and a NUL.
        let text = unsafe { std::slice::from_raw_parts(block, n as usize + 1) };
        assert_eq!(text, b"The system cannot find the file specified. \0");
        kernel32_heap::free(block.cast());
        assert_eq!(
            FormatMessageW(
                flags,
                std::ptr::null(),
                0xDEAD,
                0,
                buf.as_mut_ptr(),
                128,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_MR_MID_NOT_FOUND);
        let n = FormatMessageW(
            FORMAT_MESSAGE_FROM_STRING,
            w("plain").as_ptr().cast(),
            0,
            0,
            buf.as_mut_ptr(),
            128,
            std::ptr::null_mut(),
        );
        assert_eq!(n, 5);
        assert_eq!(
            FormatMessageW(
                flags,
                std::ptr::null(),
                5,
                0,
                buf.as_mut_ptr(),
                5,
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(last_error(), ERROR_INSUFFICIENT_BUFFER);
    }
}
