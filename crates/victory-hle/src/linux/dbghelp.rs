//! `dbghelp`: the title's crash handler loads it by name to write a
//! minidump. No dump is written; the call fails as for a full disk.
#![allow(non_snake_case)]

use std::ffi::c_void;

use victory_loader::linux::teb;

use crate::{HleModule, hle_export};

const ERROR_NOT_SUPPORTED: u32 = 50;

pub extern "win64" fn MiniDumpWriteDump(
    _process: usize,
    _process_id: u32,
    _file: usize,
    _kind: u32,
    _exception: *const c_void,
    _user_streams: *const c_void,
    _callback: *const c_void,
) -> i32 {
    tracing::warn!("MiniDumpWriteDump: no minidumps on Linux");
    teb::set_last_error(ERROR_NOT_SUPPORTED);
    0
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "dbghelp",
        dlls: &["dbghelp.dll"],
        exports: vec![hle_export!(MiniDumpWriteDump)],
    }
}
