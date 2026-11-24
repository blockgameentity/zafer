//! kernel32: waits on kernel objects, `CloseHandle`, `DuplicateHandle`.
//! The engine is in [`super::handles`]. Waits are never alertable here:
//! the title imports no APC function, so `WAIT_IO_COMPLETION` never comes.

#![allow(non_snake_case)]

use std::sync::Arc;

use victory_loader::linux::teb::set_last_error;

use super::handles::{self, Handle, Object, WAIT_FAILED};
use crate::{HleModule, hle_export};

pub(crate) const ERROR_INVALID_HANDLE: u32 = 6;
pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
const MAXIMUM_WAIT_OBJECTS: usize = 64;
const DUPLICATE_CLOSE_SOURCE: u32 = 1;

pub extern "win64" fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32 {
    WaitForMultipleObjectsEx(1, &handle, 0, milliseconds, 0)
}

pub extern "win64" fn WaitForSingleObjectEx(handle: Handle, milliseconds: u32, _alertable: i32) -> u32 {
    WaitForMultipleObjectsEx(1, &handle, 0, milliseconds, 0)
}

pub extern "win64" fn WaitForMultipleObjects(
    count: u32,
    handles: *const Handle,
    wait_all: i32,
    milliseconds: u32,
) -> u32 {
    WaitForMultipleObjectsEx(count, handles, wait_all, milliseconds, 0)
}

pub extern "win64" fn WaitForMultipleObjectsEx(
    count: u32,
    list: *const Handle,
    wait_all: i32,
    milliseconds: u32,
    _alertable: i32,
) -> u32 {
    let count = count as usize;
    if count == 0 || count > MAXIMUM_WAIT_OBJECTS || list.is_null() {
        set_last_error(ERROR_INVALID_PARAMETER);
        return WAIT_FAILED;
    }
    // SAFETY: the title passes `count` handles.
    let list = unsafe { std::slice::from_raw_parts(list, count) };
    let mut objects: Vec<Arc<dyn Object>> = Vec::with_capacity(count);
    for &h in list {
        match handles::object(h).filter(|o| o.as_waitable().is_some()) {
            Some(o) => objects.push(o),
            None => {
                set_last_error(ERROR_INVALID_HANDLE);
                return WAIT_FAILED;
            }
        }
    }
    let waitables: Vec<_> = objects.iter().filter_map(|o| o.as_waitable()).collect();
    let all = wait_all != 0 && count > 1;
    if all {
        let mut keys: Vec<_> = waitables.iter().map(|w| *w as *const _ as *const () as usize).collect();
        keys.sort_unstable();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            set_last_error(ERROR_INVALID_PARAMETER);
            return WAIT_FAILED;
        }
    }
    // Diagnostics: an endless wait past 10 s names its objects, and their
    // next change is logged.
    #[cfg(feature = "diagnostics")]
    if milliseconds == u32::MAX {
        const WAIT_TIMEOUT: u32 = 258;
        let started = std::time::Instant::now();
        let result = handles::wait(&waitables, all, 10_000);
        if result != WAIT_TIMEOUT {
            return result;
        }
        handles::long_wait(list, &waitables, true);
        let result = handles::wait(&waitables, all, milliseconds);
        handles::long_wait(list, &waitables, false);
        tracing::warn!(secs = started.elapsed().as_secs(), ?list, "long wait ended");
        return result;
    }
    handles::wait(&waitables, all, milliseconds)
}

pub extern "win64" fn CloseHandle(handle: Handle) -> i32 {
    if handles::close(handle) {
        1
    } else {
        set_last_error(ERROR_INVALID_HANDLE);
        0
    }
}

pub extern "win64" fn DuplicateHandle(
    source_process: Handle,
    source: Handle,
    target_process: Handle,
    target: *mut Handle,
    _access: u32,
    _inherit: i32,
    options: u32,
) -> i32 {
    let ours = |p: Handle| handles::get::<handles::Process>(p).is_some();
    if !ours(source_process) || !ours(target_process) {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    let Some(object) = handles::object(source) else {
        set_last_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    // A pseudo handle duplicates to a real handle of what it stands for.
    let copy = handles::insert_dyn(object);
    if options & DUPLICATE_CLOSE_SOURCE != 0 {
        handles::close(source);
    }
    if !target.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { target.write_unaligned(copy) };
    } else {
        handles::close(copy);
    }
    1
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32-wait",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-synch-*",
            "api-ms-win-core-handle-*",
        ],
        exports: vec![
            hle_export!(WaitForSingleObject),
            hle_export!(WaitForSingleObjectEx),
            hle_export!(WaitForMultipleObjects),
            hle_export!(WaitForMultipleObjectsEx),
            hle_export!(CloseHandle),
            hle_export!(DuplicateHandle),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::handles::{CURRENT_PROCESS, CURRENT_THREAD, Event, WAIT_OBJECT_0, WAIT_TIMEOUT};

    #[test]
    fn waits_check_their_handles() {
        let e = handles::insert(Event::new(true, true));
        let f = handles::insert(Event::new(true, false));
        assert_eq!(WaitForSingleObject(e, 0), WAIT_OBJECT_0);
        assert_eq!(WaitForMultipleObjects(2, [f, e].as_ptr(), 0, 0), WAIT_OBJECT_0 + 1);
        assert_eq!(WaitForMultipleObjects(2, [f, e].as_ptr(), 1, 0), WAIT_TIMEOUT);
        assert_eq!(WaitForMultipleObjects(2, [e, e].as_ptr(), 1, 0), WAIT_FAILED);
        assert_eq!(WaitForMultipleObjects(0, [e].as_ptr(), 0, 0), WAIT_FAILED);
        assert_eq!(WaitForSingleObject(0x7777_0000, 0), WAIT_FAILED);
        assert_eq!(victory_loader::linux::teb::last_error(), ERROR_INVALID_HANDLE);
        // The process is waitable and never signalled.
        assert_eq!(WaitForSingleObject(CURRENT_PROCESS, 0), WAIT_TIMEOUT);

        let mut copy = 0;
        assert_eq!(DuplicateHandle(CURRENT_PROCESS, e, CURRENT_PROCESS, &mut copy, 0, 0, 1), 1);
        assert_eq!(CloseHandle(e), 0);
        assert_eq!(WaitForSingleObject(copy, 0), WAIT_OBJECT_0);
        let mut me = 0;
        assert_eq!(DuplicateHandle(CURRENT_PROCESS, CURRENT_THREAD, CURRENT_PROCESS, &mut me, 0, 0, 2), 1);
        assert!(me != CURRENT_THREAD && WaitForSingleObject(me, 0) == WAIT_TIMEOUT);
        assert!(CloseHandle(copy) == 1 && CloseHandle(f) == 1 && CloseHandle(me) == 1);
        assert_eq!(CloseHandle(CURRENT_THREAD), 1);
    }
}
