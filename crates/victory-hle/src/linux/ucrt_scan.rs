//! The UCRT's scanf engine behind `__stdio_common_vsscanf` and
//! `__stdio_common_vswscanf` (`sscanf`, `sscanf_s`, `_snscanf`, `swscanf`
//! and relatives; the SDK's inline wrappers build the `va_list` and the
//! options word).
//!
//! Targets come from the guest's `va_list` (8-byte slots); `long` is 32
//! bits; `%s %c %[` store the function's own width and `%S %C` the other,
//! as for printf (the legacy-wide option is bit 2 here). With the secure
//! bit (the `_s` functions) every `%c %s %[` is followed by its buffer
//! size in characters (an `unsigned`); a field that does not fit stores an
//! empty string, sets ENOMEM and ends the scan. An integer is clamped at
//! 64 bits, then cut to its target. Floating point goes through glibc's
//! `strtod` in the C locale. The result is the number of fields stored, or
//! EOF (-1) when the input ended before the first conversion.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::CString;

use super::ucrt_format::{Len, from_narrow, from_wide, length, wide_argument};
use super::ucrt_runtime::{EINVAL, ENOMEM, invalid, set_errno};
use super::ucrt_string::{Unit, units, units_max};

const SECURE: u64 = 1;
const LEGACY_WIDE: u64 = 2;

fn is_space(c: u32) -> bool {
    c == 0x20 || (9..=13).contains(&c)
}

struct Input<'a, T> {
    text: &'a [T],
    at: usize,
}

impl<T: Unit> Input<'_, T> {
    fn peek(&self) -> Option<u32> {
        self.text.get(self.at).map(|&c| c.into())
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.at += 1;
        }
    }
}

struct Args(*const u64);

impl Args {
    fn next(&mut self) -> usize {
        // SAFETY: the guest's va_list holds a slot per target the format
        // names (the C contract).
        unsafe {
            let v = self.0.read_unaligned();
            self.0 = self.0.add(1);
            v as usize
        }
    }
}

enum Step {
    Next,
    InputFailure,
    MatchFailure,
    Stop, // a secure buffer was too small
}

/// Stores `value` (`size` bytes) at the next target.
fn store_int(target: usize, value: u64, size: usize) {
    // SAFETY: the guest's target of the size its conversion names.
    unsafe { std::ptr::copy_nonoverlapping(value.to_le_bytes().as_ptr(), target as *mut u8, size) };
}

fn digit(c: u32, base: u32) -> Option<u32> {
    char::from_u32(c)?.to_digit(base)
}

/// Scans `input` by `format`.
///
/// # Safety
/// `format` is NUL-terminated; `args` holds the targets it names.
pub(crate) unsafe fn scan<T: Unit>(options: u64, input: &[T], format: *const T, args: *const u64) -> i32 {
    // SAFETY: per the caller.
    let f = unsafe { units(format) };
    let wide = size_of::<T>() == 2;
    let mut input = Input { text: input, at: 0 };
    let mut args = Args(args);
    let (mut assigned, mut converted) = (0i32, false);
    let at = |i: usize| -> u32 { f.get(i).map_or(0, |&c| c.into()) };
    let mut i = 0;
    let end = |assigned: i32, converted: bool| if assigned == 0 && !converted { -1 } else { assigned };
    while i < f.len() {
        let c = at(i);
        if is_space(c) {
            while is_space(at(i)) {
                i += 1;
            }
            input.skip_space();
            continue;
        }
        if c != u32::from(b'%') || at(i + 1) == u32::from(b'%') {
            if c == u32::from(b'%') {
                i += 1;
                input.skip_space();
            }
            match input.peek() {
                None => return end(assigned, converted),
                Some(x) if x == c => {
                    input.at += 1;
                    i += 1;
                }
                Some(_) => return assigned,
            }
            continue;
        }
        i += 1;
        let suppress = at(i) == u32::from(b'*');
        if suppress {
            i += 1;
        }
        let mut width = 0usize;
        while (0x30..=0x39).contains(&at(i)) {
            width = width.saturating_mul(10).saturating_add((at(i) - 0x30) as usize);
            i += 1;
        }
        let len = length(f, &mut i);
        let kind = at(i);
        if kind == 0 {
            return end(assigned, converted);
        }
        i += 1;
        let kind = if kind < 128 { kind as u8 } else { 0xFF };
        if !matches!(kind, b'c' | b'C' | b'[' | b'n') {
            input.skip_space();
        }
        if kind != b'n' && input.peek().is_none() {
            return end(assigned, converted);
        }
        let width = if width == 0 { usize::MAX } else { width };
        let target = |args: &mut Args| if suppress { 0 } else { args.next() };
        let step = match kind {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let base = match kind {
                    b'o' => 8,
                    b'x' | b'X' | b'p' => 16,
                    b'i' => 0,
                    _ => 10,
                };
                match integer(&mut input, base, width) {
                    None => Step::MatchFailure,
                    Some(value) => {
                        let size = match len {
                            _ if kind == b'p' => 8,
                            Len::Hh => 1,
                            Len::H => 2,
                            Len::Ll | Len::I64 => 8,
                            _ => 4,
                        };
                        let t = target(&mut args);
                        if t != 0 {
                            store_int(t, value, size);
                            assigned += 1;
                        }
                        Step::Next
                    }
                }
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => match float(&mut input, width) {
                None => Step::MatchFailure,
                Some(value) => {
                    let t = target(&mut args);
                    if t != 0 {
                        if matches!(len, Len::L | Len::BigL | Len::Ll) {
                            store_int(t, value.to_bits(), 8);
                        } else {
                            store_int(t, u64::from((value as f32).to_bits()), 4);
                        }
                        assigned += 1;
                    }
                    Step::Next
                }
            },
            b'c' | b'C' | b's' | b'S' | b'[' => {
                let wide_target = wide_argument(options & LEGACY_WIDE != 0, wide, kind.is_ascii_uppercase(), len);
                let mut set = None;
                if kind == b'[' {
                    let (members, next) = scanset(f, i);
                    set = Some(members);
                    i = next;
                }
                let is_char = kind == b'c' || kind == b'C';
                let width = if is_char && width == usize::MAX { 1 } else { width };
                let start = input.at;
                while input.at - start < width {
                    let Some(c) = input.peek() else { break };
                    let take = match &set {
                        Some((negated, members)) => members.iter().any(|&(lo, hi)| (lo..=hi).contains(&c)) != *negated,
                        None if kind == b's' || kind == b'S' => !is_space(c),
                        None => true,
                    };
                    if !take {
                        break;
                    }
                    input.at += 1;
                }
                let taken = &input.text[start..input.at];
                if taken.is_empty() && input.peek().is_some() {
                    Step::MatchFailure
                } else if taken.is_empty() || (is_char && taken.len() < width) {
                    Step::InputFailure
                } else {
                    let t = target(&mut args);
                    let size = if suppress || options & SECURE == 0 { usize::MAX } else { args.next() as u32 as usize };
                    if t == 0 {
                        Step::Next
                    } else if wide_target {
                        let wide_units: Vec<u16> = if wide { from_wide(&to_u16(taken)) } else { from_narrow(&to_u8(taken)) };
                        put(t as *mut u16, &wide_units, size, !is_char)
                    } else {
                        let bytes: Vec<u8> = if wide { from_wide(&to_u16(taken)) } else { to_u8(taken) };
                        put(t as *mut u8, &bytes, size, !is_char)
                    }
                    .then_count(&mut assigned)
                }
            }
            b'n' => {
                let t = target(&mut args);
                if t != 0 {
                    let size = match len {
                        Len::Hh => 1,
                        Len::H => 2,
                        Len::Ll | Len::I64 => 8,
                        _ => 4,
                    };
                    store_int(t, input.at as u64, size);
                }
                Step::Next
            }
            _ => Step::MatchFailure,
        };
        match step {
            Step::Next => {
                if kind != b'n' {
                    converted = true;
                }
            }
            Step::InputFailure => return end(assigned, converted),
            Step::MatchFailure | Step::Stop => return assigned,
        }
    }
    assigned
}

trait Count {
    fn then_count(self, assigned: &mut i32) -> Step;
}

impl Count for Step {
    fn then_count(self, assigned: &mut i32) -> Step {
        if matches!(self, Step::Next) {
            *assigned += 1;
        }
        self
    }
}

fn to_u16<T: Unit>(s: &[T]) -> Vec<u16> {
    s.iter().map(|&c| c.into() as u16).collect()
}

fn to_u8<T: Unit>(s: &[T]) -> Vec<u8> {
    s.iter().map(|&c| c.into() as u8).collect()
}

/// Stores `text` (and a terminator for a string) in a buffer of `size`
/// units; too small: an empty string and ENOMEM.
fn put<U: Unit>(target: *mut U, text: &[U], size: usize, terminate: bool) -> Step {
    let need = text.len() + usize::from(terminate);
    if need > size {
        if size > 0 {
            // SAFETY: the buffer holds size > 0 units.
            unsafe { *target = U::default() };
        }
        set_errno(ENOMEM);
        return Step::Stop;
    }
    // SAFETY: the buffer holds `size` >= `need` units (unchecked without
    // the secure bit, as in C).
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), target, text.len());
        if terminate {
            *target.add(text.len()) = U::default();
        }
    }
    Step::Next
}

/// `%[...]` from `f[i]`: whether it is negated, its ranges, and where the
/// format goes on.
fn scanset<T: Unit>(f: &[T], mut i: usize) -> ((bool, Vec<(u32, u32)>), usize) {
    let at = |k: usize| -> u32 { f.get(k).map_or(0, |&c| c.into()) };
    let negated = at(i) == u32::from(b'^');
    if negated {
        i += 1;
    }
    let mut members = vec![];
    let mut first = true;
    while at(i) != 0 && (first || at(i) != u32::from(b']')) {
        let lo = at(i);
        if at(i + 1) == u32::from(b'-') && at(i + 2) != u32::from(b']') && at(i + 2) != 0 {
            let hi = at(i + 2);
            members.push((lo.min(hi), lo.max(hi)));
            i += 3;
        } else {
            members.push((lo, lo));
            i += 1;
        }
        first = false;
    }
    if at(i) == u32::from(b']') {
        i += 1;
    }
    ((negated, members), i)
}

/// An integer of `base` (0: by prefix) in at most `width` units.
fn integer<T: Unit>(input: &mut Input<T>, base: u32, width: usize) -> Option<u64> {
    let start = input.at;
    let left = |input: &Input<T>| width - (input.at - start);
    let mut negative = false;
    if let Some(c @ (0x2B | 0x2D)) = input.peek() {
        negative = c == 0x2D;
        input.at += 1;
    }
    let mut base = base;
    let mut any = false;
    if (base == 0 || base == 16) && left(input) > 0 && input.peek() == Some(0x30) {
        input.at += 1;
        any = true;
        if left(input) > 0 && input.peek().is_some_and(|c| c | 0x20 == 0x78) {
            input.at += 1;
            base = 16;
        } else if base == 0 {
            base = 8;
        }
    }
    if base == 0 {
        base = 10;
    }
    let mut value = 0u64;
    while left(input) > 0 {
        let Some(d) = input.peek().and_then(|c| digit(c, base)) else { break };
        value = value.saturating_mul(base.into()).saturating_add(d.into());
        any = true;
        input.at += 1;
    }
    if !any {
        input.at = start;
        return None;
    }
    Some(if negative { value.wrapping_neg() } else { value })
}

/// A floating-point field in at most `width` units, parsed by `strtod`.
fn float<T: Unit>(input: &mut Input<T>, width: usize) -> Option<f64> {
    let start = input.at;
    let mut text = String::new();
    while input.at - start < width {
        let Some(c) = input.peek().and_then(char::from_u32) else { break };
        let last = text.chars().last();
        let ok = c.is_ascii_hexdigit()
            || c == '.'
            || matches!(c, 'x' | 'X' | 'p' | 'P' | 'n' | 'N' | 'i' | 'I' | 't' | 'T' | 'y' | 'Y' | '(' | ')' | '_')
            || ((c == '+' || c == '-') && (last.is_none() || matches!(last, Some('e' | 'E' | 'p' | 'P'))));
        if !ok {
            break;
        }
        text.push(c);
        input.at += 1;
    }
    // strtod says how much of the candidate is a number.
    let candidate = CString::new(text).ok()?;
    let mut end = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string of ours.
    let value = unsafe { libc::strtod(candidate.as_ptr(), &mut end) };
    let used = end as usize - candidate.as_ptr() as usize;
    input.at = start + used;
    (used > 0).then_some(value)
}

fn sscanf<T: Unit>(options: u64, buffer: *const T, count: usize, format: *const T, args: *const u64) -> i32 {
    if buffer.is_null() || format.is_null() {
        invalid(EINVAL);
        return -1;
    }
    // SAFETY: readable up to its terminator or count.
    let input = unsafe { units_max(buffer, count) };
    // SAFETY: format is NUL-terminated; args are the guest's.
    unsafe { scan(options, input, format, args) }
}

/// (options, buffer, count, format, locale, va_list)
pub extern "win64" fn __stdio_common_vsscanf(
    options: u64,
    buffer: *const u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    args: *const u64,
) -> i32 {
    sscanf(options, buffer, count, format, args)
}

pub extern "win64" fn __stdio_common_vswscanf(
    options: u64,
    buffer: *const u16,
    count: usize,
    format: *const u16,
    _locale: usize,
    args: *const u64,
) -> i32 {
    sscanf(options, buffer, count, format, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_runtime::errno;
    use crate::linux::ucrt_string::w;

    fn scan_n(options: u64, input: &str, format: &str, targets: &[usize]) -> i32 {
        let input = format!("{input}\0");
        let format = format!("{format}\0");
        let slots: Vec<u64> = targets.iter().map(|&t| t as u64).collect();
        __stdio_common_vsscanf(options, input.as_ptr(), usize::MAX, format.as_ptr(), 0, slots.as_ptr())
    }

    fn addr<T>(v: &mut T) -> usize {
        v as *mut T as usize
    }

    #[test]
    fn integers_and_floats() {
        let (mut a, mut b, mut c) = (0i32, 0u32, 0i64);
        let mut h = 0i16;
        assert_eq!(scan_n(0, " 42 -0x1f 077", "%d %x %lli", &[addr(&mut a), addr(&mut b), addr(&mut c)]), 3);
        assert_eq!((a, b as i32, c), (42, -31, 63));
        assert_eq!(scan_n(0, "1234567", "%3d%hd", &[addr(&mut a), addr(&mut h)]), 2);
        assert_eq!((a, h), (123, 4567));
        let (mut f, mut d) = (0f32, 0f64);
        assert_eq!(scan_n(0, "1.5e2 -0.25x", "%f %lf", &[addr(&mut f), addr(&mut d)]), 2);
        assert_eq!((f, d), (150.0, -0.25));
        assert_eq!(scan_n(0, "inf", "%lf", &[addr(&mut d)]), 1);
        assert_eq!(d, f64::INFINITY);
        let mut p = 0usize;
        assert_eq!(scan_n(0, "00000000DEADBEEF", "%p", &[addr(&mut p)]), 1);
        assert_eq!(p, 0xDEAD_BEEF);
        let mut n = 0i32;
        assert_eq!(scan_n(0, "ab  7", "ab %d%n", &[addr(&mut a), addr(&mut n)]), 1);
        assert_eq!((a, n), (7, 5));
    }

    #[test]
    fn failures_and_eof() {
        let mut a = 0i32;
        assert_eq!(scan_n(0, "", "%d", &[addr(&mut a)]), -1);
        assert_eq!(scan_n(0, "   ", "%d", &[addr(&mut a)]), -1);
        assert_eq!(scan_n(0, "abc", "%d", &[addr(&mut a)]), 0);
        assert_eq!(scan_n(0, "1 x", "%d %d", &[addr(&mut a), addr(&mut a)]), 1);
        assert_eq!(scan_n(0, "1", "%d %d", &[addr(&mut a), addr(&mut a)]), 1);
        assert_eq!(scan_n(0, "5%", "%d%%", &[addr(&mut a)]), 1);
        assert_eq!(scan_n(0, "7 8", "%*d %d", &[addr(&mut a)]), 1);
        assert_eq!(a, 8);
    }

    #[test]
    fn strings_sets_and_secure_sizes() {
        let mut s = [0u8; 8];
        let mut t = [0u8; 8];
        let mut c = [0u8; 2];
        assert_eq!(scan_n(0, "  hello world", "%s %3c", &[addr(&mut s), addr(&mut t)]), 2);
        assert_eq!(&s[..6], b"hello\0");
        assert_eq!(&t[..3], b"wor");
        assert_eq!(scan_n(0, "abc123;rest", "%[a-z]%[^;];%c", &[addr(&mut s), addr(&mut t), addr(&mut c)]), 3);
        assert_eq!((&s[..4], &t[..4], c[0]), (&b"abc\0"[..], &b"123\0"[..], b'r'));
        assert_eq!(scan_n(0, "]x", "%[]x]", &[addr(&mut s)]), 1);
        assert_eq!(&s[..3], b"]x\0");
        // secure: the size follows the buffer
        assert_eq!(scan_n(SECURE, "hi there", "%s %s", &[addr(&mut s), 8, addr(&mut t), 3]), 1);
        assert_eq!((&s[..3], t[0], errno()), (&b"hi\0"[..], 0, ENOMEM));
        // a wide target in a narrow function
        let mut ws = [0u16; 8];
        assert_eq!(scan_n(0, "caf\u{e9}", "%ls", &[addr(&mut ws)]), 1);
        assert_eq!(String::from_utf16_lossy(&ws[..4]), "caf\u{e9}");
    }

    #[test]
    fn wide_scanning() {
        let input = w("12 \u{4e2d}\u{6587} x");
        let mut a = 0i32;
        let mut s = [0u16; 8];
        let mut n = [0u8; 8];
        let format = w("%d %s %S");
        let slots = [addr(&mut a) as u64, addr(&mut s) as u64, addr(&mut n) as u64];
        // ISO: %s in a wide function is narrow, so legacy is what MSVC code passes
        assert_eq!(__stdio_common_vswscanf(LEGACY_WIDE, input.as_ptr(), usize::MAX, format.as_ptr(), 0, slots.as_ptr()), 3);
        assert_eq!((a, String::from_utf16_lossy(&s[..2]), n[0]), (12, "\u{4e2d}\u{6587}".into(), b'x'));
    }
}
