//! kernel32: the process (ids, exit, command line, environment, start-up
//! information), last error, debugger questions, pointer encoding.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::sync::{LazyLock, Mutex};

use victory_loader::linux::teb;

use super::handles::{self, CURRENT_PROCESS, Handle};
use super::kernel32_wait::ERROR_INVALID_HANDLE;
use crate::{HleModule, hle_export};

const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

/// A null or NUL-terminated UTF-16 string, lossily.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
pub(crate) unsafe fn wide_string(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: NUL-terminated, by the caller.
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: `len` units were just read.
    Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }))
}

/// A null or NUL-terminated 8-bit string, lossily as UTF-8.
///
/// # Safety
/// `p` is null or points to a NUL-terminated string.
pub(crate) unsafe fn ansi_string(p: *const u8) -> Option<String> {
    // SAFETY: NUL-terminated, by the caller.
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p.cast()) }.to_string_lossy().into_owned())
}

// ---- ids, exit ----

pub extern "win64" fn GetCurrentProcess() -> Handle {
    CURRENT_PROCESS
}

pub extern "win64" fn GetCurrentProcessId() -> u32 {
    std::process::id()
}

pub extern "win64" fn GetProcessId(process: Handle) -> u32 {
    if handles::get::<handles::Process>(process).is_some() {
        std::process::id()
    } else {
        teb::set_last_error(ERROR_INVALID_HANDLE);
        0
    }
}

/// Logged and ended as the CRT's `exit` is.
pub extern "win64" fn ExitProcess(code: u32) -> ! {
    crate::modules::kernel::exit(code as i32)
}

pub extern "win64" fn SetProcessPriorityBoost(process: Handle, _disable: i32) -> i32 {
    (handles::get::<handles::Process>(process).is_some()).into()
}

/// Every other thread sees our earlier stores: `membarrier`.
pub extern "win64" fn FlushProcessWriteBuffers() {
    static REGISTERED: LazyLock<bool> = LazyLock::new(|| {
        // SAFETY: membarrier registration takes no pointers.
        unsafe { libc::syscall(libc::SYS_membarrier, 16 /* REGISTER_PRIVATE_EXPEDITED */, 0, 0) == 0 }
    });
    let cmd = if *REGISTERED { 8 /* PRIVATE_EXPEDITED */ } else { 1 /* GLOBAL */ };
    // SAFETY: as above.
    unsafe { libc::syscall(libc::SYS_membarrier, cmd, 0, 0) };
}

// ---- last error, debugging, pointers ----

pub extern "win64" fn GetLastError() -> u32 {
    teb::last_error()
}

pub extern "win64" fn SetLastError(code: u32) {
    teb::set_last_error(code);
}

/// Whether a tracer (gdb, strace) is attached.
pub extern "win64" fn IsDebuggerPresent() -> i32 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("TracerPid:"))
        .is_some_and(|pid| pid.trim() != "0")
        .into()
}

/// Identity: the pair only has to round-trip.
pub extern "win64" fn EncodePointer(p: usize) -> usize {
    p
}

pub extern "win64" fn DecodePointer(p: usize) -> usize {
    p
}

// ---- command line, environment, start-up information ----

/// Our command line as Windows spells one (what the title gets on Windows).
fn command_line() -> String {
    std::env::args_os()
        .map(|a| {
            let a = a.to_string_lossy().into_owned();
            if !a.is_empty() && !a.contains([' ', '\t', '"']) {
                return a;
            }
            // The inverse of CommandLineToArgvW.
            let mut quoted = String::from('"');
            let mut slashes = 0;
            for c in a.chars() {
                if c == '\\' {
                    slashes += 1;
                    continue;
                }
                let n = if c == '"' { slashes * 2 + 1 } else { slashes };
                quoted.extend(std::iter::repeat_n('\\', n));
                quoted.push(c);
                slashes = 0;
            }
            quoted.extend(std::iter::repeat_n('\\', slashes * 2));
            quoted.push('"');
            quoted
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub extern "win64" fn GetCommandLineA() -> *const u8 {
    static LINE: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let mut l = command_line().into_bytes();
        l.push(0);
        l
    });
    LINE.as_ptr()
}

/// What `SetEnvironmentVariable` changed, by upper-case name (`None`:
/// removed), over the host environment, which we never change (setenv
/// races with every other thread's getenv).
static ENV: LazyLock<Mutex<HashMap<String, Option<String>>>> = LazyLock::new(Default::default);

/// A variable, its name matched without case as on Windows.
pub(crate) fn environment_variable(name: &str) -> Option<String> {
    let key = name.to_ascii_uppercase();
    if let Some(value) = ENV.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return value.clone();
    }
    std::env::var_os(name)
        .or_else(|| {
            std::env::vars_os()
                .find(|(k, _)| k.as_bytes().eq_ignore_ascii_case(name.as_bytes()))
                .map(|(_, v)| v)
        })
        .map(|v| v.to_string_lossy().into_owned())
}

pub(crate) fn set_environment_variable(name: &str, value: Option<&str>) {
    let key = name.to_ascii_uppercase();
    ENV.lock().unwrap_or_else(|e| e.into_inner()).insert(key, value.map(str::to_owned));
}

/// Copies `value` + NUL to `buffer` if it fits: its length, else the size
/// it needs (with the NUL), as `GetEnvironmentVariable` answers.
fn answer<T: Copy + Default>(value: Option<Vec<T>>, buffer: *mut T, size: u32) -> u32 {
    let Some(value) = value else {
        teb::set_last_error(ERROR_ENVVAR_NOT_FOUND);
        return 0;
    };
    if value.len() >= size as usize || buffer.is_null() {
        return value.len() as u32 + 1;
    }
    // SAFETY: the title's buffer holds `size` > len units.
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
        buffer.add(value.len()).write(T::default());
    }
    value.len() as u32
}

pub extern "win64" fn GetEnvironmentVariableW(name: *const u16, buffer: *mut u16, size: u32) -> u32 {
    // SAFETY: the title's NUL-terminated name.
    let value = unsafe { wide_string(name) }.and_then(|n| environment_variable(&n));
    answer(value.map(|v| v.encode_utf16().collect()), buffer, size)
}

pub extern "win64" fn GetEnvironmentVariableA(name: *const u8, buffer: *mut u8, size: u32) -> u32 {
    // SAFETY: the title's NUL-terminated name.
    let value = unsafe { ansi_string(name) }.and_then(|n| environment_variable(&n));
    answer(value.map(String::into_bytes), buffer, size)
}

pub extern "win64" fn SetEnvironmentVariableA(name: *const u8, value: *const u8) -> i32 {
    // SAFETY: the title's NUL-terminated strings (value may be null).
    let (name, value) = unsafe { (ansi_string(name), ansi_string(value)) };
    let Some(name) = name.filter(|n| !n.is_empty() && !n.contains('=')) else {
        teb::set_last_error(super::kernel32_wait::ERROR_INVALID_PARAMETER);
        return 0;
    };
    set_environment_variable(&name, value.as_deref());
    1
}

/// `STARTUPINFOW` (104 bytes): nothing set, no standard handles.
pub extern "win64" fn GetStartupInfoW(info: *mut u8) {
    // SAFETY: the title's 104-byte STARTUPINFOW.
    unsafe {
        info.write_bytes(0, 104);
        info.cast::<u32>().write_unaligned(104);
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-process",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-*",
            "api-ms-win-core-processenvironment-*",
            "api-ms-win-core-errorhandling-*",
            "api-ms-win-core-debug-*",
            "api-ms-win-core-util-*",
            "api-ms-win-core-localization-*",
        ],
        exports: vec![
            hle_export!(GetCurrentProcess),
            hle_export!(GetCurrentProcessId),
            hle_export!(GetProcessId),
            hle_export!(ExitProcess),
            hle_export!(SetProcessPriorityBoost),
            hle_export!(FlushProcessWriteBuffers),
            hle_export!(GetLastError),
            hle_export!(SetLastError),
            hle_export!(IsDebuggerPresent),
            hle_export!(EncodePointer),
            hle_export!(DecodePointer),
            hle_export!(GetCommandLineA),
            hle_export!(GetEnvironmentVariableW),
            hle_export!(GetEnvironmentVariableA),
            hle_export!(SetEnvironmentVariableA),
            hle_export!(GetStartupInfoW),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_reads_the_host_and_keeps_our_changes() {
        let path = std::env::var("PATH").unwrap();
        let mut buffer = [0u8; 4096];
        let n = GetEnvironmentVariableA(c"path".as_ptr().cast(), buffer.as_mut_ptr(), 4096);
        assert_eq!(&buffer[..n as usize], path.as_bytes());
        assert_eq!(GetEnvironmentVariableA(c"PATH".as_ptr().cast(), buffer.as_mut_ptr(), 1), n + 1);

        assert_eq!(SetEnvironmentVariableA(c"Victory_Test".as_ptr().cast(), c"yes".as_ptr().cast()), 1);
        let name: Vec<u16> = "VICTORY_TEST\0".encode_utf16().collect();
        let mut wide = [0u16; 8];
        assert_eq!(GetEnvironmentVariableW(name.as_ptr(), wide.as_mut_ptr(), 8), 3);
        assert_eq!(&wide[..4], &[b'y' as u16, b'e' as u16, b's' as u16, 0]);
        assert!(std::env::var_os("Victory_Test").is_none());
        assert_eq!(SetEnvironmentVariableA(c"VICTORY_TEST".as_ptr().cast(), std::ptr::null()), 1);
        assert_eq!(GetEnvironmentVariableW(name.as_ptr(), wide.as_mut_ptr(), 8), 0);
        assert_eq!(teb::last_error(), ERROR_ENVVAR_NOT_FOUND);
    }

    #[test]
    fn command_line_quotes_like_windows() {
        // SAFETY: our own NUL-terminated line.
        let line = unsafe { ansi_string(GetCommandLineA()) }.unwrap();
        assert!(!line.is_empty());
        assert_eq!(GetProcessId(GetCurrentProcess()), std::process::id());
        let mut info = [0xAAu8; 104];
        GetStartupInfoW(info.as_mut_ptr());
        assert_eq!(info[0], 104);
        assert!(info[4..].iter().all(|&b| b == 0));
    }
}
