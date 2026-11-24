//! `crypt32`, as the HTTP clients the title ships use it: certificate
//! stores open empty (no server is ever reached, so no chain is ever
//! checked) and `CryptStringToBinaryW` decodes base64 and hex.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use victory_loader::linux::teb;

use crate::{HleModule, hle_export};

const CRYPT_E_NOT_FOUND: u32 = 0x8009_2004;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_INVALID_DATA: u32 = 13;
const ERROR_MORE_DATA: u32 = 234;

/// Every store is this empty one.
static STORE: u8 = 0;

fn store() -> *mut c_void {
    (&raw const STORE).cast_mut().cast()
}

pub extern "win64" fn CertOpenStore(
    _provider: *const i8,
    _encoding: u32,
    _crypt_provider: usize,
    _flags: u32,
    _para: *const c_void,
) -> *mut c_void {
    store()
}

pub extern "win64" fn CertOpenSystemStoreW(_provider: usize, _name: *const u16) -> *mut c_void {
    store()
}

pub extern "win64" fn CertCloseStore(_store: *mut c_void, _flags: u32) -> i32 {
    1
}

pub extern "win64" fn CertFindCertificateInStore(
    _store: *mut c_void,
    _encoding: u32,
    _flags: u32,
    _kind: u32,
    _para: *const c_void,
    _previous: *const c_void,
) -> *const c_void {
    teb::set_last_error(CRYPT_E_NOT_FOUND);
    std::ptr::null()
}

pub extern "win64" fn CertFindChainInStore(
    _store: *mut c_void,
    _encoding: u32,
    _flags: u32,
    _kind: u32,
    _para: *const c_void,
    _previous: *const c_void,
) -> *const c_void {
    teb::set_last_error(CRYPT_E_NOT_FOUND);
    std::ptr::null()
}

pub extern "win64" fn CertFreeCertificateContext(_context: *const c_void) -> i32 {
    1
}

pub extern "win64" fn CertFreeCertificateChain(_chain: *const c_void) {}

const CRYPT_STRING_BASE64HEADER: u32 = 0;
const CRYPT_STRING_BASE64: u32 = 1;
const CRYPT_STRING_BINARY: u32 = 2;
const CRYPT_STRING_BASE64REQUESTHEADER: u32 = 3;
const CRYPT_STRING_HEX: u32 = 4;
const CRYPT_STRING_BASE64_ANY: u32 = 6;
const CRYPT_STRING_ANY: u32 = 7;
const CRYPT_STRING_BASE64X509CRLHEADER: u32 = 9;

fn base64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| !c.is_ascii_whitespace()) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The base64 between a PEM `-----BEGIN ...-----` and `-----END ...-----`.
fn pem(text: &str) -> Option<Vec<u8>> {
    let begin = text.find("-----BEGIN")?;
    let body = &text[begin..];
    let start = body[10..].find("-----")? + 15;
    let end = body[start..].find("-----END")? + start;
    base64(&body[start..end])
}

fn hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).ok()?, 16).ok())
        .collect()
}

fn decode(text: &str, flags: u32) -> Option<Vec<u8>> {
    match flags {
        CRYPT_STRING_BASE64HEADER | CRYPT_STRING_BASE64REQUESTHEADER | CRYPT_STRING_BASE64X509CRLHEADER => pem(text),
        CRYPT_STRING_BASE64 => base64(text),
        CRYPT_STRING_HEX => hex(text),
        CRYPT_STRING_BASE64_ANY => pem(text).or_else(|| base64(text)),
        _ => None,
    }
}

pub extern "win64" fn CryptStringToBinaryW(
    text: *const u16,
    len: u32,
    flags: u32,
    out: *mut u8,
    out_len: *mut u32,
    skip: *mut u32,
    used_flags: *mut u32,
) -> i32 {
    if text.is_null() || out_len.is_null() {
        teb::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let len = if len == 0 {
        // SAFETY: NUL-terminated when no length is given.
        (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count()
    } else {
        len as usize
    };
    // SAFETY: `len` units from the title.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    let string = String::from_utf16_lossy(units);
    let decoded = match flags & 0xFF {
        CRYPT_STRING_BINARY => Some((units.iter().flat_map(|u| u.to_le_bytes()).collect(), CRYPT_STRING_BINARY)),
        CRYPT_STRING_ANY => decode(&string, CRYPT_STRING_BASE64HEADER)
            .map(|d| (d, CRYPT_STRING_BASE64HEADER))
            .or_else(|| decode(&string, CRYPT_STRING_BASE64).map(|d| (d, CRYPT_STRING_BASE64)))
            .or_else(|| Some((units.iter().flat_map(|u| u.to_le_bytes()).collect(), CRYPT_STRING_BINARY))),
        f => decode(&string, f).map(|d| (d, f)),
    };
    let Some((bytes, used)) = decoded else {
        teb::set_last_error(ERROR_INVALID_DATA);
        return 0;
    };
    // SAFETY: the title's out-parameters, each null or writable.
    unsafe {
        if !skip.is_null() {
            *skip = 0;
        }
        if !used_flags.is_null() {
            *used_flags = used;
        }
        let room = *out_len as usize;
        *out_len = bytes.len() as u32;
        if out.is_null() {
            return 1;
        }
        if room < bytes.len() {
            teb::set_last_error(ERROR_MORE_DATA);
            return 0;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
    }
    1
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "crypt32",
        dlls: &["crypt32.dll"],
        exports: vec![
            hle_export!(CertOpenStore),
            hle_export!(CertOpenSystemStoreW),
            hle_export!(CertCloseStore),
            hle_export!(CertFindCertificateInStore),
            hle_export!(CertFindChainInStore),
            hle_export!(CertFreeCertificateContext),
            hle_export!(CertFreeCertificateChain),
            hle_export!(CryptStringToBinaryW),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_w(text: &str, flags: u32) -> Option<Vec<u8>> {
        let w: Vec<u16> = text.encode_utf16().collect();
        let mut len = 0;
        let (null, none): (*mut u8, *mut u32) = (std::ptr::null_mut(), std::ptr::null_mut());
        if CryptStringToBinaryW(w.as_ptr(), w.len() as u32, flags, null, &mut len, none, none) == 0 {
            return None;
        }
        let mut out = vec![0u8; len as usize];
        assert_eq!(CryptStringToBinaryW(w.as_ptr(), w.len() as u32, flags, out.as_mut_ptr(), &mut len, none, none), 1);
        Some(out)
    }

    #[test]
    fn strings_decode_like_windows() {
        assert_eq!(decode_w("aGVsbG8=", CRYPT_STRING_BASE64).unwrap(), b"hello");
        assert_eq!(decode_w("aGVs\r\nbG8", CRYPT_STRING_BASE64).unwrap(), b"hello");
        let pem = "-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";
        assert_eq!(decode_w(pem, CRYPT_STRING_BASE64HEADER).unwrap(), [0, 1, 2]);
        assert_eq!(decode_w(pem, CRYPT_STRING_ANY).unwrap(), [0, 1, 2]);
        assert_eq!(decode_w("de ad", CRYPT_STRING_HEX).unwrap(), [0xDE, 0xAD]);
        assert_eq!(decode_w("!!", CRYPT_STRING_BASE64), None);
        assert_eq!(teb::last_error(), ERROR_INVALID_DATA);

        let w: Vec<u16> = "aGVsbG8=".encode_utf16().collect();
        let (mut out, mut len) = ([0u8; 2], 2);
        let none: *mut u32 = std::ptr::null_mut();
        assert_eq!(CryptStringToBinaryW(w.as_ptr(), 8, CRYPT_STRING_BASE64, out.as_mut_ptr(), &mut len, none, none), 0);
        assert_eq!((teb::last_error(), len), (ERROR_MORE_DATA, 5));
    }

    #[test]
    fn stores_are_empty() {
        let s = CertOpenSystemStoreW(0, std::ptr::null());
        assert!(!s.is_null());
        assert!(CertFindChainInStore(s, 1, 0, 0, std::ptr::null(), std::ptr::null()).is_null());
        assert_eq!(teb::last_error(), CRYPT_E_NOT_FOUND);
        assert_eq!(CertCloseStore(s, 0), 1);
    }
}
