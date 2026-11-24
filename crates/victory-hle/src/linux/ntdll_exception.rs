//! `ntdll`'s exception and unwind support (`api-ms-win-core-rtlsupport`),
//! on the dispatcher and unwinder of `victory_loader::linux::seh`.
#![allow(non_snake_case)]

use std::ffi::c_void;

use victory_loader::capture_stub;
use victory_loader::linux::context::{
    Context, EXCEPTION_MAXIMUM_PARAMETERS, ExceptionRecord, NonvolatilePointers, RuntimeFunction,
};
use victory_loader::linux::seh;

use crate::{HleExport, HleModule, hle_export};

pub extern "win64" fn RtlLookupFunctionEntry(
    pc: u64,
    image_base: *mut u64,
    _history: *mut c_void,
) -> *mut RuntimeFunction {
    let (function, base) = seh::lookup_function_entry(pc);
    // SAFETY: the caller's out pointer, or null.
    if let Some(out) = unsafe { image_base.as_mut() } {
        *out = base;
    }
    function
}

/// # Safety
/// The title's arguments, as for the Windows function.
pub unsafe extern "win64" fn RtlVirtualUnwind(
    handler_type: u32,
    image_base: u64,
    pc: u64,
    function: *mut RuntimeFunction,
    context: *mut Context,
    handler_data: *mut usize,
    establisher_frame: *mut u64,
    pointers: *mut NonvolatilePointers,
) -> usize {
    // SAFETY: forwarded from the caller.
    unsafe {
        seh::virtual_unwind(
            handler_type,
            image_base,
            pc,
            function,
            context,
            handler_data,
            establisher_frame,
            pointers,
        )
    }
}

pub extern "win64" fn RtlPcToFileHeader(pc: usize, base: *mut usize) -> usize {
    let found = seh::image_base_of(pc);
    // SAFETY: the caller's out pointer, or null.
    if let Some(out) = unsafe { base.as_mut() } {
        *out = found;
    }
    found
}

/// # Safety
/// `context` is a state of this thread; `record` null or a record.
pub unsafe extern "win64" fn RtlRestoreContext(context: *mut Context, record: *mut ExceptionRecord) {
    // SAFETY: forwarded from the caller.
    unsafe { seh::restore(context, record) }
}

extern "win64" fn unwind_ex(target_frame: u64, target_ip: u64, record: *mut ExceptionRecord, value: u64, caller: *mut Context) {
    // SAFETY: the title's RtlUnwindEx arguments; `caller` is the stub's capture.
    unsafe { seh::unwind_ex(target_frame, target_ip, record, value, &*caller, true) }
}
capture_stub!(RtlUnwindEx => unwind_ex);

extern "win64" fn unwind(target_frame: u64, target_ip: u64, record: *mut ExceptionRecord, value: u64, caller: *mut Context) {
    // SAFETY: as above, without the stack arguments.
    unsafe { seh::unwind_ex(target_frame, target_ip, record, value, &*caller, false) }
}
capture_stub!(RtlUnwind => unwind);

extern "win64" fn raise_exception(record: *const ExceptionRecord, _: u64, _: u64, _: u64, caller: *mut Context) {
    // SAFETY: the title's record (a copy is dispatched, as Windows does), and the stub's capture.
    let (mut record, caller) = unsafe { (*record, &mut *caller) };
    record.address = caller.rip as usize;
    record.number_parameters = record.number_parameters.min(EXCEPTION_MAXIMUM_PARAMETERS as u32);
    seh::raise(&mut record, caller)
}
capture_stub!(RtlRaiseException => raise_exception);

extern "win64" fn capture_back_trace(skip: u32, count: u32, frames: *mut usize, hash: *mut u32, caller: *mut Context) -> u16 {
    let mut walked = [0u64; 64];
    // SAFETY: the stub's capture.
    let n = seh::walk_stack(unsafe { &*caller }, &mut walked);
    let mut captured = 0u16;
    let mut sum = 0u32;
    for &pc in walked[..n].iter().skip(skip as usize).take(count as usize) {
        // SAFETY: the caller's array of `count` entries.
        unsafe { *frames.add(usize::from(captured)) = pc as usize };
        captured += 1;
        sum = sum.wrapping_add(pc as u32);
    }
    // SAFETY: the caller's out pointer, or null.
    if let Some(out) = unsafe { hash.as_mut() } {
        *out = sum;
    }
    captured
}
capture_stub!(RtlCaptureStackBackTrace => capture_back_trace);

pub extern "win64" fn RtlAddFunctionTable(entries: usize, count: u32, base: u64) -> u8 {
    u8::from(seh::add_function_table(entries, count as usize, base as usize))
}

pub extern "win64" fn RtlDeleteFunctionTable(table: usize) -> u8 {
    u8::from(seh::delete_function_table(table))
}

pub extern "win64" fn RtlInstallFunctionTableCallback(
    id: u64,
    base: u64,
    len: u32,
    callback: usize,
    context: usize,
    _out_of_process_dll: *const u16,
) -> u8 {
    u8::from(seh::install_function_table_callback(
        id as usize,
        base as usize,
        len as usize,
        callback,
        context,
    ))
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ntdll_exception",
        dlls: &["ntdll.dll", "kernel32.dll", "kernelbase.dll", "api-ms-win-core-rtlsupport-*"],
        exports: vec![
            HleExport { name: "RtlCaptureContext", address: seh::capture_context_address() },
            hle_export!(RtlLookupFunctionEntry),
            hle_export!(RtlVirtualUnwind),
            hle_export!(RtlPcToFileHeader),
            hle_export!(RtlRestoreContext),
            hle_export!(RtlUnwindEx),
            hle_export!(RtlUnwind),
            hle_export!(RtlRaiseException),
            hle_export!(RtlCaptureStackBackTrace),
            hle_export!(RtlAddFunctionTable),
            hle_export!(RtlDeleteFunctionTable),
            hle_export!(RtlInstallFunctionTableCallback),
        ],
    }
}
