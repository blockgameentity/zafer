//! `oleaut32`: `BSTR`s. A `BSTR` points at its text, which ends with a NUL
//! and has its length in bytes (without the NUL) in the four bytes before
//! it. Ours sit 8 bytes into their block, so the text is 8-aligned as on
//! 64-bit Windows. Titles import these by ordinal.
#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::{HleExport, HleModule, hle_export};

type Bstr = *mut u16;

/// A `BSTR` of `bytes` bytes, copied from `text` if it is not null.
fn alloc(text: *const u8, bytes: usize) -> Bstr {
    let Ok(prefix) = u32::try_from(bytes) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a fresh block of 8 + bytes + 2; the title passes `bytes`
    // readable bytes at `text` when it is not null.
    unsafe {
        let block = libc::malloc(8 + bytes + 2) as *mut u8;
        if block.is_null() {
            return std::ptr::null_mut();
        }
        (block.add(4) as *mut u32).write_unaligned(prefix);
        let out = block.add(8);
        if text.is_null() {
            out.write_bytes(0, bytes);
        } else {
            std::ptr::copy_nonoverlapping(text, out, bytes);
        }
        out.add(bytes).cast::<u16>().write_unaligned(0);
        out.cast()
    }
}

fn wide_len(text: *const u16) -> usize {
    // SAFETY: the title passes a NUL-terminated string.
    (0..).take_while(|&i| unsafe { *text.add(i) } != 0).count()
}

pub extern "win64" fn SysAllocString(text: *const u16) -> Bstr {
    if text.is_null() {
        return std::ptr::null_mut();
    }
    alloc(text.cast(), wide_len(text) * 2)
}

pub extern "win64" fn SysAllocStringLen(text: *const u16, len: u32) -> Bstr {
    alloc(text.cast(), len as usize * 2)
}

pub extern "win64" fn SysAllocStringByteLen(text: *const u8, len: u32) -> Bstr {
    alloc(text, len as usize)
}

pub extern "win64" fn SysFreeString(s: Bstr) {
    if !s.is_null() {
        // SAFETY: a BSTR from `alloc`, 8 bytes into its block.
        unsafe { libc::free((s as *mut u8).sub(8) as *mut c_void) };
    }
}

pub extern "win64" fn SysStringByteLen(s: Bstr) -> u32 {
    if s.is_null() {
        return 0;
    }
    // SAFETY: a BSTR: its byte length is in the four bytes before it.
    unsafe { (s as *const u8).sub(4).cast::<u32>().read_unaligned() }
}

pub extern "win64" fn SysStringLen(s: Bstr) -> u32 {
    SysStringByteLen(s) / 2
}

fn ordinal(name: &'static str, f: *const ()) -> HleExport {
    HleExport {
        name,
        address: f as usize,
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "oleaut32",
        dlls: &["oleaut32.dll"],
        exports: vec![
            hle_export!(SysAllocString),
            hle_export!(SysAllocStringLen),
            hle_export!(SysAllocStringByteLen),
            hle_export!(SysFreeString),
            hle_export!(SysStringLen),
            hle_export!(SysStringByteLen),
            ordinal("#2", SysAllocString as *const ()),
            ordinal("#4", SysAllocStringLen as *const ()),
            ordinal("#6", SysFreeString as *const ()),
            ordinal("#7", SysStringLen as *const ()),
            ordinal("#149", SysStringByteLen as *const ()),
            ordinal("#150", SysAllocStringByteLen as *const ()),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bstr_layout_is_windows() {
        let text: Vec<u16> = "hello".encode_utf16().collect();
        let s = SysAllocStringLen(text.as_ptr(), 3);
        assert_eq!(s as usize % 8, 0);
        // SAFETY: a 3-character BSTR: prefix, text, NUL.
        unsafe {
            assert_eq!((s as *const u8).sub(4).cast::<u32>().read(), 6);
            assert_eq!(std::slice::from_raw_parts(s, 4), &[104, 101, 108, 0]);
        }
        assert_eq!((SysStringLen(s), SysStringByteLen(s)), (3, 6));
        SysFreeString(s);

        let z = SysAllocStringLen(std::ptr::null(), 2);
        // SAFETY: two zeroed characters and the NUL.
        assert_eq!(unsafe { std::slice::from_raw_parts(z, 3) }, &[0, 0, 0]);
        SysFreeString(z);
        SysFreeString(std::ptr::null_mut());
        assert_eq!(SysStringLen(std::ptr::null_mut()), 0);
    }
}
