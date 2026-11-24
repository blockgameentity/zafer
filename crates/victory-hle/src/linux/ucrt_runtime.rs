//! ucrtbase: start-up and shutdown (`api-ms-win-crt-runtime`): errno, the
//! invalid-parameter path, `_initterm`, the onexit tables, argv and
//! `_beginthreadex`. Also what the other `ucrt_*` slices share.
//!
//! `exit`, `abort`, `terminate` and `_invalid_parameter_noinfo_noreturn` are
//! the portable `kernel` module's, which wins over this one.
//!
//! errno is the UCRT's own per-thread cell with the UCRT's numbers (1..34
//! are Linux's too; above that they differ). A function that fails by its
//! own rule stores one with [`fail`]; parameter validation
//! (`_VALIDATE_RETURN`) goes through [`invalid`], which also calls the
//! title's invalid-parameter handler, and without one ends the run as
//! `__fastfail` does.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::cell::Cell;
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use crate::{HleModule, hle_export};

// errno.h of the Windows SDK.
#[cfg(test)]
pub(crate) const ENOENT: i32 = 2;
pub(crate) const EBADF: i32 = 9;
pub(crate) const EAGAIN: i32 = 11;
pub(crate) const ENOMEM: i32 = 12;
pub(crate) const EACCES: i32 = 13;
#[cfg(test)]
pub(crate) const EEXIST: i32 = 17;
pub(crate) const EINVAL: i32 = 22;
pub(crate) const EMFILE: i32 = 24;
pub(crate) const ERANGE: i32 = 34;
pub(crate) const EILSEQ: i32 = 42;
pub(crate) const STRUNCATE: i32 = 80;

thread_local! {
    static ERRNO: Cell<i32> = const { Cell::new(0) };
}

pub(crate) fn errno() -> i32 {
    ERRNO.get()
}

pub(crate) fn set_errno(value: i32) {
    ERRNO.set(value);
}

/// Stores `code` in errno and returns it.
pub(crate) fn fail(code: i32) -> i32 {
    set_errno(code);
    code
}

/// A Linux errno as the UCRT's number; one it has no name for is EINVAL.
pub(crate) fn errno_from_host(host: i32) -> i32 {
    match host {
        libc::EDEADLK => 36,
        libc::ENAMETOOLONG => 38,
        libc::ENOLCK => 39,
        libc::ENOSYS => 40,
        libc::ENOTEMPTY => 41,
        libc::EILSEQ => EILSEQ,
        libc::EDQUOT => 28,
        libc::ETXTBSY => EACCES,
        libc::ELOOP => 114,
        libc::EOVERFLOW => 132,
        libc::ETIMEDOUT => 138,
        libc::ENOTBLK => EINVAL,
        0..=34 => host,
        _ => EINVAL,
    }
}

/// errno from the host call that just failed; returns -1.
pub(crate) fn fail_from_host() -> i32 {
    let host = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    set_errno(errno_from_host(host));
    -1
}

/// `_set_invalid_parameter_handler`'s handler.
type InvalidParameterHandler = extern "win64" fn(*const u16, *const u16, *const u16, u32, usize);
static INVALID_PARAMETER_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// `_invalid_parameter_noinfo`: the title's handler, or the end of the run.
pub(crate) fn invalid_parameter() {
    let handler = INVALID_PARAMETER_HANDLER.load(Ordering::Acquire);
    if handler != 0 {
        // SAFETY: a handler the title registered with this signature.
        let handler: InvalidParameterHandler = unsafe { std::mem::transmute(handler) };
        let saved = errno();
        handler(std::ptr::null(), std::ptr::null(), std::ptr::null(), 0, 0);
        set_errno(saved);
        return;
    }
    if cfg!(test) {
        return; // unit tests check the errno and the result
    }
    fast_fail("an invalid parameter reached the C runtime and the title set no handler");
}

/// The UCRT's parameter validation: errno `code`, the handler, `code`.
pub(crate) fn invalid(code: i32) -> i32 {
    set_errno(code);
    invalid_parameter();
    code
}

/// Ends the run as `__fastfail` does (STATUS_STACK_BUFFER_OVERRUN).
pub(crate) fn fast_fail(why: &str) -> ! {
    tracing::error!("{why}: the run ends as with __fastfail (0xC0000409)");
    std::process::exit(0xC000_0409_u32 as i32)
}

/// A lock held across calls (`_lock_file`, `_lock_locales`): recursive,
/// as the UCRT's critical sections.
pub(crate) struct RecursiveLock {
    state: Mutex<(u32, u32)>, // owner thread, depth
    free: Condvar,
}

impl RecursiveLock {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new((0, 0)),
            free: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, (u32, u32)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn lock(&self) {
        let me = victory_loader::linux::thread::current_id();
        let mut state = self.state();
        if state.1 > 0 && state.0 == me {
            state.1 += 1;
            return;
        }
        while state.1 > 0 {
            state = self.free.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        *state = (me, 1);
    }

    /// Releases one level; a thread that does not hold it changes nothing.
    pub(crate) fn unlock(&self) {
        let me = victory_loader::linux::thread::current_id();
        let mut state = self.state();
        if state.1 == 0 || state.0 != me {
            return;
        }
        state.1 -= 1;
        if state.1 == 0 {
            state.0 = 0;
            self.free.notify_one();
        }
    }

    pub(crate) fn guard(&self) -> RecursiveGuard<'_> {
        self.lock();
        RecursiveGuard(self)
    }
}

pub(crate) struct RecursiveGuard<'a>(&'a RecursiveLock);

impl Drop for RecursiveGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}

// ---- errno ------------------------------------------------------------------

pub extern "win64" fn _errno() -> *mut i32 {
    ERRNO.with(Cell::as_ptr)
}

pub extern "win64" fn _get_errno(value: *mut i32) -> i32 {
    if value.is_null() {
        return invalid(EINVAL);
    }
    // SAFETY: checked for null; the title's int.
    unsafe { *value = errno() };
    0
}

pub extern "win64" fn _set_errno(value: i32) -> i32 {
    set_errno(value);
    0
}

/// The UCRT's own texts: its numbers are not Linux's above 34.
fn error_text(number: i32) -> &'static str {
    const TEXTS: [&str; 43] = [
        "No error",
        "Operation not permitted",
        "No such file or directory",
        "No such process",
        "Interrupted function call",
        "Input/output error",
        "No such device or address",
        "Arg list too long",
        "Exec format error",
        "Bad file descriptor",
        "No child processes",
        "Resource temporarily unavailable",
        "Not enough space",
        "Permission denied",
        "Bad address",
        "Unknown error",
        "Resource device",
        "File exists",
        "Improper link",
        "No such device",
        "Not a directory",
        "Is a directory",
        "Invalid argument",
        "Too many open files in system",
        "Too many open files",
        "Inappropriate I/O control operation",
        "Unknown error",
        "File too large",
        "No space left on device",
        "Invalid seek",
        "Read-only file system",
        "Too many links",
        "Broken pipe",
        "Domain error",
        "Result too large",
        "Unknown error",
        "Resource deadlock avoided",
        "Unknown error",
        "Filename too long",
        "No locks available",
        "Function not implemented",
        "Directory not empty",
        "Illegal byte sequence",
    ];
    const POSIX: [&str; 41] = [
        "address in use",
        "address not available",
        "address family not supported",
        "connection already in progress",
        "bad message",
        "operation canceled",
        "connection aborted",
        "connection refused",
        "connection reset",
        "destination address required",
        "host unreachable",
        "identifier removed",
        "operation in progress",
        "already connected",
        "too many symbolic link levels",
        "message size",
        "network down",
        "network reset",
        "network unreachable",
        "no buffer space",
        "no message available",
        "no link",
        "no message",
        "no protocol option",
        "no stream resources",
        "not a stream",
        "not connected",
        "state not recoverable",
        "not a socket",
        "not supported",
        "operation not supported",
        "Unknown error",
        "value too large",
        "owner dead",
        "protocol error",
        "protocol not supported",
        "wrong protocol type",
        "stream timeout",
        "timed out",
        "text file busy",
        "operation would block",
    ];
    match number {
        0..=42 => TEXTS[number as usize],
        100..=140 => POSIX[(number - 100) as usize],
        _ => "Unknown error",
    }
}

/// The text is copied to a buffer of the calling thread, as in the UCRT.
pub extern "win64" fn strerror(number: i32) -> *mut c_char {
    thread_local! {
        static BUFFER: Cell<[u8; 94]> = const { Cell::new([0; 94]) };
    }
    let text = error_text(number).as_bytes();
    let mut buffer = [0u8; 94];
    buffer[..text.len()].copy_from_slice(text);
    BUFFER.set(buffer);
    BUFFER.with(|b| b.as_ptr().cast())
}

// ---- the invalid-parameter path ----------------------------------------------

pub extern "win64" fn _invalid_parameter_noinfo() {
    invalid_parameter();
}

pub extern "win64" fn _invoke_watson(
    _expression: *const u16,
    _function: *const u16,
    _file: *const u16,
    _line: u32,
    _reserved: usize,
) {
    fast_fail("_invoke_watson");
}

pub extern "win64" fn _set_invalid_parameter_handler(handler: usize) -> usize {
    INVALID_PARAMETER_HANDLER.swap(handler, Ordering::AcqRel)
}

pub extern "win64" fn _wassert(message: *const u16, file: *const u16, line: u32) {
    // SAFETY: the title's NUL-terminated strings, or null.
    let (message, file) = unsafe { (wide_text(message), wide_text(file)) };
    tracing::error!(%message, %file, line, "title assertion failed");
    std::process::exit(3) // the UCRT's abort()
}

/// A NUL-terminated UTF-16 string for a log line; "?" for null.
///
/// # Safety
/// `s` is null or NUL-terminated.
pub(crate) unsafe fn wide_text(s: *const u16) -> String {
    if s.is_null() {
        return "?".into();
    }
    // SAFETY: NUL-terminated, per the caller.
    String::from_utf16_lossy(unsafe { super::ucrt_string::units(s) })
}

// ---- _initterm, the onexit tables ---------------------------------------------

type Pvfv = extern "win64" fn();
type Pifv = extern "win64" fn() -> i32;

/// An image's C++ initialisers, in link order; null entries are skipped.
pub extern "win64" fn _initterm(first: *const usize, last: *const usize) {
    let mut at = first;
    while at < last {
        // SAFETY: [first, last) is the image's initialiser table.
        let entry = unsafe { at.read() };
        if entry != 0 {
            // SAFETY: a non-null entry is a `void (*)(void)`.
            unsafe { std::mem::transmute::<usize, Pvfv>(entry)() };
        }
        // SAFETY: still inside or one past the table.
        at = unsafe { at.add(1) };
    }
}

/// The C initialisers: the first that fails stops the walk with its code.
pub extern "win64" fn _initterm_e(first: *const usize, last: *const usize) -> i32 {
    let mut at = first;
    while at < last {
        // SAFETY: [first, last) is the image's initialiser table.
        let entry = unsafe { at.read() };
        if entry != 0 {
            // SAFETY: a non-null entry is an `int (*)(void)`.
            let result = unsafe { std::mem::transmute::<usize, Pifv>(entry)() };
            if result != 0 {
                return result;
            }
        }
        // SAFETY: still inside or one past the table.
        at = unsafe { at.add(1) };
    }
    0
}

/// `_onexit_table_t`. The UCRT encodes the pointers; the title only ever
/// compares `first` to the sentinel it stored itself, so here they are
/// plain: a malloc block [first, end) of which [first, last) is filled.
#[repr(C)]
pub struct OnexitTable {
    first: *mut usize,
    last: *mut usize,
    end: *mut usize,
}

/// Registration and execution exclude each other; a handler may register.
static EXIT_LOCK: RecursiveLock = RecursiveLock::new();

pub extern "win64" fn _initialize_onexit_table(table: *mut OnexitTable) -> i32 {
    // SAFETY: the title's table, or null.
    let Some(table) = (unsafe { table.as_mut() }) else {
        return -1;
    };
    if table.first == table.end {
        table.first = std::ptr::null_mut();
        table.last = std::ptr::null_mut();
        table.end = std::ptr::null_mut();
    }
    0
}

pub extern "win64" fn _register_onexit_function(table: *mut OnexitTable, function: usize) -> i32 {
    // SAFETY: the title's table, or null.
    let Some(table) = (unsafe { table.as_mut() }) else {
        return -1;
    };
    let _lock = EXIT_LOCK.guard();
    if table.last == table.end {
        let count = (table.last as usize - table.first as usize) / 8;
        let grown = if count == 0 { 32 } else { count * 2 };
        // SAFETY: `first` is null or a block of ours.
        let fresh = unsafe { libc::realloc(table.first.cast(), grown * 8) }.cast::<usize>();
        if fresh.is_null() {
            return -1;
        }
        table.first = fresh;
        // SAFETY: inside the new block.
        table.last = unsafe { fresh.add(count) };
        // SAFETY: one past the new block.
        table.end = unsafe { fresh.add(grown) };
    }
    // SAFETY: last < end.
    unsafe {
        table.last.write(function);
        table.last = table.last.add(1);
    }
    0
}

/// Newest first. A handler that registers another one changes the table
/// under the walk, which then starts again from the new end. The table is
/// empty and freed afterwards.
pub extern "win64" fn _execute_onexit_table(table: *mut OnexitTable) -> i32 {
    // SAFETY: the title's table, or null.
    let Some(table) = (unsafe { table.as_mut() }) else {
        return -1;
    };
    let _lock = EXIT_LOCK.guard();
    let (mut first, mut last) = (table.first, table.last);
    if first.is_null() || first as isize == -1 {
        return 0;
    }
    let mut seen = (first, last);
    loop {
        // SAFETY: [first, last) is the filled part of the table.
        while last > first && unsafe { last.sub(1).read() } == 0 {
            // SAFETY: still inside the table.
            last = unsafe { last.sub(1) };
        }
        if last == first {
            break;
        }
        // SAFETY: last > first, inside the table.
        let function = unsafe {
            last = last.sub(1);
            let f = last.read();
            last.write(0);
            f
        };
        // SAFETY: a registered `int (*)(void)`.
        unsafe { std::mem::transmute::<usize, Pifv>(function)() };
        if (table.first, table.last) != seen {
            // the handler registered another one, or ran the table itself
            seen = (table.first, table.last);
            (first, last) = seen;
            if first.is_null() {
                return 0;
            }
        }
    }
    // SAFETY: our block.
    unsafe { libc::free(first.cast()) };
    table.first = std::ptr::null_mut();
    table.last = std::ptr::null_mut();
    table.end = std::ptr::null_mut();
    0
}

/// The process's atexit table (an EXE built /MD registers here).
static ATEXIT: Mutex<Vec<usize>> = Mutex::new(Vec::new());

pub extern "win64" fn _crt_atexit(function: usize) -> i32 {
    ATEXIT.lock().unwrap_or_else(|e| e.into_inner()).push(function);
    0
}

/// ponytail: accepted, never run; nothing serves `quick_exit` yet.
pub extern "win64" fn _crt_at_quick_exit(_function: usize) -> i32 {
    0
}

/// The EXE's `thread_local` destructors, run for the thread that exits.
static THREAD_LOCAL_EXIT: AtomicUsize = AtomicUsize::new(0);

pub extern "win64" fn _register_thread_local_exe_atexit_callback(callback: usize) {
    THREAD_LOCAL_EXIT.store(callback, Ordering::Release);
}

/// What `exit` owes the title before the process ends, once: the EXE's
/// thread_local destructors, the atexit table newest first, then every
/// stream's buffer. `_cexit` and the `kernel` module's `exit` run it.
pub(crate) fn shutdown() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let callback = THREAD_LOCAL_EXIT.load(Ordering::Acquire);
    if callback != 0 {
        type TlsCallback = extern "win64" fn(*mut c_void, u32, *mut c_void);
        // SAFETY: a PIMAGE_TLS_CALLBACK the title registered.
        let callback: TlsCallback = unsafe { std::mem::transmute(callback) };
        callback(std::ptr::null_mut(), 0, std::ptr::null_mut()); // DLL_PROCESS_DETACH
    }
    loop {
        let next = ATEXIT.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let Some(function) = next else { break };
        // SAFETY: a registered `void (*)(void)`, called without our lock.
        unsafe { std::mem::transmute::<usize, Pvfv>(function)() };
    }
    super::ucrt_stdio::flush_all();
}

pub extern "win64" fn _cexit() {
    shutdown();
}

pub extern "win64" fn _c_exit() {}

/// Ends the process now: no handlers, no flush.
pub extern "win64" fn _exit(code: i32) -> ! {
    tracing::warn!(code, "title calls _exit");
    std::process::exit(code)
}

/// `_seh_filter_exe`/`_seh_filter_dll`: an exception is a C signal only
/// when `signal` set a handler for it.
/// ponytail: nothing serves `signal`, so the search always goes on.
pub extern "win64" fn _seh_filter_exe(_code: u32, _pointers: *mut c_void) -> i32 {
    0 // EXCEPTION_CONTINUE_SEARCH
}

pub extern "win64" fn _seh_filter_dll(_code: u32, _pointers: *mut c_void) -> i32 {
    0
}

// ---- settings the start-up stores ------------------------------------------

static APP_TYPE: AtomicI32 = AtomicI32::new(0);

pub extern "win64" fn _set_app_type(kind: i32) {
    APP_TYPE.store(kind, Ordering::Relaxed);
}

pub extern "win64" fn _set_new_handler(handler: usize) -> usize {
    super::ucrt_heap::NEW_HANDLER.swap(handler, Ordering::AcqRel)
}

// ---- argv and the environment -----------------------------------------------

// The host's own command line, as the host UCRT parses victory.exe's on
// Windows: arguments as given, narrow text UTF-8.
static ARGC: AtomicI32 = AtomicI32::new(0);
static ARGV: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());
static WARGV: AtomicPtr<*mut u16> = AtomicPtr::new(std::ptr::null_mut());

fn leak_narrow(text: &str) -> *mut c_char {
    CString::new(text.replace('\0', ""))
        .unwrap_or_default()
        .into_raw()
}

fn leak_wide(text: &str) -> *mut u16 {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    Box::leak(units.into_boxed_slice()).as_mut_ptr()
}

/// Builds the argument vector once; the title may rewrite the pointers.
fn configure_argv(mode: i32, wide: bool) -> i32 {
    match mode {
        0 => return 0, // _crt_argv_no_arguments
        1 | 2 => {}
        _ => return invalid(EINVAL),
    }
    let args: Vec<String> = std::env::args().collect();
    ARGC.store(args.len() as i32, Ordering::Release);
    if wide && WARGV.load(Ordering::Acquire).is_null() {
        let mut v: Vec<*mut u16> = args.iter().map(|a| leak_wide(a)).collect();
        v.push(std::ptr::null_mut());
        WARGV.store(Box::leak(v.into_boxed_slice()).as_mut_ptr(), Ordering::Release);
    }
    if !wide && ARGV.load(Ordering::Acquire).is_null() {
        let mut v: Vec<*mut c_char> = args.iter().map(|a| leak_narrow(a)).collect();
        v.push(std::ptr::null_mut());
        ARGV.store(Box::leak(v.into_boxed_slice()).as_mut_ptr(), Ordering::Release);
    }
    0
}

pub extern "win64" fn _configure_narrow_argv(mode: i32) -> i32 {
    configure_argv(mode, false)
}

pub extern "win64" fn _configure_wide_argv(mode: i32) -> i32 {
    configure_argv(mode, true)
}

pub extern "win64" fn __p___argc() -> *mut i32 {
    ARGC.as_ptr()
}

pub extern "win64" fn __p___argv() -> *mut *mut *mut c_char {
    ARGV.as_ptr()
}

pub extern "win64" fn __p___wargv() -> *mut *mut *mut u16 {
    WARGV.as_ptr()
}

/// The command line after the program name, as `WinMain` gets it.
pub extern "win64" fn _get_narrow_winmain_command_line() -> *mut c_char {
    static LINE: OnceLock<usize> = OnceLock::new();
    *LINE.get_or_init(|| {
        let line: Vec<String> = std::env::args()
            .skip(1)
            .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a })
            .collect();
        leak_narrow(&line.join(" ")) as usize
    }) as *mut c_char
}

pub extern "win64" fn _initialize_narrow_environment() -> i32 {
    0
}

unsafe extern "C" {
    static environ: *mut *mut c_char;
}

/// The host's environment, as it is (UTF-8).
pub extern "win64" fn _get_initial_narrow_environment() -> *mut *mut c_char {
    // SAFETY: glibc's environ, read once.
    unsafe { environ }
}

// ---- threads ----------------------------------------------------------------

type StartRoutine = unsafe extern "win64" fn(*mut c_void) -> u32;

/// A thread `_beginthreadex` started without the kernel32 slice.
struct Thread;
impl super::handles::Object for Thread {}

/// `CreateThread` with the UCRT's error report. Goes through the kernel32
/// slice's `CreateThread` when there is one, so the handle is waitable;
/// else a guest thread with a handle that only closes (flags ignored).
pub extern "win64" fn _beginthreadex(
    security: *mut c_void,
    stack_size: u32,
    start: Option<StartRoutine>,
    argument: *mut c_void,
    flags: u32,
    thread_id: *mut u32,
) -> usize {
    let Some(start) = start else {
        invalid(EINVAL);
        return 0;
    };
    let mut id = 0u32;
    let handle = match super::lookup("kernel32.dll", "CreateThread") {
        Some(address) => {
            type CreateThread = extern "win64" fn(
                *mut c_void,
                usize,
                StartRoutine,
                *mut c_void,
                u32,
                *mut u32,
            ) -> usize;
            // SAFETY: the kernel32 slice's CreateThread has this signature.
            let create: CreateThread = unsafe { std::mem::transmute(address) };
            create(
                security,
                stack_size as usize,
                start,
                argument,
                flags,
                &mut id,
            )
        }
        None => {
            match victory_loader::linux::thread::spawn(
                start,
                argument,
                stack_size as usize,
                Box::new(|_| {}),
            ) {
                Ok(spawned) => {
                    id = spawned.id;
                    super::handles::insert(Arc::new(Thread))
                }
                Err(_) => 0,
            }
        }
    };
    if handle == 0 {
        fail(EAGAIN);
        return 0;
    }
    if !thread_id.is_null() {
        // SAFETY: checked for null; the title's DWORD.
        unsafe { *thread_id = id };
    }
    handle
}

pub extern "win64" fn _endthreadex(code: u32) {
    match super::lookup("kernel32.dll", "ExitThread") {
        Some(address) => {
            // SAFETY: the kernel32 slice's ExitThread has this signature.
            let exit: extern "win64" fn(u32) -> ! = unsafe { std::mem::transmute(address) };
            exit(code)
        }
        None => tracing::error!(code, "_endthreadex without ExitThread: the thread goes on"),
    }
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-runtime",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-runtime-*"],
        exports: vec![
            hle_export!(_errno),
            hle_export!(_get_errno),
            hle_export!(_set_errno),
            hle_export!(strerror),
            hle_export!(_invalid_parameter_noinfo),
            hle_export!(_invoke_watson),
            hle_export!(_set_invalid_parameter_handler),
            hle_export!(_wassert),
            hle_export!(_initterm),
            hle_export!(_initterm_e),
            hle_export!(_initialize_onexit_table),
            hle_export!(_register_onexit_function),
            hle_export!(_execute_onexit_table),
            hle_export!(_crt_atexit),
            hle_export!(_crt_at_quick_exit),
            hle_export!(_register_thread_local_exe_atexit_callback),
            hle_export!(_cexit),
            hle_export!(_c_exit),
            hle_export!(_exit),
            hle_export!(_seh_filter_exe),
            hle_export!(_seh_filter_dll),
            hle_export!(_set_app_type),
            hle_export!(_set_new_handler),
            hle_export!(_configure_narrow_argv),
            hle_export!(_configure_wide_argv),
            hle_export!(__p___argc),
            hle_export!(__p___argv),
            hle_export!(__p___wargv),
            hle_export!(_get_narrow_winmain_command_line),
            hle_export!(_initialize_narrow_environment),
            hle_export!(_get_initial_narrow_environment),
            hle_export!(_beginthreadex),
            hle_export!(_endthreadex),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn errno_is_per_thread_and_mapped() {
        // SAFETY: our own thread's cell.
        unsafe { *_errno() = 5 };
        assert_eq!(errno(), 5);
        std::thread::spawn(|| assert_eq!(errno(), 0)).join().unwrap();
        assert_eq!(errno_from_host(libc::EILSEQ), 42);
        assert_eq!(errno_from_host(libc::ENAMETOOLONG), 38);
        assert_eq!(errno_from_host(libc::ENOENT), 2);
        assert_eq!(errno_from_host(libc::EPROTO), EINVAL);
        let mut out = 0;
        assert_eq!(_get_errno(&mut out), 0);
        assert_eq!(out, 5);
        assert_eq!(_get_errno(std::ptr::null_mut()), EINVAL);
    }

    #[test]
    fn strerror_has_the_ucrts_texts() {
        // SAFETY: a NUL-terminated thread buffer.
        let text = |n| unsafe { std::ffi::CStr::from_ptr(strerror(n)) }.to_str().unwrap().to_owned();
        assert_eq!(text(2), "No such file or directory");
        assert_eq!(text(42), "Illegal byte sequence");
        assert_eq!(text(138), "timed out");
        assert_eq!(text(77), "Unknown error");
    }

    static ORDER: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    static TABLE: Mutex<usize> = Mutex::new(0);
    extern "win64" fn first() -> i32 {
        ORDER.lock().unwrap().push(1);
        0
    }
    extern "win64" fn second() -> i32 {
        ORDER.lock().unwrap().push(2);
        // a late registration runs next
        let table = *TABLE.lock().unwrap() as *mut OnexitTable;
        _register_onexit_function(table, third as *const () as usize);
        0
    }
    extern "win64" fn third() -> i32 {
        ORDER.lock().unwrap().push(3);
        0
    }

    #[test]
    fn onexit_tables_run_newest_first_and_take_late_entries() {
        let mut table = OnexitTable {
            first: std::ptr::null_mut(),
            last: std::ptr::null_mut(),
            end: std::ptr::null_mut(),
        };
        *TABLE.lock().unwrap() = &mut table as *mut _ as usize;
        assert_eq!(_initialize_onexit_table(&mut table), 0);
        for _ in 0..40 {
            assert_eq!(_register_onexit_function(&mut table, 0), 0); // grows past 32
        }
        assert_eq!(_register_onexit_function(&mut table, first as *const () as usize), 0);
        assert_eq!(_register_onexit_function(&mut table, second as *const () as usize), 0);
        assert_eq!(_execute_onexit_table(&mut table), 0);
        assert_eq!(*ORDER.lock().unwrap(), [2, 3, 1]);
        assert!(table.first.is_null());
        assert_eq!(_execute_onexit_table(&mut table), 0);
        assert_eq!(_initialize_onexit_table(std::ptr::null_mut()), -1);
    }

    static INIT: AtomicU32 = AtomicU32::new(0);
    extern "win64" fn init_a() {
        INIT.fetch_add(1, Ordering::Relaxed);
    }
    extern "win64" fn init_ok() -> i32 {
        INIT.fetch_add(10, Ordering::Relaxed);
        0
    }
    extern "win64" fn init_fails() -> i32 {
        7
    }

    #[test]
    fn initterm_skips_nulls_and_stops_at_a_failure() {
        let table = [init_a as *const () as usize, 0, init_a as *const () as usize];
        _initterm(table.as_ptr(), table.as_ptr_range().end);
        assert_eq!(INIT.load(Ordering::Relaxed), 2);
        let table = [init_ok as *const () as usize, 0, init_fails as *const () as usize, init_ok as *const () as usize];
        assert_eq!(_initterm_e(table.as_ptr(), table.as_ptr_range().end), 7);
        assert_eq!(INIT.load(Ordering::Relaxed), 12);
    }

    static HANDLED: AtomicU32 = AtomicU32::new(0);
    extern "win64" fn handler(_: *const u16, _: *const u16, _: *const u16, _: u32, _: usize) {
        HANDLED.fetch_add(1, Ordering::Relaxed);
        set_errno(0); // the caller's errno survives the handler
    }

    #[test]
    fn invalid_parameters_reach_the_handler_and_keep_errno() {
        let old = _set_invalid_parameter_handler(handler as *const () as usize);
        assert_eq!(invalid(ERANGE), ERANGE);
        assert_eq!(errno(), ERANGE);
        assert!(HANDLED.load(Ordering::Relaxed) >= 1);
        _set_invalid_parameter_handler(old);
    }

    #[test]
    fn argv_is_the_hosts_and_null_terminated() {
        assert_eq!(_configure_narrow_argv(1), 0);
        assert_eq!(_configure_wide_argv(2), 0);
        // SAFETY: configured above.
        unsafe {
            let argc = *__p___argc();
            assert_eq!(argc as usize, std::env::args().count());
            let argv = *__p___argv();
            assert!((*argv.add(argc as usize)).is_null());
            assert!(!(*argv).is_null());
            let wargv = *__p___wargv();
            assert_eq!(*(*wargv), std::env::args().next().unwrap().encode_utf16().next().unwrap());
        }
        assert_eq!(_configure_narrow_argv(9), EINVAL);
    }

    static RAN: AtomicU32 = AtomicU32::new(0);
    unsafe extern "win64" fn thread_main(arg: *mut c_void) -> u32 {
        RAN.store(arg as u32, Ordering::Release);
        0
    }

    #[test]
    fn beginthreadex_runs_the_routine() {
        let mut id = 0;
        let h = _beginthreadex(
            std::ptr::null_mut(),
            0,
            Some(thread_main),
            41 as *mut c_void,
            0,
            &mut id,
        );
        assert_ne!(h, 0);
        assert_ne!(id, 0);
        while RAN.load(Ordering::Acquire) != 41 {
            std::thread::yield_now();
        }
        assert_eq!(_beginthreadex(std::ptr::null_mut(), 0, None, std::ptr::null_mut(), 0, &mut id), 0);
    }

    #[test]
    fn recursive_lock_nests_and_excludes() {
        static LOCK: RecursiveLock = RecursiveLock::new();
        static INSIDE: AtomicU32 = AtomicU32::new(0);
        LOCK.lock();
        LOCK.lock();
        let t = std::thread::spawn(|| {
            let _g = LOCK.guard();
            INSIDE.store(1, Ordering::Release);
        });
        LOCK.unlock();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(INSIDE.load(Ordering::Acquire), 0);
        LOCK.unlock();
        t.join().unwrap();
        assert_eq!(INSIDE.load(Ordering::Acquire), 1);
    }
}
