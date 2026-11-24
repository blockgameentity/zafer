//! `kernel32`'s exception functions (`api-ms-win-core-errorhandling`,
//! `-debug`): `RaiseException`, vectored handlers, the unhandled-exception
//! filter. `SetUnhandledExceptionFilter` is the portable `kernel` module's,
//! which hands its filter to `seh::set_top_level_filter`.
#![allow(non_snake_case)]

use victory_loader::capture_stub;
use victory_loader::linux::context::{
    Context, EXCEPTION_MAXIMUM_PARAMETERS, EXCEPTION_NONCONTINUABLE, ExceptionPointers, ExceptionRecord,
    STATUS_BREAKPOINT, STATUS_FAIL_FAST_EXCEPTION,
};
use victory_loader::linux::seh;

use crate::{HleModule, hle_export};

extern "win64" fn raise_exception(code: u32, flags: u32, count: u32, arguments: *const usize, caller: *mut Context) {
    let mut record = ExceptionRecord::new(code, flags & EXCEPTION_NONCONTINUABLE, 0);
    if !arguments.is_null() {
        let n = (count as usize).min(EXCEPTION_MAXIMUM_PARAMETERS);
        // SAFETY: the caller's `count` arguments.
        record.information[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(arguments, n) });
        record.number_parameters = n as u32;
    }
    // SAFETY: the stub's capture of the caller: a handler that continues execution resumes
    // there, as if RaiseException had returned.
    seh::raise(&mut record, unsafe { &mut *caller })
}
capture_stub!(RaiseException => raise_exception);

extern "win64" fn debug_break(_: u64, _: u64, _: u64, _: u64, caller: *mut Context) {
    let mut record = ExceptionRecord::new(STATUS_BREAKPOINT, 0, 0);
    record.number_parameters = 1;
    // SAFETY: the stub's capture.
    seh::raise(&mut record, unsafe { &mut *caller })
}
capture_stub!(DebugBreak => debug_break);

pub extern "win64" fn AddVectoredExceptionHandler(first: u32, handler: usize) -> usize {
    seh::add_vectored_handler(false, first != 0, handler)
}

pub extern "win64" fn RemoveVectoredExceptionHandler(handle: usize) -> u32 {
    u32::from(seh::remove_vectored_handler(false, handle))
}

pub extern "win64" fn AddVectoredContinueHandler(first: u32, handler: usize) -> usize {
    seh::add_vectored_handler(true, first != 0, handler)
}

pub extern "win64" fn RemoveVectoredContinueHandler(handle: usize) -> u32 {
    u32::from(seh::remove_vectored_handler(true, handle))
}

/// The CRT calls it itself with a record it made up (a failed security
/// check, `abort`), and decides what to do with the answer.
///
/// # Safety
/// `info` points at a record and a context, or is null.
pub unsafe extern "win64" fn UnhandledExceptionFilter(info: *mut ExceptionPointers) -> i32 {
    // SAFETY: as the caller says.
    if info.is_null() || unsafe { (*info).record.is_null() || (*info).context.is_null() } {
        return 1; // EXCEPTION_EXECUTE_HANDLER
    }
    // SAFETY: as above.
    unsafe { seh::unhandled_exception_filter(info) }
}

/// Ends the process at once, offering the exception to no handler.
///
/// # Safety
/// `record` and `context` are null or valid.
pub unsafe extern "win64" fn RaiseFailFastException(record: *mut ExceptionRecord, context: *mut Context, _flags: u32) {
    // SAFETY: as the caller says.
    let mut own = unsafe { record.as_ref() }.copied().unwrap_or(ExceptionRecord::new(STATUS_FAIL_FAST_EXCEPTION, 0, 0));
    own.flags |= EXCEPTION_NONCONTINUABLE;
    // SAFETY: as above.
    let mut context = unsafe { context.as_ref() }.copied().unwrap_or_else(Context::zeroed);
    seh::terminate(&mut own, &mut context)
}

/// Windows Error Reporting never runs: a file registered for a report is
/// accepted and forgotten.
pub extern "win64" fn WerRegisterFile(_path: *const u16, _kind: u32, _flags: u32) -> i32 {
    0 // S_OK
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "kernel32_exception",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-errorhandling-*",
            "api-ms-win-core-debug-*",
            "api-ms-win-core-windowserrorreporting-*",
        ],
        exports: vec![
            hle_export!(RaiseException),
            hle_export!(DebugBreak),
            hle_export!(AddVectoredExceptionHandler),
            hle_export!(RemoveVectoredExceptionHandler),
            hle_export!(AddVectoredContinueHandler),
            hle_export!(RemoveVectoredContinueHandler),
            hle_export!(UnhandledExceptionFilter),
            hle_export!(RaiseFailFastException),
                hle_export!(WerRegisterFile),
        ],
    }
}
