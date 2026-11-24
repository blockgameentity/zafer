//! Structured exception handling for title code: faults and
//! `RaiseException` are dispatched through the images' `.pdata`, as the
//! Windows kernel and `ntdll` do. `RtlDispatchException` and `RtlUnwindEx`
//! as the public documentation and the MSVC language handlers define them;
//! `__C_specific_handler` and `__CxxFrameHandler*` are the title's own
//! (its `vcruntime140*.dll` is mapped, not replaced) and call back into
//! the `Rtl*` functions here.
//!
//! Where a walk ends: a pc in our code gets the boundary entry (`asm.rs`),
//! so an exception that reaches a frame of ours is unhandled; it does not
//! travel through our code to title frames further out. Every call from
//! here into title code goes through `asm::frame_call`, which leads a walk
//! started inside it back into title frames.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::asm;
use super::context::*;
use super::unwind::{self, Entry};

/// Makes the image mapped at `base` (`size` bytes) known to the
/// dispatcher and unwinder: its exception directory is read from its
/// headers.
pub fn register_image(base: usize, size: usize) {
    super::signals::install();
    unwind::add_image(base, size);
}

/// Turns faults in title code into exceptions (installs the signal
/// handler); unhandled ones are reported to stderr and the crash log.
pub fn report_crashes() {
    super::signals::install();
}

/// Forgets an image before it is unmapped.
pub fn unregister_image(base: usize) {
    unwind::remove_image(base);
}

/// The name crash reports give the image at `base` (else its export
/// directory's).
pub fn name_image(base: usize, name: &str) {
    unwind::name_image(base, name);
}

/// The base of the registered image holding `address`.
pub fn image_base(address: usize) -> Option<usize> {
    unwind::image_of(address).map(|(base, _)| base)
}

/// `module+rva` of a registered image, or the bare address.
pub fn locate(address: usize) -> String {
    match unwind::image_of(address) {
        Some((base, name)) => format!("{name}+{:#x}", address - base),
        None => host_location(address).unwrap_or_else(|| format!("{address:#x}")),
    }
}

/// `file+offset (symbol)` of an address in our own binary or a host
/// library, for `addr2line -e <file> <offset>`.
fn host_location(address: usize) -> Option<String> {
    // SAFETY: Dl_info is plain data, filled by dladdr.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: dladdr only reads the loader's tables.
    if unsafe { libc::dladdr(address as *const libc::c_void, &mut info) } == 0
        || info.dli_fname.is_null()
    {
        return None;
    }
    // SAFETY: dladdr gives NUL-terminated names that live as long as the module.
    let file = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
    let file = file.rsplit('/').next().unwrap_or_default().to_owned();
    let mut text = format!("{file}+{:#x}", address - info.dli_fbase as usize);
    if !info.dli_sname.is_null() {
        // SAFETY: as above.
        let name = unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy();
        text += &format!(" ({name})");
    }
    Some(text)
}

/// The calling thread's stack `[StackLimit, StackBase)` from its TEB
/// (`gs:[0x10]`, `gs:[0x08]`), or `None` for a thread without one.
pub(crate) fn stack_bounds() -> Option<(u64, u64)> {
    let mut gs = 0u64;
    // SAFETY: ARCH_GET_GS writes the GS base to `gs`.
    let ok = unsafe { libc::syscall(libc::SYS_arch_prctl, 0x1004, &mut gs as *mut u64) } == 0;
    if !ok || gs == 0 {
        return None;
    }
    let base = asm::read64(gs + 8)?;
    let limit = asm::read64(gs + 0x10)?;
    (limit != 0 && limit < base).then_some((limit, base))
}

struct Stack(Option<(u64, u64)>);

impl Stack {
    fn current() -> Stack {
        Stack(stack_bounds())
    }
    fn contains(&self, at: u64) -> bool {
        self.0.is_none_or(|(low, high)| at >= low && at < high)
    }
}

// ---- the frame call's language handler (asm.rs) -------------------------------------------

/// The `kind` of a frame call: what a walk that comes across it is told.
/// `PASS`: nothing, the walk goes on in the frames of `resume`.
const PASS: u64 = 0;
const EXCEPTION: u64 = 1;
const UNWIND: u64 = 2;
const UNWINDING: u32 = EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND;

/// What `RtlpExecuteHandlerForException` / `ForUnwind` answer when a walk
/// comes across them: a second exception raised while a handler of the
/// first ran (nested), or while an unwind was calling a handler (collided:
/// the new walk takes over where that unwind had got to).
pub(crate) unsafe extern "win64" fn frame_call_handler(
    record: *mut ExceptionRecord,
    frame: u64,
    _context: *mut Context,
    dispatcher: *mut DispatcherContext,
) -> i32 {
    // SAFETY: `frame` is a frame call's frame (its establisher frame), whose
    // slots hold the kind and the interrupted dispatcher context; the
    // dispatcher passes valid pointers.
    unsafe {
        let kind = *((frame + 0x20) as *const u64);
        let interrupted = *((frame + 0x28) as *const *const DispatcherContext);
        let d = &mut *dispatcher;
        if kind == EXCEPTION && (*record).flags & UNWINDING == 0 {
            d.establisher_frame = (*interrupted).establisher_frame;
            return DISPOSITION_NESTED_EXCEPTION;
        }
        if kind == UNWIND {
            let target_ip = d.target_ip;
            *d = *interrupted;
            d.target_ip = target_ip;
            return DISPOSITION_COLLIDED_UNWIND;
        }
    }
    DISPOSITION_CONTINUE_SEARCH
}

/// Calls a language handler from a frame that leads a walk to `resume`.
fn call_handler(
    resume: *const Context,
    kind: u64,
    handler: usize,
    record: &mut ExceptionRecord,
    frame: u64,
    context: *mut Context,
    d: &mut DispatcherContext,
) -> i32 {
    let a = [record as *mut _ as u64, frame, context as u64, d as *mut _ as u64];
    // SAFETY: a language handler named by the unwind data of a registered image; `resume` is
    // a live context of this thread.
    unsafe { asm::frame_call(&*resume, kind, handler, a) as i32 }
}

/// Calls a vectored handler or a filter from a frame that leads a walk to
/// `resume`.
fn call_filter(resume: &Context, filter: usize, info: &mut ExceptionPointers) -> i32 {
    // SAFETY: a handler the title registered.
    unsafe { asm::frame_call(resume, PASS, filter, [info as *mut _ as u64, 0, 0, 0]) as i32 }
}

// ---- vectored handlers and the top-level filter -------------------------------------------

/// Vectored exception handlers, then vectored continue handlers: (handle, handler).
static VECTORED: Mutex<[Vec<(usize, usize)>; 2]> = Mutex::new([Vec::new(), Vec::new()]);
static NEXT_HANDLE: AtomicUsize = AtomicUsize::new(0x10);
static TOP_FILTER: AtomicUsize = AtomicUsize::new(0);
const MAX_VECTORED: usize = 64;

/// `AddVectoredExceptionHandler` (`continue_handler` false) or
/// `AddVectoredContinueHandler`; the handle removes it.
pub fn add_vectored_handler(continue_handler: bool, first: bool, handler: usize) -> usize {
    let handle = NEXT_HANDLE.fetch_add(0x10, Ordering::Relaxed);
    let mut lists = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
    let list = &mut lists[usize::from(continue_handler)];
    list.insert(if first { 0 } else { list.len() }, (handle, handler));
    handle
}

/// `RemoveVectoredExceptionHandler` / `RemoveVectoredContinueHandler`.
pub fn remove_vectored_handler(continue_handler: bool, handle: usize) -> bool {
    let mut lists = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
    let list = &mut lists[usize::from(continue_handler)];
    let before = list.len();
    list.retain(|&(h, _)| h != handle);
    list.len() != before
}

/// Calls the handlers of one list until one continues execution. Copied
/// out first: a handler may add or remove handlers, and nothing here may
/// own memory (a handler can leave by unwinding past these frames).
fn call_vectored(continue_handler: bool, record: &mut ExceptionRecord, context: &mut Context) -> bool {
    let mut handlers = [0usize; MAX_VECTORED];
    let n = {
        let lists = VECTORED.lock().unwrap_or_else(|e| e.into_inner());
        let list = &lists[usize::from(continue_handler)];
        for (slot, &(_, h)) in handlers.iter_mut().zip(list) {
            *slot = h;
        }
        list.len().min(MAX_VECTORED)
    };
    let mut info = ExceptionPointers { record, context };
    let resume = *context;
    handlers[..n]
        .iter()
        .any(|&h| call_filter(&resume, h, &mut info) == EXCEPTION_CONTINUE_EXECUTION)
}

/// `SetUnhandledExceptionFilter`: the previous filter.
pub fn set_top_level_filter(filter: usize) -> usize {
    TOP_FILTER.swap(filter, Ordering::AcqRel)
}

/// `UnhandledExceptionFilter`: the title's filter if one is set, else the
/// crash report and `EXCEPTION_EXECUTE_HANDLER`.
///
/// # Safety
/// `info` points at a record and a context, as for any filter.
pub unsafe fn unhandled_exception_filter(info: *mut ExceptionPointers) -> i32 {
    let filter = TOP_FILTER.load(Ordering::Acquire);
    if filter != 0 {
        type Filter = unsafe extern "win64" fn(*mut ExceptionPointers) -> i32;
        // SAFETY: the title's filter, with the pointers we were given.
        let answer = unsafe { std::mem::transmute::<usize, Filter>(filter)(info) };
        if answer != EXCEPTION_CONTINUE_SEARCH {
            return answer;
        }
    }
    // SAFETY: as the caller says.
    unsafe { report_unhandled(info) };
    EXCEPTION_EXECUTE_HANDLER
}

thread_local! {
    /// Set once this thread's unhandled exception is reported, so the end
    /// of the dispatch does not report it twice.
    static REPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "diagnostics")]
static SAVE: std::sync::OnceLock<Vec<(usize, usize, std::path::PathBuf)>> = std::sync::OnceLock::new();

/// Debugging aid: on a reported crash, also write each `[address, address
/// + len)` to its file.
#[cfg(feature = "diagnostics")]
pub fn save_on_crash(ranges: Vec<(usize, usize, std::path::PathBuf)>) {
    let _ = SAVE.set(ranges);
}

/// Reports an exception nobody handles, as the Windows crash reporter
/// does (`module+rva`, registers, the stack), to stderr and the crash log.
///
/// # Safety
/// `info` points at a record and a context.
pub unsafe fn report_unhandled(info: *const ExceptionPointers) {
    REPORTED.set(true);
    // SAFETY: as the caller says.
    let (record, context) = unsafe { (&*(*info).record, &*(*info).context) };
    let mut report = crate::crash_log::Report::default();
    report.header("unhandled exception", record.code, locate(record.address), record.parameters());
    report.registers(&context.gpr);
    let mut frames = [0u64; crate::crash_log::MAX_FRAMES];
    let n = unwind::walk(context, &mut frames);
    for (i, &pc) in frames[..n].iter().enumerate() {
        report.line(format!("  #{i:<2} {}", locate(pc as usize)));
    }
    report.finish(true);
    #[cfg(feature = "diagnostics")]
    if super::stacks::ON_CRASH.load(Ordering::Relaxed) {
        super::stacks::dump_stacks();
    }
    #[cfg(feature = "diagnostics")]
    for (address, len, file) in SAVE.get().into_iter().flatten() {
        // SAFETY: the caller of `save_on_crash` named readable memory.
        let bytes = unsafe { std::slice::from_raw_parts(*address as *const u8, *len) };
        let saved = std::fs::write(file, bytes);
        eprintln!("  saved {address:#x}+{len:#x} to {} ({saved:?})", file.display());
    }
}

// ---- the dispatcher -------------------------------------------------------------------------

/// First chance for (record, context): vectored handlers, frame handlers,
/// then the top-level filter and the end of the process. Returns only by
/// loading a context.
pub(crate) fn dispatch(record: &mut ExceptionRecord, context: &mut Context) -> ! {
    if tracing::enabled!(target: "victory_loader::seh", tracing::Level::DEBUG) {
        let mut frames = [0u64; 12];
        let n = unwind::walk(context, &mut frames);
        let frames: Vec<String> = frames[..n].iter().map(|&pc| locate(pc as usize)).collect();
        tracing::debug!(
            target: "victory_loader::seh",
            code = format_args!("{:#x}", record.code),
            at = %locate(record.address),
            parameters = format_args!("{:x?}", record.parameters()),
            ?frames,
            "first chance"
        );
    }
    if call_vectored(false, record, context) || call_frame_handlers(record, context) {
        call_vectored(true, record, context);
        restore_context(context, None);
    }
    unhandled(record, context)
}

/// Nobody took it: the top-level filter has the last word, then the crash
/// report ends the process with the exception code.
fn unhandled(record: &mut ExceptionRecord, context: &mut Context) -> ! {
    let mut info = ExceptionPointers { record, context };
    let filter = TOP_FILTER.load(Ordering::Acquire);
    let resume = *context;
    if filter != 0 && call_filter(&resume, filter, &mut info) == EXCEPTION_CONTINUE_EXECUTION {
        restore_context(context, None);
    }
    terminate(record, context)
}

/// Ends the process with `record`'s code after reporting it (unless a
/// filter already did); no handler is called.
pub fn terminate(record: &mut ExceptionRecord, context: &mut Context) -> ! {
    if !REPORTED.get() {
        let info = ExceptionPointers { record, context };
        // SAFETY: our own record and context.
        unsafe { report_unhandled(&info) };
    }
    // SAFETY: ends the process at once, as an unhandled exception does on Windows.
    unsafe { libc::_exit(record.code as i32) }
}

/// A status the dispatcher raises about `record` (a noncontinuable
/// exception continued, a handler's nonsense answer). The second one in a
/// row for the same reason ends the process instead of recursing.
fn raise_status(code: u32, record: &mut ExceptionRecord, context: &mut Context) -> ! {
    let mut status = ExceptionRecord::new(code, EXCEPTION_NONCONTINUABLE, context.rip as usize);
    let repeated = record.code == code && !record.record.is_null();
    status.record = record;
    if repeated {
        terminate(&mut status, context);
    }
    dispatch(&mut status, context)
}

/// [`unwind::virtual_unwind`] for the dispatcher's walks: `None` when the
/// stack is not readable (the title destroyed its frame).
fn unwind_frame(
    kind: u32,
    base: usize,
    pc: u64,
    f: *mut RuntimeFunction,
    c: &mut Context,
) -> Option<unwind::Unwound> {
    unwind::virtual_unwind(kind, base, pc, f, c, None).ok()
}

/// The most frames a dispatcher walk visits: a corrupt stack ends it.
const MAX_WALK: usize = 1 << 16;

/// Phase one: every frame from the exception outwards is asked, through
/// the language handler its unwind data names. True when a handler said
/// to continue execution.
fn call_frame_handlers(record: &mut ExceptionRecord, context: &mut Context) -> bool {
    let mut c = *context;
    let mut d = DispatcherContext::zeroed();
    let stack = Stack::current();
    let mut flags = record.flags & EXCEPTION_NONCONTINUABLE;
    let mut nested_frame = 0u64;
    for _ in 0..MAX_WALK {
        if !stack.contains(c.rsp()) {
            break;
        }
        let rsp = c.rsp();
        let (mut pc, mut base, mut function) = match unwind::lookup(c.rip) {
            Entry::Boundary => break,
            Entry::Leaf(_) => {
                // A leaf, or nothing at all: a call through a bad pointer is its caller's fault.
                match asm::read64(rsp) {
                    Some(rip) if rip != 0 => {
                        c.rip = rip;
                        c.gpr[RSP] += 8;
                        continue;
                    }
                    _ => break,
                }
            }
            Entry::Function(f, base) => (c.rip, base, f),
        };
        let Some(mut u) = unwind_frame(UNW_FLAG_EHANDLER, base, pc, function, &mut c)
            .filter(|u| stack.contains(u.establisher_frame) && u.establisher_frame & 7 == 0)
            .filter(|_| c.rsp() > rsp)
        else {
            flags |= EXCEPTION_STACK_INVALID;
            break;
        };
        if u.handler == 0 {
            continue;
        }
        let mut scope = 0;
        loop {
            record.flags = flags;
            d.control_pc = pc;
            d.image_base = base as u64;
            d.function_entry = function;
            d.establisher_frame = u.establisher_frame;
            d.target_ip = 0;
            d.context_record = &mut c;
            // SAFETY: a handler address from the unwind data; never zero here.
            d.language_handler = Some(unsafe { std::mem::transmute::<usize, LanguageHandler>(u.handler) });
            d.handler_data = u.handler_data as *mut _;
            d.history_table = std::ptr::null_mut();
            d.scope_index = scope;
            let at: *mut Context = context;
            let answer = call_handler(at, EXCEPTION, u.handler, record, u.establisher_frame, at, &mut d);
            flags |= record.flags & EXCEPTION_NONCONTINUABLE;
            if nested_frame == u.establisher_frame {
                // The end of the frames a nested exception passes a second time.
                flags &= !EXCEPTION_NESTED_CALL;
                nested_frame = 0;
            }
            match answer {
                DISPOSITION_CONTINUE_EXECUTION => {
                    if flags & EXCEPTION_NONCONTINUABLE != 0 {
                        raise_status(STATUS_NONCONTINUABLE_EXCEPTION, record, context);
                    }
                    return true;
                }
                DISPOSITION_CONTINUE_SEARCH => {}
                DISPOSITION_NESTED_EXCEPTION => {
                    flags |= EXCEPTION_NESTED_CALL;
                    nested_frame = nested_frame.max(d.establisher_frame);
                }
                DISPOSITION_COLLIDED_UNWIND => {
                    // The frame the interrupted unwind was at is asked once more, from the scope
                    // it had reached, and the walk goes on from that frame's caller.
                    pc = d.control_pc;
                    base = d.image_base as usize;
                    function = d.function_entry;
                    // SAFETY: the interrupted unwind's context, still on the stack below us.
                    c = unsafe { *d.context_record };
                    let _ = unwind_frame(UNW_FLAG_NHANDLER, base, pc, function, &mut c);
                    u.establisher_frame = d.establisher_frame;
                    u.handler = d.language_handler.map_or(0, |h| h as usize);
                    u.handler_data = d.handler_data as usize;
                    scope = d.scope_index;
                    continue;
                }
                _ => raise_status(STATUS_INVALID_DISPOSITION, record, context),
            }
            break;
        }
    }
    record.flags = flags;
    false
}

/// `RtlUnwindEx`: phase two. Calls the unwind handler of every frame from
/// `caller`'s up to `target_frame`, then continues at `target_ip` in that
/// frame with rax = `return_value`. `caller` is the context `RtlUnwindEx`
/// was called in; `original` the caller's buffer for the frames' contexts.
pub(crate) fn unwind(
    target_frame: u64,
    target_ip: u64,
    record: Option<&mut ExceptionRecord>,
    return_value: u64,
    original: Option<&mut Context>,
    history: usize,
    caller: &Context,
) -> ! {
    let mut own_record = ExceptionRecord::new(STATUS_UNWIND, 0, caller.rip as usize);
    let record = record.unwrap_or(&mut own_record);
    let mut own_original = Context::zeroed();
    let original: *mut Context = original.unwrap_or(&mut own_original);
    let mut scratch = Context::zeroed();
    // Two contexts leapfrog: `current` is the frame being unwound, `previous` gets its caller's.
    let mut current: *mut Context = original;
    let mut previous: *mut Context = &mut scratch;
    // SAFETY: `current` and `previous` point at the two buffers above (or the caller's) for
    // the whole walk; each step reads one and writes the other.
    let cur = |p: *mut Context| unsafe { &mut *p };
    *cur(current) = *caller;
    let mut d = DispatcherContext::zeroed();
    let stack = Stack::current();
    let mut flags = EXCEPTION_UNWINDING | if target_frame == 0 { EXCEPTION_EXIT_UNWIND } else { 0 };
    let mut frame = caller.rsp();
    let mut first = true;
    for _ in 0..MAX_WALK {
        if !first && !(stack.contains(frame) && frame != target_frame) {
            break;
        }
        first = false;
        let now = cur(current);
        let (mut pc, mut base, mut function) = match unwind::lookup(now.rip) {
            Entry::Boundary => break,
            Entry::Leaf(_) => {
                if !stack.contains(now.rsp()) {
                    break;
                }
                match asm::read64(now.rsp()) {
                    Some(rip) if rip != 0 => {
                        now.rip = rip;
                        now.gpr[RSP] += 8;
                        continue;
                    }
                    _ => break,
                }
            }
            Entry::Function(f, base) => (now.rip, base, f),
        };
        *cur(previous) = *now;
        let unwound = unwind_frame(UNW_FLAG_UHANDLER, base, pc, function, cur(previous));
        let Some(mut u) = unwound.filter(|u| {
            let f = u.establisher_frame;
            stack.contains(f) && f & 7 == 0 && (target_frame == 0 || f <= target_frame)
        }) else {
            record.flags = flags;
            raise_status(STATUS_BAD_STACK, record, cur(current));
        };
        frame = u.establisher_frame;
        if u.handler == 0 {
            if frame != target_frame {
                std::mem::swap(&mut current, &mut previous);
            }
            continue;
        }
        d.target_ip = target_ip;
        let mut scope = 0;
        loop {
            if frame == target_frame {
                flags |= EXCEPTION_TARGET_UNWIND;
            }
            record.flags = flags;
            let now = cur(current);
            now.gpr[RAX] = return_value; // a handler may continue with this context
            d.control_pc = pc;
            d.image_base = base as u64;
            d.function_entry = function;
            d.establisher_frame = frame;
            d.context_record = current;
            // SAFETY: a handler address from the unwind data; never zero here.
            d.language_handler = Some(unsafe { std::mem::transmute::<usize, LanguageHandler>(u.handler) });
            d.handler_data = u.handler_data as *mut _;
            d.history_table = history as *mut _;
            d.scope_index = scope;
            let answer = call_handler(caller, UNWIND, u.handler, record, frame, current, &mut d);
            flags &= !(EXCEPTION_COLLIDED_UNWIND | EXCEPTION_TARGET_UNWIND);
            match answer {
                DISPOSITION_CONTINUE_SEARCH => {
                    if frame != target_frame {
                        std::mem::swap(&mut current, &mut previous);
                    }
                }
                DISPOSITION_COLLIDED_UNWIND => {
                    // This unwind ran into an earlier one that was inside a handler: take over
                    // where that one was, its frame, its handler and the scope it had reached.
                    pc = d.control_pc;
                    base = d.image_base as usize;
                    function = d.function_entry;
                    // SAFETY: the interrupted unwind's context.
                    *cur(original) = unsafe { *d.context_record };
                    current = original;
                    previous = &mut scratch;
                    *cur(previous) = *cur(current);
                    let _ = unwind_frame(UNW_FLAG_NHANDLER, base, pc, function, cur(previous));
                    frame = d.establisher_frame;
                    u.handler = d.language_handler.map_or(0, |h| h as usize);
                    u.handler_data = d.handler_data as usize;
                    scope = d.scope_index;
                    flags |= EXCEPTION_COLLIDED_UNWIND;
                    continue;
                }
                _ => {
                    record.flags = flags;
                    raise_status(STATUS_INVALID_DISPOSITION, record, cur(current));
                }
            }
            break;
        }
    }
    let now = cur(current);
    if target_frame != 0 && frame == target_frame {
        now.gpr[RAX] = return_value;
        if record.code != STATUS_UNWIND_CONSOLIDATE {
            now.rip = target_ip;
        }
        restore_context(now, Some(record));
    }
    // An exit unwind (no target) has run every handler; a target no frame has is a corrupt
    // stack. Windows raises the record a second time here, which ends the process.
    record.flags = flags;
    terminate(record, now)
}

/// `RtlRestoreContext`, `STATUS_LONGJUMP` and `STATUS_UNWIND_CONSOLIDATE`
/// included.
pub(crate) fn restore_context(context: &mut Context, record: Option<&mut ExceptionRecord>) -> ! {
    match record {
        Some(r) if r.code == STATUS_LONGJUMP && r.number_parameters >= 1 => {
            /// `_JUMP_BUFFER` (setjmp.h, x64).
            #[repr(C)]
            struct JumpBuffer {
                frame: u64,
                rbx: u64,
                rsp: u64,
                rbp: u64,
                rsi: u64,
                rdi: u64,
                r12_15: [u64; 4],
                rip: u64,
                mx_csr: u32,
                fp_csr: u16,
                spare: u16,
                xmm6_15: [M128A; 10],
            }
            const _: () = assert!(std::mem::size_of::<JumpBuffer>() == 256);
            // SAFETY: longjmp passes its jump buffer as the first parameter.
            let j = unsafe { &*(r.information[0] as *const JumpBuffer) };
            context.gpr[RBX] = j.rbx;
            context.gpr[RSP] = j.rsp;
            context.gpr[RBP] = j.rbp;
            context.gpr[RSI] = j.rsi;
            context.gpr[RDI] = j.rdi;
            context.gpr[R12..=R15].copy_from_slice(&j.r12_15);
            context.rip = j.rip;
            context.mx_csr = j.mx_csr;
            context.flt_save.mx_csr = j.mx_csr;
            context.flt_save.control_word = j.fp_csr;
            context.xmm()[6..].copy_from_slice(&j.xmm6_15);
        }
        Some(r) if r.code == STATUS_UNWIND_CONSOLIDATE && r.number_parameters >= 1 => {
            // The C++ runtime's catch: the callback runs the catch block on top of the frames
            // that were logically unwound and returns where the catching function goes on.
            // While it runs, a walk (a throw inside the catch block) sees `context`, the
            // catching function's frame, as this frame's caller.
            let callback = r.information[0];
            let resume = *context;
            // SAFETY: the consolidation callback the language handler put in the record.
            context.rip = unsafe { asm::frame_call(&resume, PASS, callback, [r as *mut _ as u64, 0, 0, 0]) };
        }
        _ => {}
    }
    if context.context_flags & CONTEXT_FLOATING_POINT == CONTEXT_FLOATING_POINT {
        let mx_csr = context.mx_csr & 0xFFFF; // the reserved bits fault
        // SAFETY: loads a valid MXCSR value.
        unsafe { std::arch::asm!("ldmxcsr [{}]", in(reg) &mx_csr, options(nostack)) };
        // ponytail: only the x87 control word is reloaded, not its register stack (fxrstor of a
        // validated copy if a title resumes x87 code); a zeroed one is a hand-made context.
        let control = context.flt_save.control_word;
        if control != 0 {
            // SAFETY: clears pending x87 exceptions and loads the control word.
            unsafe { std::arch::asm!("fnclex", "fldcw [{}]", in(reg) &control, options(nostack)) };
        }
    }
    // SAFETY: a context of this thread, as every caller has.
    unsafe { asm::restore(context) }
}

/// Defines `$stub`, a title-callable entry that captures its caller's
/// context and calls
/// `$target(a1, a2, a3, a4, caller: *mut Context)` (`extern "win64"`)
/// with the caller's first four arguments untouched. A Rust function
/// cannot do this itself: by the time its prologue has run, the caller's
/// nonvolatile registers are gone. `caller` is the state as if the call
/// had just returned; its 5th and 6th stack arguments are at
/// `caller.rsp() + 0x20` and `+ 0x28`. If `$target` returns, so does the
/// stub, with its rax.
#[macro_export]
macro_rules! capture_stub {
    ($stub:ident => $target:path) => {
        // frame: 0x00 home of the target's arguments, 0x20 its 5th, 0x30 the context
        // (1232 bytes), 0x508 the return address.
        ::core::arch::global_asm!(
            concat!(
                ".text\n.p2align 4\n.globl victory_stub_", stringify!($stub),
                "\n.hidden victory_stub_", stringify!($stub),
                "\nvictory_stub_", stringify!($stub), ":\n",
                "subq $0x508, %rsp\n",
                "movq %rcx, 0x20(%rsp)\n",
                "leaq 0x30(%rsp), %rcx\n",
                "call victory_seh_capture_context\n",
                "movq 0x20(%rsp), %rcx\n",
                "movq %rcx, 0xb0(%rsp)\n",
                "leaq 0x510(%rsp), %rax\n",
                "movq %rax, 0xc8(%rsp)\n",
                "movq 0x508(%rsp), %rax\n",
                "movq %rax, 0x128(%rsp)\n",
                "leaq 0x30(%rsp), %rax\n",
                "movq %rax, 0x20(%rsp)\n",
                "call {target}\n",
                "addq $0x508, %rsp\n",
                "ret\n",
            ),
            target = sym $target,
            options(att_syntax)
        );
        unsafe extern "win64" {
            #[link_name = concat!("victory_stub_", stringify!($stub))]
            pub fn $stub();
        }
    };
}

/// `RaiseException`'s work, for a capture stub's target: dispatches
/// `record` as raised by `caller`. Returns (to `caller`) only if a handler
/// continues execution.
pub fn raise(record: &mut ExceptionRecord, caller: &mut Context) -> ! {
    if record.address == 0 {
        record.address = caller.rip as usize;
    }
    dispatch(record, caller)
}

/// `RtlUnwindEx` for a capture stub's target; `caller` is its caller's
/// context, whose stack holds the 5th and 6th arguments.
///
/// # Safety
/// The arguments are the title's `RtlUnwindEx` call's.
pub unsafe fn unwind_ex(
    target_frame: u64,
    target_ip: u64,
    record: *mut ExceptionRecord,
    return_value: u64,
    caller: &Context,
    with_stack_arguments: bool,
) -> ! {
    let (original, history) = if with_stack_arguments {
        let at = caller.rsp() + 0x20;
        (asm::read64(at).unwrap_or(0), asm::read64(at + 8).unwrap_or(0))
    } else {
        (0, 0)
    };
    // SAFETY: the title's record and context buffer, or null.
    let (record, original) = unsafe { (record.as_mut(), (original as *mut Context).as_mut()) };
    unwind(target_frame, target_ip, record, return_value, original, history as usize, caller)
}

/// `RtlRestoreContext`.
///
/// # Safety
/// `context` is a state of this thread; `record` null or a record.
pub unsafe fn restore(context: *mut Context, record: *mut ExceptionRecord) -> ! {
    // SAFETY: as the caller says.
    unsafe { restore_context(&mut *context, record.as_mut()) }
}

/// `RtlCaptureContext`, callable from title code.
pub fn capture_context_address() -> usize {
    asm::capture_context_address()
}

/// `RtlLookupFunctionEntry`: the entry and its image base (0 if none).
pub fn lookup_function_entry(pc: u64) -> (*mut RuntimeFunction, u64) {
    match unwind::lookup(pc) {
        Entry::Function(f, base) => (f, base as u64),
        Entry::Boundary => (asm::boundary() as *mut _, asm::base() as u64),
        Entry::Leaf(base) => (std::ptr::null_mut(), base as u64),
    }
}

/// `RtlVirtualUnwind`: the handler (or 0); the handler data and the
/// establisher frame through the pointers.
///
/// # Safety
/// The title's arguments: `function` an entry of `base`, `context` valid.
#[allow(clippy::too_many_arguments)] // RtlVirtualUnwind's
pub unsafe fn virtual_unwind(
    handler_type: u32,
    base: u64,
    pc: u64,
    function: *mut RuntimeFunction,
    context: *mut Context,
    handler_data: *mut usize,
    establisher_frame: *mut u64,
    pointers: *mut NonvolatilePointers,
) -> usize {
    // SAFETY: as the caller says.
    let (context, pointers) = unsafe { (&mut *context, pointers.as_mut()) };
    let u = unwind::virtual_unwind(handler_type, base as usize, pc, function, context, pointers)
        .unwrap_or_default();
    // SAFETY: the title's out pointers, or null.
    unsafe {
        if let Some(d) = handler_data.as_mut() {
            *d = u.handler_data;
        }
        if let Some(f) = establisher_frame.as_mut() {
            *f = u.establisher_frame;
        }
    }
    u.handler
}

/// `RtlPcToFileHeader`: the base of the registered image `pc` is in.
pub fn image_base_of(pc: usize) -> usize {
    unwind::image_of(pc).map_or(0, |(base, _)| base)
}

/// `RtlAddFunctionTable`.
pub fn add_function_table(entries: usize, count: usize, base: usize) -> bool {
    unwind::add_table(entries, count, base)
}

/// `RtlInstallFunctionTableCallback`.
pub fn install_function_table_callback(
    id: usize,
    base: usize,
    len: usize,
    callback: usize,
    context: usize,
) -> bool {
    unwind::add_callback(id, base, len, callback, context)
}

/// `RtlDeleteFunctionTable`.
pub fn delete_function_table(id: usize) -> bool {
    unwind::remove_table(id)
}

/// Up to `frames.len()` code addresses from `context` outwards
/// (`RtlCaptureStackBackTrace`).
pub fn walk_stack(context: &Context, frames: &mut [u64]) -> usize {
    unwind::walk(context, frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    const MAGIC: u32 = 0xE0BE_EF01;
    static SEEN: AtomicU32 = AtomicU32::new(0);
    static CONTINUED: AtomicU32 = AtomicU32::new(0);

    extern "win64" fn raise_impl(code: u64, a: u64, _: u64, _: u64, caller: *mut Context) {
        let mut record = ExceptionRecord::new(code as u32, 0, 0);
        record.number_parameters = 1;
        record.information[0] = a as usize;
        // SAFETY: the stub's capture of our caller.
        raise(&mut record, unsafe { &mut *caller })
    }
    crate::capture_stub!(test_raise => raise_impl);

    unsafe extern "win64" fn on_magic(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: the dispatcher's pointers.
        let record = unsafe { &*(*info).record };
        if record.code != MAGIC {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        SEEN.store(record.information[0] as u32, Ordering::SeqCst);
        EXCEPTION_CONTINUE_EXECUTION
    }

    unsafe extern "win64" fn on_continue(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: as above.
        if unsafe { (*(*info).record).code } == MAGIC {
            CONTINUED.fetch_add(1, Ordering::SeqCst);
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    #[test]
    fn raised_exceptions_continue_in_their_caller() {
        let handle = add_vectored_handler(false, true, on_magic as *const () as usize);
        let cont = add_vectored_handler(true, false, on_continue as *const () as usize);
        // SAFETY: the stub takes the win64 arguments raise_impl does.
        let raise: extern "win64" fn(u64, u64) = unsafe { std::mem::transmute(test_raise as *const ()) };
        // Values live across the call: the restored context must give them back.
        let kept = std::hint::black_box([1u64, 2, 3, 4, 5, 6, 7, 8]);
        raise(u64::from(MAGIC), 77);
        assert_eq!(std::hint::black_box(kept), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(SEEN.load(Ordering::SeqCst), 77);
        assert!(CONTINUED.load(Ordering::SeqCst) >= 1);
        raise(u64::from(MAGIC), 78);
        assert_eq!(SEEN.load(Ordering::SeqCst), 78);
        assert!(remove_vectored_handler(false, handle) && !remove_vectored_handler(false, handle));
        assert!(remove_vectored_handler(true, cont));
    }

    /// A page of machine code outside our program: title code, as far as
    /// the signal handler is concerned.
    fn code_page(bytes: &[u8]) -> usize {
        // SAFETY: a fresh anonymous mapping we fill and make executable.
        unsafe {
            let page = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            assert_ne!(page, libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), page as *mut u8, bytes.len());
            assert_eq!(libc::mprotect(page, 4096, libc::PROT_READ | libc::PROT_EXEC), 0);
            page as usize
        }
    }

    static PAGES: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    /// Repairs the faults of the test pages: skips the faulting instruction
    /// (its length in the byte after the page's code, at +0x800 + offset)
    /// and returns the code in rax.
    unsafe extern "win64" fn repair(info: *mut ExceptionPointers) -> i32 {
        // SAFETY: the dispatcher's pointers.
        let (record, context) = unsafe { (&*(*info).record, &mut *(*info).context) };
        let Some(&page) = PAGES.lock().unwrap().iter().find(|&&p| (p..p + 0x800).contains(&record.address))
        else {
            return EXCEPTION_CONTINUE_SEARCH;
        };
        assert_eq!(context.rip as usize, record.address);
        // SAFETY: the length table the test wrote after the code.
        context.rip += u64::from(unsafe { *((page + 0x800 + record.address - page) as *const u8) });
        context.gpr[RAX] = u64::from(record.code);
        if record.code == STATUS_ACCESS_VIOLATION {
            context.gpr[RAX] ^= record.information[1] as u64 ^ (record.information[0] as u64) << 32;
        }
        EXCEPTION_CONTINUE_EXECUTION
    }

    #[test]
    fn faults_in_title_code_reach_vectored_handlers() {
        crate::linux::signals::install();
        let handle = add_vectored_handler(false, false, repair as *const () as usize);
        let mut bytes = vec![0xCC; 0x900];
        let mut code = |at: usize, b: &[u8], len_at: usize, len: u8| {
            bytes[at..at + b.len()].copy_from_slice(b);
            bytes[0x800 + len_at] = len;
        };
        // 0x00: mov eax, [0x10] (a read of an unmapped page); ret
        code(0x00, &[0x8B, 0x04, 0x25, 0x10, 0, 0, 0, 0xC3], 0x00, 7);
        // 0x10: xor ecx, ecx; xor edx, edx; mov eax, 7; div ecx; ret
        code(0x10, &[0x31, 0xC9, 0x31, 0xD2, 0xB8, 7, 0, 0, 0, 0xF7, 0xF1, 0xC3], 0x19, 2);
        // 0x20: int3; ret (reported at the int3, which is one byte)
        code(0x20, &[0xCC, 0xC3], 0x20, 1);
        // 0x30: ud2; ret
        code(0x30, &[0x0F, 0x0B, 0xC3], 0x30, 2);
        // 0x40: mov [0x18], eax (a write); ret
        code(0x40, &[0x89, 0x04, 0x25, 0x18, 0, 0, 0, 0xC3], 0x40, 7);
        let page = code_page(&bytes);
        PAGES.lock().unwrap().push(page);
        let run = |at: usize| {
            // SAFETY: the code above, which returns eax.
            let f: extern "win64" fn() -> u64 = unsafe { std::mem::transmute(page + at) };
            f()
        };
        assert_eq!(run(0x00), u64::from(STATUS_ACCESS_VIOLATION) ^ 0x10);
        assert_eq!(run(0x10), u64::from(STATUS_INTEGER_DIVIDE_BY_ZERO));
        assert_eq!(run(0x20), u64::from(STATUS_BREAKPOINT));
        assert_eq!(run(0x30), u64::from(STATUS_ILLEGAL_INSTRUCTION));
        assert_eq!(run(0x40), u64::from(STATUS_ACCESS_VIOLATION) ^ 0x18 ^ 1 << 32);
        assert!(remove_vectored_handler(false, handle));
    }
}
