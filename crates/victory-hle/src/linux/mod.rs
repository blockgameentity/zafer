//! Our own implementation, on Linux, of the Windows DLLs that the host
//! serves on Windows ([`crate::HostPolicy`]): `kernel32`, `ntdll`, the UCRT,
//! `ws2_32` and the rest. See `docs/linux.md`.
//!
//! One file per DLL family and slice (`kernel32_sync.rs`, `ucrt_math.rs`),
//! each with a `module()` listed in [`modules`]. Every function is
//! `extern "win64"` and named exactly like the export; an export by
//! ordinal is listed under the name `#<ordinal>`.

use std::sync::LazyLock;

use crate::HleModule;

mod advapi32_eventing;
mod advapi32_registry;
mod audio_out;
mod bcrypt;
mod combase;
mod crypt32;
mod dbghelp;
#[allow(dead_code)] // used by the slices as they land
pub(crate) mod handles;
mod kernel32_exception;
pub(crate) mod kernel32_file;
pub(crate) mod kernel32_heap;
mod kernel32_interlocked;
mod kernel32_io;
mod kernel32_locale;
mod kernel32_memory;
mod kernel32_path;
mod kernel32_process;
mod kernel32_sync;
mod kernel32_system;
mod kernel32_text;
mod kernel32_thread;
mod kernel32_threadpool;
mod kernel32_time;
mod kernel32_wait;
mod mfplat;
mod mmdevapi;
mod ntdll_exception;
mod oleaut32;
pub(crate) mod system_dlls;
mod ucrt_convert;
mod ucrt_environment;
mod ucrt_filesystem;
mod ucrt_format;
mod ucrt_heap;
mod ucrt_locale;
mod ucrt_math;
pub(crate) mod ucrt_runtime;
mod ucrt_scan;
mod ucrt_stdio;
mod ucrt_string;
mod ucrt_time;
mod ucrt_utility;
pub(crate) mod user32;
mod winhttp;
mod ws2_32;
mod xaudio2;
fn modules() -> Vec<HleModule> {
    vec![
        kernel32_wait::module(),
        kernel32_sync::module(),
        kernel32_process::module(),
        kernel32_system::module(),
        kernel32_thread::module(),
        kernel32_threadpool::module(),
        kernel32_time::module(),
        kernel32_interlocked::module(),
        ucrt_convert::module(),
        ucrt_environment::module(),
        ucrt_filesystem::module(),
        ucrt_heap::module(),
        ucrt_locale::module(),
        ucrt_math::module(),
        ucrt_runtime::module(),
        ucrt_stdio::module(),
        ucrt_string::module(),
        ucrt_time::module(),
        ucrt_utility::module(),
        kernel32_file::module(),
        kernel32_heap::module(),
        kernel32_io::module(),
        kernel32_locale::module(),
        kernel32_memory::module(),
        kernel32_path::module(),
        kernel32_text::module(),
        kernel32_exception::module(),
        ntdll_exception::module(),
        advapi32_eventing::module(),
        advapi32_registry::module(),
        bcrypt::module(),
        combase::module(),
        crypt32::module(),
        dbghelp::module(),
        mfplat::module(),
        oleaut32::module(),
        user32::module(),
        winhttp::module(),
        ws2_32::module(),
        xaudio2::module(),
    ]
}

static MODULES: LazyLock<Vec<HleModule>> = LazyLock::new(modules);

/// Our Linux function for `dll!name`, if we have one.
pub(crate) fn lookup(dll: &str, name: &str) -> Option<usize> {
    MODULES
        .iter()
        .filter(|m| m.serves(dll))
        .flat_map(|m| m.exports.iter())
        .find(|e| e.name == name)
        .map(|e| e.address)
}

/// Whether any of our modules serves `dll`.
pub(crate) fn serves(dll: &str) -> bool {
    MODULES.iter().any(|m| m.serves(dll))
}

/// As [`lookup`], for an import by ordinal.
pub(crate) fn lookup_ordinal(dll: &str, ordinal: u16) -> Option<usize> {
    lookup(dll, &format!("#{ordinal}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two modules that serve one DLL must not both export a name: the
    /// first listed would hide the other.
    #[test]
    fn no_export_is_served_twice() {
        let modules = modules();
        let mut clashes = Vec::new();
        for (i, a) in modules.iter().enumerate() {
            for b in &modules[i + 1..] {
                let overlap = a.dlls.iter().chain(b.dlls).any(|pattern| {
                    let dll = pattern.replace('*', "x");
                    a.serves(&dll) && b.serves(&dll)
                });
                if !overlap {
                    continue;
                }
                for e in &a.exports {
                    if b.exports.iter().any(|f| f.name == e.name) {
                        clashes.push(format!("{} in {} and {}", e.name, a.name, b.name));
                    }
                }
            }
        }
        assert!(clashes.is_empty(), "{clashes:#?}");
    }
}
