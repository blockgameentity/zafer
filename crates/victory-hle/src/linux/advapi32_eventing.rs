//! Event tracing (`advapi32`, `api-ms-win-eventing-*`): providers register
//! and write into nothing. No session ever enables them, so their enable
//! callbacks are never called and they read level and flags 0.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use victory_loader::linux::teb;

use crate::{HleModule, hle_export};

const ERROR_SUCCESS: u32 = 0;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_INVALID_PARAMETER: u32 = 87;

static NEXT: AtomicU64 = AtomicU64::new(1);

fn handle(out: *mut u64) -> u32 {
    if out.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    // SAFETY: checked non-null.
    unsafe { out.write(NEXT.fetch_add(1, Ordering::Relaxed)) };
    ERROR_SUCCESS
}

pub extern "win64" fn EventRegister(_provider: *const c_void, _callback: *const c_void, _context: *mut c_void, out: *mut u64) -> u32 {
    handle(out)
}

pub extern "win64" fn EventUnregister(_handle: u64) -> u32 {
    ERROR_SUCCESS
}

pub extern "win64" fn EventSetInformation(_handle: u64, _class: u32, _info: *const c_void, _len: u32) -> u32 {
    ERROR_SUCCESS
}

pub extern "win64" fn EventEnabled(_handle: u64, _descriptor: *const c_void) -> u8 {
    0
}

pub extern "win64" fn EventProviderEnabled(_handle: u64, _level: u8, _keyword: u64) -> u8 {
    0
}

pub extern "win64" fn EventWrite(_handle: u64, _descriptor: *const c_void, _count: u32, _data: *const c_void) -> u32 {
    ERROR_SUCCESS
}

pub extern "win64" fn EventWriteTransfer(
    _handle: u64,
    _descriptor: *const c_void,
    _activity: *const c_void,
    _related: *const c_void,
    _count: u32,
    _data: *const c_void,
) -> u32 {
    ERROR_SUCCESS
}

pub extern "win64" fn RegisterTraceGuidsW(
    _callback: *const c_void,
    _context: *mut c_void,
    _control: *const c_void,
    _count: u32,
    _registrations: *mut c_void,
    _mof_path: *const u16,
    _mof_resource: *const u16,
    out: *mut u64,
) -> u32 {
    handle(out)
}

pub extern "win64" fn UnregisterTraceGuids(_handle: u64) -> u32 {
    ERROR_SUCCESS
}

/// Only valid inside an enable callback, which never comes.
pub extern "win64" fn GetTraceLoggerHandle(_buffer: *mut c_void) -> u64 {
    teb::set_last_error(ERROR_INVALID_HANDLE);
    u64::MAX
}

pub extern "win64" fn GetTraceEnableLevel(_logger: u64) -> u8 {
    0
}

pub extern "win64" fn GetTraceEnableFlags(_logger: u64) -> u32 {
    0
}

/// Variadic on Windows; the arguments past these are never read.
pub extern "win64" fn TraceMessage(_logger: u64, _flags: u32, _guid: *const c_void, _number: u16) -> u32 {
    ERROR_SUCCESS
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "advapi32-eventing",
        dlls: &["advapi32.dll", "api-ms-win-eventing-*"],
        exports: vec![
            hle_export!(EventRegister),
            hle_export!(EventUnregister),
            hle_export!(EventSetInformation),
            hle_export!(EventEnabled),
            hle_export!(EventProviderEnabled),
            hle_export!(EventWrite),
            hle_export!(EventWriteTransfer),
            hle_export!(RegisterTraceGuidsW),
            hle_export!(UnregisterTraceGuids),
            hle_export!(GetTraceLoggerHandle),
            hle_export!(GetTraceEnableLevel),
            hle_export!(GetTraceEnableFlags),
            hle_export!(TraceMessage),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_register_and_stay_disabled() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(EventRegister(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), &mut a), ERROR_SUCCESS);
        assert_eq!(EventRegister(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), &mut b), ERROR_SUCCESS);
        assert!(a != 0 && a != b);
        assert_eq!(EventEnabled(a, std::ptr::null()), 0);
        assert_eq!(EventUnregister(a), ERROR_SUCCESS);
        assert_eq!(EventRegister(std::ptr::null(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()), ERROR_INVALID_PARAMETER);
    }
}
