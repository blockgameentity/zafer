//! kernel32 virtual memory over the region table
//! (`victory_loader::linux::vm`).

#![allow(non_snake_case)] // named like the exports

use std::ffi::c_void;

use victory_loader::linux::teb::set_last_error;
use victory_loader::linux::vm::{self, MemoryBasicInformation};

use crate::HleModule;
use crate::hle_export;

const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_BAD_LENGTH: u32 = 24;
const ERROR_NOACCESS: u32 = 998;

/// The console's own bits say where memory lives, not how the CPU may use
/// it: `MEM_GRAPHICS` and `MEM_TITLE` in the type, `PAGE_GRAPHICS_*` in the
/// protection. Dropped.
const MEM_CONSOLE: u32 = 0x1000_0000 | 0x4000_0000;
const PAGE_GRAPHICS: u32 = 0x0007_F800;

/// A protection of graphics bits only has no CPU access; 0 stays 0, which
/// is no protection and fails as on Windows.
fn cpu_protect(protect: u32) -> u32 {
    match protect & !PAGE_GRAPHICS {
        0 if protect != 0 => vm::PAGE_NOACCESS,
        cpu => cpu,
    }
}

/// `GetCurrentProcess()` is the only process here.
fn own(process: *mut c_void) -> bool {
    let ours = process as isize == -1;
    if !ours {
        set_last_error(ERROR_INVALID_HANDLE);
    }
    ours
}

fn done<T>(result: Result<T, u32>, failed: T) -> T {
    result.unwrap_or_else(|e| {
        set_last_error(e);
        failed
    })
}

pub extern "win64" fn VirtualAlloc(
    address: *mut c_void,
    size: usize,
    kind: u32,
    protect: u32,
) -> *mut c_void {
    // The placeholder bits are VirtualAlloc2's.
    if kind & (vm::MEM_REPLACE_PLACEHOLDER | vm::MEM_RESERVE_PLACEHOLDER) != 0 {
        set_last_error(vm::ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let result = vm::alloc(
        address as usize,
        size,
        kind & !MEM_CONSOLE,
        cpu_protect(protect),
        vm::PAGE,
    );
    done(result, 0) as *mut c_void
}

pub extern "win64" fn VirtualAllocEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    kind: u32,
    protect: u32,
) -> *mut c_void {
    if !own(process) {
        return std::ptr::null_mut();
    }
    VirtualAlloc(address, size, kind, protect)
}

pub extern "win64" fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32 {
    done(vm::free(address as usize, size, kind).map(|()| 1), 0)
}

pub extern "win64" fn VirtualFreeEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    kind: u32,
) -> i32 {
    if !own(process) {
        return 0;
    }
    VirtualFree(address, size, kind)
}

pub extern "win64" fn VirtualProtect(
    address: *mut c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> i32 {
    if old.is_null() {
        set_last_error(ERROR_NOACCESS);
        return 0;
    }
    let protect = cpu_protect(protect);
    let (lo, hi) = (address as usize & !(vm::PAGE - 1), address as usize + size);
    match vm::protect(address as usize, size, protect) {
        Ok(previous) => {
            // A title that write-protects its own data may keep `old` in
            // those very pages: then it cannot be stored.
            let at = old as usize;
            if vm::writable(protect) || at + 4 <= lo || at >= hi {
                // SAFETY: the caller's out pointer, checked non-null.
                unsafe { old.write(previous) };
            }
            1
        }
        Err(e) => {
            set_last_error(e);
            0
        }
    }
}

pub extern "win64" fn VirtualProtectEx(
    process: *mut c_void,
    address: *mut c_void,
    size: usize,
    protect: u32,
    old: *mut u32,
) -> i32 {
    if !own(process) {
        return 0;
    }
    VirtualProtect(address, size, protect, old)
}

pub extern "win64" fn VirtualQuery(
    address: *const c_void,
    info: *mut MemoryBasicInformation,
    length: usize,
) -> usize {
    if info.is_null() || length < size_of::<MemoryBasicInformation>() {
        set_last_error(ERROR_BAD_LENGTH);
        return 0;
    }
    match vm::query(address as usize) {
        Ok(answer) => {
            // SAFETY: the caller's buffer, at least this large.
            unsafe { info.write_unaligned(answer) };
            size_of::<MemoryBasicInformation>()
        }
        Err(e) => {
            set_last_error(e);
            0
        }
    }
}

pub extern "win64" fn VirtualQueryEx(
    process: *mut c_void,
    address: *const c_void,
    info: *mut MemoryBasicInformation,
    length: usize,
) -> usize {
    if !own(process) {
        return 0;
    }
    VirtualQuery(address, info, length)
}

pub extern "win64" fn GetLargePageMinimum() -> usize {
    vm::LARGE_PAGE
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-memory",
        dlls: &["kernel32.dll", "kernelbase.dll", "api-ms-win-core-memory-*"],
        exports: vec![
            hle_export!(VirtualAlloc),
            hle_export!(VirtualAllocEx),
            hle_export!(VirtualFree),
            hle_export!(VirtualFreeEx),
            hle_export!(VirtualProtect),
            hle_export!(VirtualProtectEx),
            hle_export!(VirtualQuery),
            hle_export!(VirtualQueryEx),
            hle_export!(GetLargePageMinimum),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_loader::linux::teb::last_error;

    #[test]
    fn the_win32_surface_reports_through_the_last_error() {
        let p = VirtualAlloc(
            std::ptr::null_mut(),
            0x2000,
            vm::MEM_COMMIT,
            vm::PAGE_READWRITE,
        );
        assert!(!p.is_null());
        let mut info = MemoryBasicInformation::default();
        assert_eq!(VirtualQuery(p, &mut info, 48), 48);
        assert_eq!((info.state, info.region_size), (vm::MEM_COMMIT, 0x2000));
        assert_eq!(VirtualQuery(p, &mut info, 47), 0);
        assert_eq!(last_error(), ERROR_BAD_LENGTH);

        let mut old = 0;
        assert_eq!(VirtualProtect(p, 1, vm::PAGE_READONLY | 0x800, &mut old), 1);
        assert_eq!(old, vm::PAGE_READWRITE);
        assert_eq!(
            VirtualProtect(p, 1, vm::PAGE_READWRITE, std::ptr::null_mut()),
            0
        );
        assert_eq!(last_error(), ERROR_NOACCESS);
        // `old` inside the pages made read-only is left alone.
        let inside = p.cast::<u32>();
        assert_eq!(VirtualProtect(p, 0x1000, vm::PAGE_READONLY, inside), 1);

        assert_eq!(VirtualFreeEx(4 as *mut c_void, p, 0, vm::MEM_RELEASE), 0);
        assert_eq!(last_error(), ERROR_INVALID_HANDLE);
        assert_eq!(VirtualFree(p, 1, vm::MEM_RELEASE), 0);
        assert_eq!(last_error(), vm::ERROR_INVALID_PARAMETER);
        assert_eq!(VirtualFree(p, 0, vm::MEM_RELEASE), 1);
        let placeholder = vm::MEM_RESERVE | vm::MEM_RESERVE_PLACEHOLDER;
        assert!(VirtualAlloc(std::ptr::null_mut(), 0x10000, placeholder, 1).is_null());
    }
}
