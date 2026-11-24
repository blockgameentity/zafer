//! The Windows x64 structures exception handling passes to title code
//! (`winnt.h` layouts), and the status codes and flags that go with them.

use std::ffi::c_void;

/// `M128A`: one XMM register.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct M128A {
    pub low: u64,
    pub high: u64,
}

/// `XMM_SAVE_AREA32`: the FXSAVE image.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct XmmSaveArea32 {
    pub control_word: u16,
    pub status_word: u16,
    pub tag_word: u8,
    pub reserved1: u8,
    pub error_opcode: u16,
    pub error_offset: u32,
    pub error_selector: u16,
    pub reserved2: u16,
    pub data_offset: u32,
    pub data_selector: u16,
    pub reserved3: u16,
    pub mx_csr: u32,
    pub mx_csr_mask: u32,
    pub float_registers: [M128A; 8],
    pub xmm: [M128A; 16],
    pub reserved4: [u8; 96],
}

/// `CONTEXT` (x64). The integer registers are an array in x86 register
/// numbering ([`RAX`] .. [`R15`]), as unwind codes name them.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct Context {
    pub home: [u64; 6],
    pub context_flags: u32,
    pub mx_csr: u32,
    pub seg_cs: u16,
    pub seg_ds: u16,
    pub seg_es: u16,
    pub seg_fs: u16,
    pub seg_gs: u16,
    pub seg_ss: u16,
    pub eflags: u32,
    /// Dr0..Dr3, Dr6, Dr7.
    pub dr: [u64; 6],
    pub gpr: [u64; 16],
    pub rip: u64,
    pub flt_save: XmmSaveArea32,
    /// Unused by x64 Windows; a context made from a fault carries the upper
    /// halves of ymm0..15 in the first 16, as [`Context::vector_control`] says.
    pub vector_register: [M128A; 26],
    pub vector_control: u64,
    pub debug_control: u64,
    pub last_branch_to_rip: u64,
    pub last_branch_from_rip: u64,
    pub last_exception_to_rip: u64,
    pub last_exception_from_rip: u64,
}

impl Context {
    pub fn zeroed() -> Self {
        // SAFETY: every field is an integer or an array of them.
        unsafe { std::mem::zeroed() }
    }

    pub fn rsp(&self) -> u64 {
        self.gpr[RSP]
    }

    pub fn xmm(&mut self) -> &mut [M128A; 16] {
        &mut self.flt_save.xmm
    }
}

pub const RAX: usize = 0;
pub const RCX: usize = 1;
pub const RDX: usize = 2;
pub const RBX: usize = 3;
pub const RSP: usize = 4;
pub const RBP: usize = 5;
pub const RSI: usize = 6;
pub const RDI: usize = 7;
pub const R8: usize = 8;
pub const R9: usize = 9;
pub const R10: usize = 10;
pub const R11: usize = 11;
pub const R12: usize = 12;
pub const R13: usize = 13;
pub const R14: usize = 14;
pub const R15: usize = 15;

/// [`Context::vector_control`] of a context whose ymm upper halves are in
/// `vector_register[..16]`, and of one where they were all zero.
pub const YMM_SAVED: u64 = 0x5649_4354_594d_4d31;
pub const YMM_CLEAN: u64 = 0x5649_4354_594d_4d30;

pub const CONTEXT_AMD64: u32 = 0x0010_0000;
pub const CONTEXT_CONTROL: u32 = CONTEXT_AMD64 | 0x1;
pub const CONTEXT_INTEGER: u32 = CONTEXT_AMD64 | 0x2;
pub const CONTEXT_SEGMENTS: u32 = CONTEXT_AMD64 | 0x4;
pub const CONTEXT_FLOATING_POINT: u32 = CONTEXT_AMD64 | 0x8;
pub const CONTEXT_FULL: u32 = CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_FLOATING_POINT;

pub const EXCEPTION_MAXIMUM_PARAMETERS: usize = 15;

/// `EXCEPTION_RECORD`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ExceptionRecord {
    pub code: u32,
    pub flags: u32,
    pub record: *mut ExceptionRecord,
    pub address: usize,
    pub number_parameters: u32,
    pub information: [usize; EXCEPTION_MAXIMUM_PARAMETERS],
}

impl ExceptionRecord {
    pub fn new(code: u32, flags: u32, address: usize) -> Self {
        ExceptionRecord {
            code,
            flags,
            record: std::ptr::null_mut(),
            address,
            number_parameters: 0,
            information: [0; EXCEPTION_MAXIMUM_PARAMETERS],
        }
    }

    /// The parameters it carries.
    pub fn parameters(&self) -> &[usize] {
        let n = (self.number_parameters as usize).min(EXCEPTION_MAXIMUM_PARAMETERS);
        &self.information[..n]
    }
}

/// `EXCEPTION_POINTERS`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ExceptionPointers {
    pub record: *mut ExceptionRecord,
    pub context: *mut Context,
}

/// `RUNTIME_FUNCTION`: RVAs from the image base.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeFunction {
    pub begin: u32,
    pub end: u32,
    pub unwind_data: u32,
}

/// `KNONVOLATILE_CONTEXT_POINTERS`: where an unwind found each register.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NonvolatilePointers {
    pub floating: [*mut M128A; 16],
    pub integer: [*mut u64; 16],
}

/// A language handler (`PEXCEPTION_ROUTINE`).
pub type LanguageHandler = unsafe extern "win64" fn(
    record: *mut ExceptionRecord,
    establisher_frame: u64,
    context: *mut Context,
    dispatcher: *mut DispatcherContext,
) -> i32;

/// `DISPATCHER_CONTEXT`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DispatcherContext {
    pub control_pc: u64,
    pub image_base: u64,
    pub function_entry: *mut RuntimeFunction,
    pub establisher_frame: u64,
    pub target_ip: u64,
    pub context_record: *mut Context,
    pub language_handler: Option<LanguageHandler>,
    pub handler_data: *mut c_void,
    /// `UNWIND_HISTORY_TABLE*`: passed along, never read.
    pub history_table: *mut c_void,
    pub scope_index: u32,
    pub fill0: u32,
}

impl DispatcherContext {
    pub fn zeroed() -> Self {
        // SAFETY: integers, raw pointers and an `Option` of a function pointer, all valid as zero.
        unsafe { std::mem::zeroed() }
    }
}

/// A vectored handler or an unhandled-exception filter.
pub type ExceptionFilter = unsafe extern "win64" fn(info: *mut ExceptionPointers) -> i32;

// ExceptionRecord.flags
pub const EXCEPTION_NONCONTINUABLE: u32 = 0x01;
pub const EXCEPTION_UNWINDING: u32 = 0x02;
pub const EXCEPTION_EXIT_UNWIND: u32 = 0x04;
pub const EXCEPTION_STACK_INVALID: u32 = 0x08;
pub const EXCEPTION_NESTED_CALL: u32 = 0x10;
pub const EXCEPTION_TARGET_UNWIND: u32 = 0x20;
pub const EXCEPTION_COLLIDED_UNWIND: u32 = 0x40;

// What a filter answers.
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;

// EXCEPTION_DISPOSITION: what a language handler answers.
pub const DISPOSITION_CONTINUE_EXECUTION: i32 = 0;
pub const DISPOSITION_CONTINUE_SEARCH: i32 = 1;
pub const DISPOSITION_NESTED_EXCEPTION: i32 = 2;
pub const DISPOSITION_COLLIDED_UNWIND: i32 = 3;

pub const STATUS_GUARD_PAGE_VIOLATION: u32 = 0x8000_0001;
pub const STATUS_DATATYPE_MISALIGNMENT: u32 = 0x8000_0002;
pub const STATUS_BREAKPOINT: u32 = 0x8000_0003;
pub const STATUS_SINGLE_STEP: u32 = 0x8000_0004;
pub const STATUS_LONGJUMP: u32 = 0x8000_0026;
pub const STATUS_UNWIND_CONSOLIDATE: u32 = 0x8000_0029;
pub const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const STATUS_IN_PAGE_ERROR: u32 = 0xC000_0006;
pub const STATUS_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const STATUS_NONCONTINUABLE_EXCEPTION: u32 = 0xC000_0025;
pub const STATUS_INVALID_DISPOSITION: u32 = 0xC000_0026;
pub const STATUS_UNWIND: u32 = 0xC000_0027;
pub const STATUS_BAD_STACK: u32 = 0xC000_0028;
pub const STATUS_FLOAT_DENORMAL_OPERAND: u32 = 0xC000_008D;
pub const STATUS_FLOAT_DIVIDE_BY_ZERO: u32 = 0xC000_008E;
pub const STATUS_FLOAT_INEXACT_RESULT: u32 = 0xC000_008F;
pub const STATUS_FLOAT_INVALID_OPERATION: u32 = 0xC000_0090;
pub const STATUS_FLOAT_OVERFLOW: u32 = 0xC000_0091;
pub const STATUS_FLOAT_UNDERFLOW: u32 = 0xC000_0093;
pub const STATUS_INTEGER_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const STATUS_INTEGER_OVERFLOW: u32 = 0xC000_0095;
pub const STATUS_PRIVILEGED_INSTRUCTION: u32 = 0xC000_0096;
pub const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
pub const STATUS_STACK_BUFFER_OVERRUN: u32 = 0xC000_0409;
pub const STATUS_FAIL_FAST_EXCEPTION: u32 = 0xC000_0602;

// UNWIND_INFO flags, and RtlVirtualUnwind's handler type.
pub const UNW_FLAG_NHANDLER: u32 = 0;
pub const UNW_FLAG_EHANDLER: u32 = 1;
pub const UNW_FLAG_UHANDLER: u32 = 2;
pub const UNW_FLAG_CHAININFO: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn layouts_are_the_windows_ones() {
        assert_eq!(size_of::<M128A>(), 16);
        assert_eq!(size_of::<XmmSaveArea32>(), 512);
        assert_eq!(offset_of!(XmmSaveArea32, mx_csr), 24);
        assert_eq!(offset_of!(XmmSaveArea32, xmm), 160);

        assert_eq!((size_of::<Context>(), align_of::<Context>()), (1232, 16));
        assert_eq!(offset_of!(Context, context_flags), 0x30);
        assert_eq!(offset_of!(Context, mx_csr), 0x34);
        assert_eq!(offset_of!(Context, seg_cs), 0x38);
        assert_eq!(offset_of!(Context, seg_ss), 0x42);
        assert_eq!(offset_of!(Context, eflags), 0x44);
        assert_eq!(offset_of!(Context, dr), 0x48);
        assert_eq!(offset_of!(Context, gpr), 0x78);
        assert_eq!(offset_of!(Context, gpr) + 8 * RSP, 0x98);
        assert_eq!(offset_of!(Context, gpr) + 8 * R15, 0xF0);
        assert_eq!(offset_of!(Context, rip), 0xF8);
        assert_eq!(offset_of!(Context, flt_save), 0x100);
        assert_eq!(offset_of!(Context, flt_save) + offset_of!(XmmSaveArea32, xmm), 0x1A0);
        assert_eq!(offset_of!(Context, vector_register), 0x300);
        assert_eq!(offset_of!(Context, vector_control), 0x4A0);
        assert_eq!(offset_of!(Context, last_exception_from_rip), 0x4C8);

        assert_eq!(size_of::<ExceptionRecord>(), 152);
        assert_eq!(offset_of!(ExceptionRecord, address), 16);
        assert_eq!(offset_of!(ExceptionRecord, number_parameters), 24);
        assert_eq!(offset_of!(ExceptionRecord, information), 32);
        assert_eq!(size_of::<ExceptionPointers>(), 16);
        assert_eq!(size_of::<RuntimeFunction>(), 12);
        assert_eq!(size_of::<NonvolatilePointers>(), 256);

        assert_eq!(size_of::<DispatcherContext>(), 0x50);
        assert_eq!(offset_of!(DispatcherContext, establisher_frame), 0x18);
        assert_eq!(offset_of!(DispatcherContext, context_record), 0x28);
        assert_eq!(offset_of!(DispatcherContext, language_handler), 0x30);
        assert_eq!(offset_of!(DispatcherContext, history_table), 0x40);
        assert_eq!(offset_of!(DispatcherContext, scope_index), 0x48);
    }
}
