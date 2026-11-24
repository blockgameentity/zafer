//! The UCRT's printf engine, for narrow and 16-bit wide output, and the
//! `__stdio_common_v*printf` functions that fill a buffer (the ones that
//! write to a FILE are in `ucrt_stdio`, which also lists these).
//!
//! Arguments come from the guest's `va_list`: a pointer to 8-byte slots
//! (Windows x64 passes every variadic argument, a double too, in one).
//! What differs from glibc, as the UCRT documents it:
//! - `long` is 32 bits; `%I64d`, `%I32d`, `%Id`; `%p` is 16 upper-case hex
//!   digits without `0x`;
//! - `%s`/`%c` take the function's own width, `%S`/`%C` the other, `h`
//!   narrow, `l`/`w` wide; in a wide function only with the legacy-wide
//!   option (which MSVC's headers set unless `_CRT_STDIO_ISO_WIDE_SPECIFIERS`);
//!   `%Z` is an ANSI_STRING or UNICODE_STRING;
//! - a null string prints "(null)"; the 0 flag pads strings too;
//! - `%e` has at least two exponent digits (three with the legacy option),
//!   `%a` 13 digits by default; infinities and NaNs print as inf, nan,
//!   nan(ind), nan(snan) ("1.#INF"... in msvcrt mode);
//! - `%n` is refused (EINVAL) as `_set_printf_count_output` starts;
//! - a character after `%` that starts no conversion is printed as it is.
//!
//! Floating point is correctly rounded with ties to even (the UCRT's rule
//! since Windows 10 2004), from Rust's own exact formatting, so no locale
//! shows. Narrow text is UTF-8 when it meets wide text (where the UCRT
//! would use the C locale's one byte per unit).
//! Not here: positional parameters (the `_p` functions) and locales.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use super::ucrt_runtime::{EINVAL, ERANGE, errno, invalid, set_errno};
use super::ucrt_string::{Unit, units, units_max};

// The options word (corecrt_stdio_config.h).
const LEGACY_VSPRINTF_NULL_TERMINATION: u64 = 1;
const STANDARD_SNPRINTF_BEHAVIOR: u64 = 2;
pub(crate) const LEGACY_WIDE_SPECIFIERS: u64 = 4;
const LEGACY_MSVCRT_COMPATIBILITY: u64 = 8;
const LEGACY_THREE_DIGIT_EXPONENTS: u64 = 16;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Len {
    None,
    Hh,
    H,
    L,
    Ll,
    BigL,
    I32,
    I64,
    W,
    T,
}

/// Whether `%c`, `%s`, `%[` or `%Z` takes a wide argument in a function of
/// `wide` units (the UCRT's `is_wide_character_specifier`).
pub(crate) fn wide_argument(legacy_wide: bool, wide: bool, upper: bool, len: Len) -> bool {
    match len {
        Len::H => false,
        Len::L | Len::W => true,
        Len::T => wide,
        _ => (wide && legacy_wide) != upper,
    }
}

/// Reads one length modifier at `f[*i]`.
pub(crate) fn length<T: Unit>(f: &[T], i: &mut usize) -> Len {
    let at = |k: usize| -> u8 { f.get(k).map_or(0, |&c| c.into().min(255) as u8) };
    let len = match at(*i) {
        b'h' if at(*i + 1) == b'h' => {
            *i += 1;
            Len::Hh
        }
        b'h' => Len::H,
        b'l' if at(*i + 1) == b'l' => {
            *i += 1;
            Len::Ll
        }
        b'l' => Len::L,
        b'j' | b'z' | b't' => Len::Ll,
        b'L' => Len::BigL,
        b'w' => Len::W,
        b'T' => Len::T,
        b'I' => {
            if at(*i + 1) == b'6' && at(*i + 2) == b'4' {
                *i += 2;
                Len::I64
            } else if at(*i + 1) == b'3' && at(*i + 2) == b'2' {
                *i += 2;
                Len::I32
            } else {
                Len::Ll // the pointer's width
            }
        }
        _ => return Len::None,
    };
    *i += 1;
    len
}

/// Narrow units as output units (UTF-8 decoded for wide output).
pub(crate) fn from_narrow<T: Unit>(bytes: &[u8]) -> Vec<T> {
    if size_of::<T>() == 1 {
        bytes.iter().map(|&b| T::from_u32(b.into())).collect()
    } else {
        String::from_utf8_lossy(bytes)
            .encode_utf16()
            .map(|u| T::from_u32(u.into()))
            .collect()
    }
}

/// Wide units as output units (UTF-8 encoded for narrow output).
pub(crate) fn from_wide<T: Unit>(wide: &[u16]) -> Vec<T> {
    if size_of::<T>() == 2 {
        wide.iter().map(|&u| T::from_u32(u.into())).collect()
    } else {
        String::from_utf16_lossy(wide)
            .bytes()
            .map(|b| T::from_u32(b.into()))
            .collect()
    }
}

struct Args(*const u64);

impl Args {
    fn next(&mut self) -> u64 {
        // SAFETY: the guest's va_list holds an 8-byte slot per argument the
        // format names (the C contract).
        unsafe {
            let v = self.0.read_unaligned();
            self.0 = self.0.add(1);
            v
        }
    }
}

fn digits(mut value: u64, base: u64, upper: bool) -> String {
    let set: &[u8; 16] = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
    let mut text = vec![];
    while value != 0 {
        text.push(set[(value % base) as usize]);
        value /= base;
    }
    text.reverse();
    String::from_utf8(text).unwrap_or_default()
}

/// `e+05` from Rust's `e5`: a sign and at least two (or three) digits.
fn exponent(text: &str, mark: char, min_digits: usize) -> String {
    let Some(at) = text.find(mark) else { return text.into() };
    let (mantissa, e) = text.split_at(at);
    let e = &e[1..];
    let (sign, number) = match e.strip_prefix('-') {
        Some(n) => ('-', n),
        None => ('+', e.strip_prefix('+').unwrap_or(e)),
    };
    format!("{mantissa}{mark}{sign}{number:0>min_digits$}")
}

/// `%a` of a finite, non-negative value, without `0x`.
fn hex_float(value: f64, precision: Option<usize>, alt: bool) -> String {
    let bits = value.to_bits();
    let raw_exponent = ((bits >> 52) & 0x7FF) as i32;
    let mut mantissa = bits & ((1 << 52) - 1);
    let mut lead = u64::from(raw_exponent != 0);
    let exp = match (raw_exponent, mantissa) {
        (0, 0) => 0,
        (0, _) => -1022,
        _ => raw_exponent - 1023,
    };
    let precision = precision.unwrap_or(13);
    let mut text = String::new();
    if precision < 13 {
        let shift = (13 - precision as u32) * 4;
        let rest = mantissa & ((1 << shift) - 1);
        mantissa >>= shift;
        let half = 1u64 << (shift - 1);
        let odd = if precision == 0 { lead & 1 == 1 } else { mantissa & 1 == 1 };
        if rest > half || (rest == half && odd) {
            mantissa += 1;
            if mantissa >> (precision * 4) != 0 {
                lead += 1;
                mantissa &= (1 << (precision * 4)) - 1;
            }
        }
        if precision > 0 {
            text = format!("{mantissa:0w$x}", w = precision);
        }
    } else {
        text = format!("{mantissa:013x}");
        text.extend(std::iter::repeat_n('0', precision - 13));
    }
    let point = if precision > 0 || alt { "." } else { "" };
    format!("{lead}{point}{text}p{}{}", if exp < 0 { '-' } else { '+' }, exp.unsigned_abs())
}

/// A finite, non-negative value as `%e`, `%f`, `%g` or `%a` (lower case).
fn finite_text(value: f64, kind: u8, precision: Option<usize>, alt: bool, options: u64) -> String {
    let exp_digits = if options & LEGACY_THREE_DIGIT_EXPONENTS != 0 { 3 } else { 2 };
    let mut text = match kind {
        b'a' => return hex_float(value, precision, alt),
        b'f' => format!("{value:.p$}", p = precision.unwrap_or(6)),
        b'e' => exponent(&format!("{value:.p$e}", p = precision.unwrap_or(6)), 'e', exp_digits),
        _ => {
            let significant = match precision {
                Some(0) => 1,
                Some(p) => p,
                None => 6,
            };
            let e_form = format!("{value:.p$e}", p = significant - 1);
            let x: i32 = e_form[e_form.find('e').unwrap_or(0) + 1..].parse().unwrap_or(0);
            let mut text = if x >= -4 && x < significant as i32 {
                format!("{value:.p$}", p = (significant as i32 - 1 - x) as usize)
            } else {
                exponent(&e_form, 'e', exp_digits)
            };
            if !alt && text.contains('.') {
                let end = text.find('e').unwrap_or(text.len());
                let keep = text[..end].trim_end_matches('0').trim_end_matches('.').len();
                text.replace_range(keep..end, "");
            }
            text
        }
    };
    if alt && !text.contains('.') {
        let at = text.find('e').unwrap_or(text.len());
        text.insert(at, '.');
    }
    text
}

/// The text of |value|; `special` for an infinity or NaN.
fn float_text(value: f64, kind: u8, precision: Option<usize>, alt: bool, options: u64) -> (String, bool) {
    if value.is_finite() {
        return (finite_text(value.abs(), kind | 0x20, precision, alt, options), false);
    }
    let bits = value.to_bits();
    let mantissa = bits & ((1 << 52) - 1);
    let quiet = 1u64 << 51;
    let legacy = options & LEGACY_MSVCRT_COMPATIBILITY != 0;
    let text = if mantissa == 0 {
        if legacy { "1.#INF" } else { "inf" }
    } else if mantissa & quiet == 0 {
        if legacy { "1.#SNAN" } else { "nan(snan)" }
    } else if bits >> 63 == 1 && mantissa == quiet {
        if legacy { "1.#IND" } else { "nan(ind)" }
    } else if legacy {
        "1.#QNAN"
    } else {
        "nan"
    };
    (text.into(), true)
}

/// Formats `format` with the guest's `args`. None for a format the UCRT
/// refuses (a string ending inside a conversion, `%n`, a width that does
/// not fit an int).
///
/// # Safety
/// `format` is NUL-terminated; `args` holds what it names.
pub(crate) unsafe fn format<T: Unit>(options: u64, format: *const T, args: *const u64) -> Option<Vec<T>> {
    // SAFETY: per the caller.
    let f = unsafe { units(format) };
    let wide = size_of::<T>() == 2;
    let legacy_wide = options & LEGACY_WIDE_SPECIFIERS != 0;
    let mut args = Args(args);
    let mut out: Vec<T> = Vec::new();
    let at = |i: usize| -> u32 { f.get(i).map_or(0, |&c| c.into()) };
    let mut i = 0;
    while i < f.len() {
        if at(i) != u32::from(b'%') {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        let (mut left, mut plus, mut space, mut alt, mut zero) = (false, false, false, false, false);
        loop {
            match at(i) {
                0x2D => left = true,
                0x2B => plus = true,
                0x20 => space = true,
                0x23 => alt = true,
                0x30 => zero = true,
                _ => break,
            }
            i += 1;
        }
        let number = |i: &mut usize| -> Option<i32> {
            let mut value: i32 = 0;
            while (0x30..=0x39).contains(&at(*i)) {
                value = value.checked_mul(10)?.checked_add((at(*i) - 0x30) as i32)?;
                *i += 1;
            }
            Some(value)
        };
        let width = if at(i) == u32::from(b'*') {
            i += 1;
            let w = args.next() as i32;
            if w < 0 {
                left = true;
            }
            w.unsigned_abs() as usize
        } else {
            number(&mut i)? as usize
        };
        let mut precision: Option<usize> = None;
        if at(i) == u32::from(b'.') {
            i += 1;
            if at(i) == u32::from(b'*') {
                i += 1;
                let p = args.next() as i32;
                precision = (p >= 0).then_some(p as usize);
            } else {
                precision = Some(number(&mut i)? as usize);
            }
        }
        let len = length(f, &mut i);
        let kind = at(i);
        if kind == 0 {
            return None; // the format ends inside a conversion
        }
        i += 1;
        let kind = if kind < 128 { kind as u8 } else { 0xFF };

        let mut prefix = String::new();
        let mut zeros = 0usize;
        let mut body: Vec<T> = vec![];
        let mut counted: Option<usize> = None; // what the body counts as against the width
        match kind {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' => {
                let raw = args.next();
                let signed = kind == b'd' || kind == b'i';
                let bits = match len {
                    _ if kind == b'p' => 64,
                    Len::Ll | Len::I64 => 64,
                    Len::Hh => 8,
                    Len::H => 16,
                    _ => 32,
                };
                let mut value = if bits == 64 { raw } else { raw & ((1 << bits) - 1) };
                if signed {
                    let extended = ((value << (64 - bits)) as i64) >> (64 - bits);
                    if extended < 0 {
                        prefix.push('-');
                        value = extended.unsigned_abs();
                    } else if plus {
                        prefix.push('+');
                    } else if space {
                        prefix.push(' ');
                    }
                }
                let precision = if kind == b'p' { Some(16) } else { precision };
                let precision = match precision {
                    Some(p) => {
                        zero = false; // a precision takes the 0 flag's place
                        p.min(512)
                    }
                    None => 1,
                };
                let base = match kind {
                    b'o' => 8,
                    b'x' | b'X' | b'p' => 16,
                    _ => 10,
                };
                let text = digits(value, base, kind == b'X' || kind == b'p');
                zeros = precision.saturating_sub(text.len());
                if alt && base == 16 && kind != b'p' && value != 0 {
                    prefix.push_str(if kind == b'x' { "0x" } else { "0X" });
                }
                if alt && base == 8 && zeros == 0 && !text.is_empty() {
                    zeros = 1;
                }
                body = from_narrow(text.as_bytes());
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                let value = f64::from_bits(args.next());
                if precision.is_some_and(|p| p > 1 << 24) {
                    return None;
                }
                let (mut text, special) = float_text(value, kind, precision, alt, options);
                if special {
                    zero = false;
                }
                if value.is_sign_negative() {
                    prefix.push('-');
                } else if plus {
                    prefix.push('+');
                } else if space {
                    prefix.push(' ');
                }
                if !special && (kind == b'a' || kind == b'A') {
                    prefix.push_str("0x");
                }
                if kind.is_ascii_uppercase() {
                    text.make_ascii_uppercase();
                    prefix.make_ascii_uppercase();
                }
                body = from_narrow(text.as_bytes());
            }
            b'c' | b'C' => {
                let raw = args.next();
                if wide_argument(legacy_wide, wide, kind == b'C', len) {
                    let unit = raw as u16;
                    if !wide && (0xD800..=0xDFFF).contains(&unit) {
                        continue; // no narrow form: nothing
                    }
                    body = from_wide(&[unit]);
                } else {
                    body = vec![T::from_u32(u32::from(raw as u8))];
                }
            }
            b's' | b'S' | b'Z' => {
                let mut pointer = args.next() as usize;
                let wide_arg = wide_argument(legacy_wide, wide, kind == b'S', len);
                let limit = precision.unwrap_or(usize::MAX);
                let mut counted_length = None;
                if kind == b'Z' && pointer != 0 {
                    // SAFETY: an ANSI_STRING/UNICODE_STRING: u16 length in
                    // bytes, then the buffer pointer at offset 8.
                    let (bytes, buffer) =
                        unsafe { (*(pointer as *const u16), *((pointer + 8) as *const usize)) };
                    pointer = buffer;
                    counted_length = Some(usize::from(bytes) / if wide_arg { 2 } else { 1 });
                }
                if pointer == 0 {
                    let null = &b"(null)"[..limit.min(6)];
                    body = from_narrow(null);
                } else if wide_arg {
                    // SAFETY: a wide string the guest passed, up to its
                    // terminator, the precision or its counted length.
                    let text = unsafe {
                        match counted_length {
                            Some(n) => std::slice::from_raw_parts(pointer as *const u16, n),
                            None => units_max(pointer as *const u16, limit),
                        }
                    };
                    body = from_wide(text);
                    counted = Some(text.len()); // the UCRT pads by the argument's length
                } else {
                    // SAFETY: as above, narrow.
                    let text = unsafe {
                        match counted_length {
                            Some(n) => std::slice::from_raw_parts(pointer as *const u8, n),
                            None => units_max(pointer as *const u8, limit),
                        }
                    };
                    body = from_narrow(text);
                }
            }
            b'n' => return None, // _set_printf_count_output is off
            _ => {
                // not a conversion: the character itself ("%%" too)
                out.push(f[i - 1]);
                continue;
            }
        }
        let length = prefix.len() + zeros + counted.unwrap_or(body.len());
        let pad = width.saturating_sub(length);
        let fill = |out: &mut Vec<T>, c: u8, n: usize| out.extend(std::iter::repeat_n(T::from_u32(c.into()), n));
        if !left && !zero {
            fill(&mut out, b' ', pad);
        }
        out.extend(prefix.bytes().map(|b| T::from_u32(b.into())));
        if zero && !left {
            fill(&mut out, b'0', pad);
        }
        fill(&mut out, b'0', zeros);
        out.extend(body);
        if left {
            fill(&mut out, b' ', pad);
        }
    }
    Some(out)
}

/// The buffer rules of the UCRT's `common_vsprintf` for each combination
/// of the two snprintf option bits: -1 for an error or a truncation; -2
/// truncated and terminated, with neither bit set.
fn vsprintf<T: Unit>(options: u64, buffer: *mut T, count: usize, f: *const T, args: *const u64) -> i32 {
    if f.is_null() || (count != 0 && buffer.is_null()) {
        invalid(EINVAL);
        return -1;
    }
    // SAFETY: f is NUL-terminated; args are the guest's.
    let Some(text) = (unsafe { format(options, f, args) }) else {
        invalid(EINVAL);
        if !buffer.is_null() && count != 0 {
            // SAFETY: count > 0.
            unsafe { *buffer = T::default() };
        }
        return -1;
    };
    let counts_on = options & STANDARD_SNPRINTF_BEHAVIOR != 0 || buffer.is_null();
    let result = if counts_on || text.len() <= count { text.len() as i32 } else { -1 };
    if buffer.is_null() {
        return result;
    }
    let used = text.len().min(count);
    // SAFETY: the buffer holds count units.
    let put = |at: usize, c: T| unsafe { *buffer.add(at) = c };
    for (k, &c) in text[..used].iter().enumerate() {
        put(k, c);
    }
    if options & LEGACY_VSPRINTF_NULL_TERMINATION != 0 {
        if count == 0 && result != 0 {
            return -1;
        }
        if used != count {
            put(used, T::default());
        }
        result
    } else if options & STANDARD_SNPRINTF_BEHAVIOR != 0 {
        if count != 0 {
            put(if used == count { count - 1 } else { used }, T::default());
        }
        result
    } else {
        if count == 0 {
            return -1;
        }
        if used == count {
            put(count - 1, T::default());
            return -2;
        }
        put(used, T::default());
        result
    }
}

/// `sprintf_s`: everything and the terminator must fit, else the buffer
/// is emptied and the invalid-parameter path reports ERANGE.
fn vsprintf_s<T: Unit>(options: u64, buffer: *mut T, count: usize, f: *const T, args: *const u64) -> i32 {
    if f.is_null() || buffer.is_null() || count == 0 {
        invalid(EINVAL);
        return -1;
    }
    let result = vsprintf(options, buffer, count, f, args);
    if result < 0 {
        // SAFETY: count > 0.
        unsafe { *buffer = T::default() };
    }
    if result == -2 {
        invalid(ERANGE);
        return -1;
    }
    result
}

/// `_vsnprintf_s`: at most `max_count` units; with `_TRUNCATE` as much as
/// fits, and -1 says it was cut.
fn vsnprintf_s<T: Unit>(
    options: u64,
    buffer: *mut T,
    count: usize,
    max_count: usize,
    f: *const T,
    args: *const u64,
) -> i32 {
    if f.is_null() {
        invalid(EINVAL);
        return -1;
    }
    if max_count == 0 && buffer.is_null() && count == 0 {
        return 0;
    }
    if buffer.is_null() || count == 0 {
        invalid(EINVAL);
        return -1;
    }
    let saved = errno();
    let result;
    if count > max_count {
        result = vsprintf(options, buffer, max_count + 1, f, args);
        if result == -2 {
            set_errno(saved);
            return -1; // cut at max_count, as asked
        }
    } else {
        result = vsprintf(options, buffer, count, f, args);
        // SAFETY: count > 0.
        unsafe { *buffer.add(count - 1) = T::default() };
        if result == -2 && max_count == usize::MAX {
            if errno() == ERANGE {
                set_errno(saved);
            }
            return -1;
        }
    }
    if result < 0 {
        // SAFETY: count > 0.
        unsafe { *buffer = T::default() };
        if result == -2 {
            invalid(ERANGE);
        }
        return -1;
    }
    result
}

// ---- the exports: (options, buffer, count, [max_count,] format, locale, va_list)

pub extern "win64" fn __stdio_common_vsprintf(
    options: u64,
    buffer: *mut u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsprintf(options, buffer, count, format, args)
}

pub extern "win64" fn __stdio_common_vswprintf(
    options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsprintf(options, buffer, count, format, args)
}

pub extern "win64" fn __stdio_common_vsprintf_s(
    options: u64,
    buffer: *mut u8,
    count: usize,
    format: *const u8,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsprintf_s(options, buffer, count, format, args)
}

pub extern "win64" fn __stdio_common_vswprintf_s(
    options: u64,
    buffer: *mut u16,
    count: usize,
    format: *const u16,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsprintf_s(options, buffer, count, format, args)
}

pub extern "win64" fn __stdio_common_vsnprintf_s(
    options: u64,
    buffer: *mut u8,
    count: usize,
    max_count: usize,
    format: *const u8,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsnprintf_s(options, buffer, count, max_count, format, args)
}

pub extern "win64" fn __stdio_common_vsnwprintf_s(
    options: u64,
    buffer: *mut u16,
    count: usize,
    max_count: usize,
    format: *const u16,
    _locale: usize,
    args: *const u64,
) -> i32 {
    vsnprintf_s(options, buffer, count, max_count, format, args)
}

/// Formats into a host string, for the FILE functions; None after an
/// invalid format (reported).
pub(crate) fn to_vec<T: Unit>(options: u64, f: *const T, args: *const u64) -> Option<Vec<T>> {
    if f.is_null() {
        invalid(EINVAL);
        return None;
    }
    // SAFETY: f is NUL-terminated; args are the guest's.
    let text = unsafe { format(options, f, args) };
    if text.is_none() {
        invalid(EINVAL);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_string::w;

    /// An argument slot as the guest passes it.
    enum A<'a> {
        I(i64),
        F(f64),
        S(&'a [u8]),
        W(&'a [u16]),
    }

    fn slots(args: &[A]) -> Vec<u64> {
        args.iter()
            .map(|a| match a {
                A::I(v) => *v as u64,
                A::F(v) => v.to_bits(),
                A::S(s) => s.as_ptr() as u64,
                A::W(s) => s.as_ptr() as u64,
            })
            .collect()
    }

    fn fmt(options: u64, f: &str, args: &[A]) -> String {
        let f = format!("{f}\0");
        let slots = slots(args);
        let mut buf = [0u8; 512];
        let n = __stdio_common_vsprintf(options | STANDARD_SNPRINTF_BEHAVIOR, buf.as_mut_ptr(), 512, f.as_ptr(), 0, slots.as_ptr());
        assert!(n >= 0, "{f}");
        String::from_utf8_lossy(&buf[..n as usize]).into_owned()
    }

    fn p(f: &str, args: &[A]) -> String {
        fmt(0, f, args)
    }

    #[test]
    fn integers() {
        use A::I;
        assert_eq!(p("%d|%5d|%-5d|%05d|%+d|% d", &[I(-42), I(42), I(42), I(-42), I(7), I(7)]), "-42|   42|42   |-0042|+7| 7");
        assert_eq!(p("%ld %lu", &[I(0x1_0000_0001), I(-1)]), "1 4294967295"); // long is 32 bits
        assert_eq!(p("%I64d %lld %zd %Id", &[I(-1 << 40), I(1 << 40), I(-5), I(-6)]), "-1099511627776 1099511627776 -5 -6");
        assert_eq!(p("%I32x %hx %hhd", &[I(0x1_2345_6789), I(0x12345), I(0x1FF)]), "23456789 2345 -1");
        assert_eq!(p("%#x %#X %#o %#x %o", &[I(255), I(255), I(8), I(0), I(0)]), "0xff 0XFF 010 0 0");
        assert_eq!(p("%.3d|%.0d|%8.3x|%-+6d|", &[I(5), I(0), I(10), I(3)]), "005||     00a|+3    |");
        assert_eq!(p("%p", &[I(0x12AB)]), "00000000000012AB");
        assert_eq!(p("%*d|%-*d|%.*d", &[I(4), I(1), I(-3), I(2), I(3), I(9)]), "   1|2  |009");
        assert_eq!(p("%u %llu", &[I(-1), I(-1)]), "4294967295 18446744073709551615");
    }

    #[test]
    fn floats() {
        use A::F;
        assert_eq!(p("%f %.2f %.0f %.0f %#.0f", &[F(1.5), F(0.125), F(2.5), F(3.5), F(2.0)]), "1.500000 0.12 2 4 2.");
        assert_eq!(p("%e %.2E %e", &[F(12345.678), F(0.000123), F(0.0)]), "1.234568e+04 1.23E-04 0.000000e+00");
        assert_eq!(fmt(LEGACY_THREE_DIGIT_EXPONENTS, "%e", &[F(1e5)]), "1.000000e+005");
        assert_eq!(p("%e", &[F(1e300)]), "1.000000e+300");
        assert_eq!(p("%g %g %g %g %G %#g", &[F(100000.0), F(1e6), F(0.0001), F(1e-5), F(1.5e-10), F(1.0)]), "100000 1e+06 0.0001 1e-05 1.5E-10 1.00000");
        assert_eq!(p("%g %.3g %g", &[F(1.23456789), F(1234.0), F(0.0)]), "1.23457 1.23e+03 0");
        assert_eq!(p("%a %A %.2a %a %.0a", &[F(1.0), F(-0.5), F(1.999), F(0.0), F(1.5)]), "0x1.0000000000000p+0 -0X1.0000000000000P-1 0x2.00p+0 0x0.0000000000000p+0 0x2p+0");
        assert_eq!(p("%f %F %f %e", &[F(f64::INFINITY), F(f64::NEG_INFINITY), F(f64::from_bits(0xFFF8_0000_0000_0000)), F(f64::from_bits(0x7FF8_0000_0000_0001))]), "inf -INF -nan(ind) nan");
        assert_eq!(p("%f", &[F(f64::from_bits(0x7FF0_0000_0000_0001))]), "nan(snan)");
        assert_eq!(fmt(LEGACY_MSVCRT_COMPATIBILITY, "%f", &[F(f64::INFINITY)]), "1.#INF");
        assert_eq!(p("%08.2f|%-8.1f|%+.1f|%08f", &[F(-1.23456), F(2.25), F(2.0), F(f64::INFINITY)]), "-0001.23|2.2     |+2.0|     inf");
        assert_eq!(p("%.1f %.1f", &[F(0.05), F(0.25)]), "0.1 0.2"); // 0.05 is above the tie; 0.25 ties to even
    }

    #[test]
    fn characters_and_strings() {
        use A::*;
        let wide = w("wide\u{e9}");
        assert_eq!(p("%s|%5s|%-5s|%.2s|%05s", &[S(b"abc\0"), S(b"ab\0"), S(b"ab\0"), S(b"abc\0"), S(b"ab\0")]), "abc|   ab|ab   |ab|000ab");
        assert_eq!(p("%ls %S %ws %hs", &[W(&wide), W(&wide), W(&wide), S(b"n\0")]), "wide\u{e9} wide\u{e9} wide\u{e9} n");
        assert_eq!(p("%s %.3s", &[I(0), I(0)]), "(null) (nu");
        assert_eq!(p("%c%c%lc%C", &[I(65), I(0x142), I(0xE9), I(0x41)]), "AB\u{e9}A");
        assert_eq!(p("%6ls|", &[W(&wide)]), " wide\u{e9}|"); // padded by the argument's 5 units
        assert_eq!(p("100%% %y", &[]), "100% y");
        let ansi: [u64; 2] = [3 | (3 << 16), b"xyz".as_ptr() as u64];
        assert_eq!(p("%Z", &[I(ansi.as_ptr() as i64)]), "xyz");
    }

    fn wfmt(options: u64, f: &str, args: &[A]) -> String {
        let f = w(f);
        let slots = slots(args);
        let mut buf = [0u16; 128];
        let n = __stdio_common_vswprintf(options, buf.as_mut_ptr(), 128, f.as_ptr(), 0, slots.as_ptr());
        String::from_utf16_lossy(&buf[..n as usize])
    }

    #[test]
    fn wide_functions_and_legacy_specifiers() {
        use A::*;
        let wide = w("wid");
        // legacy: %s is the function's own (wide), %S the other
        assert_eq!(wfmt(LEGACY_WIDE_SPECIFIERS, "%s %S %hs %ls %c", &[W(&wide), S(b"nar\0"), S(b"h\0"), W(&wide), I(0x4E2D)]), "wid nar h wid \u{4e2d}");
        // ISO: %s is narrow, %S wide
        assert_eq!(wfmt(0, "%s %S %d", &[S(b"caf\xc3\xa9\0"), W(&wide), I(-1)]), "caf\u{e9} wid -1");
    }

    #[test]
    fn buffer_rules() {
        let f = b"hello\0";
        let call = |options, buf: &mut [u8], count| {
            __stdio_common_vsprintf(options, buf.as_mut_ptr(), count, f.as_ptr(), 0, std::ptr::null())
        };
        let mut buf = [b'#'; 8];
        // _vsnprintf (legacy): no terminator when it does not fit, -1
        assert_eq!(call(LEGACY_VSPRINTF_NULL_TERMINATION, &mut buf, 5), 5);
        assert_eq!(&buf[..6], b"hello#");
        assert_eq!(call(LEGACY_VSPRINTF_NULL_TERMINATION, &mut buf, 3), -1);
        // C99 vsnprintf: counts on and terminates
        assert_eq!(call(STANDARD_SNPRINTF_BEHAVIOR, &mut buf, 3), 5);
        assert_eq!(&buf[..3], b"he\0");
        // neither: -2 when cut, terminated
        assert_eq!(call(0, &mut buf, 3), -2);
        assert_eq!(&buf[..3], b"he\0");
        assert_eq!(call(0, &mut buf, 8), 5);
        // a null buffer counts (_vscprintf)
        assert_eq!(__stdio_common_vsprintf(0, std::ptr::null_mut(), 0, f.as_ptr(), 0, std::ptr::null()), 5);
        // sprintf_s: too small is ERANGE through the invalid-parameter path
        assert_eq!(__stdio_common_vsprintf_s(0, buf.as_mut_ptr(), 5, f.as_ptr(), 0, std::ptr::null()), -1);
        assert_eq!((buf[0], errno()), (0, ERANGE));
        assert_eq!(__stdio_common_vsprintf_s(0, buf.as_mut_ptr(), 6, f.as_ptr(), 0, std::ptr::null()), 5);
        // _snprintf_s with _TRUNCATE and with a count
        assert_eq!(__stdio_common_vsnprintf_s(0, buf.as_mut_ptr(), 4, usize::MAX, f.as_ptr(), 0, std::ptr::null()), -1);
        assert_eq!(&buf[..4], b"hel\0");
        assert_eq!(__stdio_common_vsnprintf_s(0, buf.as_mut_ptr(), 8, 2, f.as_ptr(), 0, std::ptr::null()), -1);
        assert_eq!(&buf[..3], b"he\0");
        assert_eq!(__stdio_common_vsnprintf_s(0, buf.as_mut_ptr(), 4, 10, f.as_ptr(), 0, std::ptr::null()), -1);
        assert_eq!((buf[0], errno()), (0, ERANGE));
        assert_eq!(__stdio_common_vsnprintf_s(0, buf.as_mut_ptr(), 8, 7, f.as_ptr(), 0, std::ptr::null()), 5);
        // %n is refused
        let n = b"%n\0";
        let slot = [buf.as_mut_ptr() as u64];
        assert_eq!(__stdio_common_vsprintf(0, buf.as_mut_ptr(), 8, n.as_ptr(), 0, slot.as_ptr()), -1);
        assert_eq!(errno(), EINVAL);
        assert_eq!(__stdio_common_vsprintf(0, buf.as_mut_ptr(), 8, c"%l".as_ptr().cast(), 0, slot.as_ptr()), -1);
    }
}
