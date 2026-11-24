//! Media Foundation (`mfplat`, `mfreadwrite`): started and stopped, with
//! no media source. The title opens a media file by making attributes for
//! its source reader first; that fails here, and the title's open returns
//! "cannot open" before it reaches a resolver. Its intro video is Bink
//! (a DLL it ships), which this does not touch.
// ponytail: no Media Foundation playback; add sources and readers when a
// title needs a file only Media Foundation decodes.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::LazyLock;
use std::time::Instant;

use super::combase::{E_NOTIMPL, HRESULT, S_OK, put};
use crate::{HleModule, hle_export};

pub extern "win64" fn MFStartup(_version: u32, _flags: u32) -> HRESULT {
    S_OK
}

pub extern "win64" fn MFShutdown() -> HRESULT {
    S_OK
}

/// 100 ns units since an arbitrary start.
pub extern "win64" fn MFGetSystemTime() -> i64 {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    (START.elapsed().as_nanos() / 100) as i64
}

pub extern "win64" fn MFCreateAttributes(out: *mut *mut c_void, _size: u32) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn MFCreateMediaType(out: *mut *mut c_void) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn MFCreateSourceResolver(out: *mut *mut c_void) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn MFCreateSourceReaderFromMediaSource(
    _source: *mut c_void,
    _attributes: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub extern "win64" fn MFCreateSourceReaderFromURL(
    _url: *const u16,
    _attributes: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    put(out, std::ptr::null_mut());
    E_NOTIMPL
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "mfplat",
        dlls: &["mfplat.dll", "mfreadwrite.dll"],
        exports: vec![
            hle_export!(MFStartup),
            hle_export!(MFShutdown),
            hle_export!(MFGetSystemTime),
            hle_export!(MFCreateAttributes),
            hle_export!(MFCreateMediaType),
            hle_export!(MFCreateSourceResolver),
            hle_export!(MFCreateSourceReaderFromMediaSource),
            hle_export!(MFCreateSourceReaderFromURL),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_and_makes_nothing() {
        assert_eq!(MFStartup(0x20070, 1), S_OK);
        let mut out = std::ptr::dangling_mut::<c_void>();
        assert_eq!(MFCreateAttributes(&mut out, 3), E_NOTIMPL);
        assert!(out.is_null());
        let t = MFGetSystemTime();
        assert!(MFGetSystemTime() >= t);
        assert_eq!(MFShutdown(), S_OK);
    }
}
