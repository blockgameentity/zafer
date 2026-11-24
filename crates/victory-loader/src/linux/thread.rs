//! Guest threads: host threads that run title code, each with a TEB at
//! its GS base, a stack with a guard page, and the attached images' TLS
//! callbacks and `DllMain` on start and exit.
//!
//! The thread's title code runs inside a guest frame (`victory_guest_call`
//! below): `ExitThread` ([`exit_current`]) resumes there, abandoning the
//! title's frames without unwinding them, as Windows does.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::image::LoadedImage;
use super::teb::{self, Teb};
use super::tls;
use crate::DLL_PROCESS_DETACH;

const DLL_THREAD_ATTACH: u32 = 2;
const DLL_THREAD_DETACH: u32 = 3;

/// A title thread's start routine (`LPTHREAD_START_ROUTINE`).
pub type StartRoutine = unsafe extern "win64" fn(*mut c_void) -> u32;

/// What [`spawn`] reports once the thread exists.
#[derive(Debug, Clone, Copy)]
pub struct Spawned {
    /// The Windows thread id (`GetCurrentThreadId` on that thread).
    pub id: u32,
}

/// Stack size of a thread that asks for none: the exe's `SizeOfStackReserve`.
static DEFAULT_STACK: AtomicUsize = AtomicUsize::new(1 << 20);

/// Sets the stack size [`spawn`] uses when asked for 0.
pub fn set_default_stack(bytes: usize) {
    DEFAULT_STACK.store(bytes.max(64 << 10), Ordering::Relaxed);
}

/// Starts `start(param)` on a new guest thread with `stack_size` bytes of
/// stack (0: the image's default). `on_exit` runs on that thread with its
/// exit code once title code is done with it (the start routine returned,
/// or it called `ExitThread`). Returns once the thread has its TEB; the
/// attach callbacks run on the new thread.
pub fn spawn(
    start: StartRoutine,
    param: *mut c_void,
    stack_size: usize,
    on_exit: Box<dyn FnOnce(u32) + Send>,
) -> std::io::Result<Spawned> {
    let param = param as usize;
    let (tx, rx) = std::sync::mpsc::channel();
    builder(stack_size).spawn(move || {
        let code = run_guest(
            || {
                let _ = tx.send(current_id());
            },
            // SAFETY: title code given to us as a thread start routine.
            move || unsafe { start(param as *mut c_void) },
        );
        on_exit(code);
    })?;
    let id = rx.recv().map_err(std::io::Error::other)?;
    Ok(Spawned { id })
}

pub(crate) fn builder(stack_size: usize) -> std::thread::Builder {
    let stack = match stack_size {
        0 => DEFAULT_STACK.load(Ordering::Relaxed),
        n => n.max(64 << 10),
    };
    // std's threads come from glibc with a guard page below the stack.
    std::thread::Builder::new().stack_size(stack)
}

/// On a thread of our own making: attaches it, calls `ready` (the TEB
/// exists), delivers `DLL_THREAD_ATTACH`, runs `body` in a guest frame,
/// then delivers `DLL_THREAD_DETACH` and detaches. Returns `body`'s result,
/// or the code given to `ExitThread`.
pub(crate) fn run_guest(ready: impl FnOnce(), body: impl FnOnce() -> u32) -> u32 {
    windows_fpu();
    let teb = attach_block();
    ready();
    // SAFETY: the calling thread's own TEB, live until `detach` below.
    let code = unsafe {
        guest_frame(teb, || {
            notify_all(DLL_THREAD_ATTACH);
            body()
        })
    };
    detach();
    code
}

/// The x64 Windows ABI starts a thread with the x87 control word `0x027F`
/// (53-bit precision); Linux starts it with `0x037F`. MXCSR is `0x1F80`
/// on both. Only on guest threads: host threads keep their own.
fn windows_fpu() {
    let (x87, mxcsr): (u16, u32) = (0x027F, 0x1F80);
    // SAFETY: loads two control registers from locals.
    unsafe {
        std::arch::asm!(
            "fldcw [{x87}]",
            "ldmxcsr [{mxcsr}]",
            x87 = in(reg) &x87,
            mxcsr = in(reg) &mxcsr,
            options(nostack, readonly),
        )
    };
}

/// Gives a host thread that is about to call title code a TEB and static
/// TLS, and delivers `DLL_THREAD_ATTACH`. Cheap when already attached.
/// The thread detaches when it ends.
pub fn attach_current() {
    if !teb::current().is_null() {
        return;
    }
    attach_block();
    DETACH_AT_EXIT.with(|d| d.0.set(true));
    notify_all(DLL_THREAD_ATTACH);
}

struct DetachAtExit(std::cell::Cell<bool>);
impl Drop for DetachAtExit {
    fn drop(&mut self) {
        if self.0.get() {
            detach();
        }
    }
}
thread_local! {
    static DETACH_AT_EXIT: DetachAtExit = const { DetachAtExit(std::cell::Cell::new(false)) };
}

/// `ExitThread`: leaves the calling thread's title code with `code`.
/// On a thread from [`spawn`] it resumes after the guest frame, which
/// detaches it and runs its `on_exit`.
pub fn exit_current(code: u32) -> ! {
    let teb = teb::current();
    // SAFETY: the calling thread's own TEB, if it has one.
    if !teb.is_null() && unsafe { (*teb).exit_frame } != 0 {
        // SAFETY: the frame is live: we are inside it on this thread.
        unsafe { victory_guest_exit((*teb).exit_frame, code) }
    }
    // ponytail: a host thread attached with attach_current ends without
    // unwinding its Rust frames (their destructors and TLS leak); give it a
    // guest frame if one ever calls ExitThread.
    detach();
    // SAFETY: ends only the calling thread; glibc's join still sees it end.
    unsafe { libc::syscall(libc::SYS_exit, code as libc::c_long) };
    unreachable!("SYS_exit returned")
}

/// The calling thread's Windows thread id.
pub fn current_id() -> u32 {
    // SAFETY: gettid has no preconditions.
    unsafe { libc::gettid() as u32 }
}

/// Code run when an attached thread goes away, after `DLL_THREAD_DETACH`
/// and before its TLS blocks and TEB are freed (FLS callbacks).
static EXIT_HOOKS: Mutex<Vec<fn(&mut Teb)>> = Mutex::new(Vec::new());

/// Adds a hook every detaching thread runs; hooks are never removed.
pub fn add_exit_hook(hook: fn(&mut Teb)) {
    EXIT_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(hook);
}

/// The calling thread's TEB, its stack bounds, TLS blocks and GS base:
/// everything of an attach but `DLL_THREAD_ATTACH`.
fn attach_block() -> *mut Teb {
    // SAFETY: every field of a TEB is valid zeroed.
    let teb: *mut Teb = Box::into_raw(unsafe { teb::zeroed::<Teb>() });
    let (low, high, guard) = stack_bounds();
    // SAFETY: our fresh TEB.
    unsafe {
        let t = &mut *teb;
        t.stack_base = high;
        t.stack_limit = low;
        t.deallocation_stack = low - guard;
        t.self_ = teb;
        t.process_id = std::process::id() as usize;
        t.thread_id = current_id() as usize;
        t.thread_local_storage_pointer = t.tls_blocks.as_mut_ptr();
        t.tls_expansion_slots = t.tls_expansion.as_mut_ptr();
        t.fls_data = t.fls.as_mut_ptr();
        t.peb = teb::peb();
        tls::thread_attached(teb);
    }
    teb::set_current(teb);
    // Room to report a fault, even a stack overflow, on this thread.
    super::signals::attach_thread();
    teb
}

/// The calling thread's stack: (lowest usable address, one past the
/// highest, guard size). The initial thread reports at most 8 MiB, whatever
/// the stack rlimit is.
fn stack_bounds() -> (usize, usize, usize) {
    // SAFETY: queries the calling thread's attributes into a local.
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        let (mut low, mut size, mut guard) = (std::ptr::null_mut(), 0, 0);
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) == 0 {
            libc::pthread_attr_getstack(&attr, &mut low, &mut size);
            libc::pthread_attr_getguardsize(&attr, &mut guard);
            libc::pthread_attr_destroy(&mut attr);
        }
        let high = low as usize + size;
        if std::process::id() == current_id() {
            size = size.min(8 << 20);
        }
        (high - size, high, guard)
    }
}

/// `DLL_THREAD_DETACH`, the exit hooks, then frees the TLS blocks and the
/// TEB and clears the GS base. Nothing if the thread has no TEB.
fn detach() {
    let teb = teb::current();
    if teb.is_null() {
        return;
    }
    notify_all(DLL_THREAD_DETACH);
    let hooks = EXIT_HOOKS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for hook in hooks {
        // SAFETY: the calling thread's own TEB.
        hook(unsafe { &mut *teb });
    }
    // SAFETY: attached by `attach_block` on this thread.
    unsafe { tls::thread_detached(teb) };
    // Title code on this thread now faults instead of reading a freed TEB.
    teb::set_current(std::ptr::null_mut());
    // SAFETY: made by `attach_block`, now unlisted and no longer current.
    drop(unsafe { Box::from_raw(teb) });
}

/// Runs `f` in a guest frame of `teb`'s thread: [`exit_current`] resumes
/// right after it with its code.
///
/// # Safety
/// `teb` is the calling thread's own TEB.
unsafe fn guest_frame<F: FnOnce() -> u32>(teb: *mut Teb, f: F) -> u32 {
    extern "C" fn call<F: FnOnce() -> u32>(f: *mut c_void) -> u32 {
        // SAFETY: `f` is the `Option<F>` below, taken once.
        let f = unsafe { &mut *(f as *mut Option<F>) };
        f.take().expect("guest frame called twice")()
    }
    let mut f = Some(f);
    // SAFETY: `call::<F>` takes the pointer it is given; the frame slot is
    // the TEB's, live for the thread.
    let code = unsafe {
        let frame = &raw mut (*teb).exit_frame;
        let code = victory_guest_call(call::<F>, (&raw mut f).cast(), frame);
        *frame = 0;
        code
    };
    // After `ExitThread`, `f` (or what it moved out) is leaked, not dropped:
    // its frames were abandoned.
    std::mem::forget(f);
    code
}

unsafe extern "C" {
    /// Saves the SysV callee-saved registers, stores the stack pointer in
    /// `*frame`, and returns `f(arg)`, or what `victory_guest_exit` gives.
    fn victory_guest_call(
        f: extern "C" fn(*mut c_void) -> u32,
        arg: *mut c_void,
        frame: *mut usize,
    ) -> u32;
    /// Returns from the `victory_guest_call` whose `*frame` is `frame`, with `code`.
    fn victory_guest_exit(frame: usize, code: u32) -> !;
}

std::arch::global_asm!(
    ".globl victory_guest_call",
    ".type victory_guest_call, @function",
    "victory_guest_call:",
    ".cfi_startproc",
    "push rbp",
    ".cfi_def_cfa_offset 16",
    ".cfi_offset rbp, -16",
    "mov rbp, rsp",
    ".cfi_def_cfa_register rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "sub rsp, 8",
    "mov [rdx], rsp",
    "mov rax, rdi",
    "mov rdi, rsi",
    "call rax",
    "victory_guest_resume:",
    "add rsp, 8",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    ".cfi_def_cfa rsp, 8",
    "ret",
    ".cfi_endproc",
    ".size victory_guest_call, . - victory_guest_call",
    "",
    ".globl victory_guest_exit",
    ".type victory_guest_exit, @function",
    "victory_guest_exit:",
    "mov rsp, rdi",
    "mov eax, esi",
    "lea rbp, [rsp + 48]",
    "jmp victory_guest_resume",
    ".size victory_guest_exit, . - victory_guest_exit",
);

struct Mapped {
    image: Arc<LoadedImage>,
    /// Its `DLL_PROCESS_ATTACH` is done; only then does it get thread notices.
    attached: bool,
    /// `DisableThreadLibraryCalls`: no more `DllMain` thread notices.
    quiet: bool,
}

/// Every title image we mapped; attached ones in initialisation order.
static IMAGES: Mutex<Vec<Mapped>> = Mutex::new(Vec::new());

fn images() -> std::sync::MutexGuard<'static, Vec<Mapped>> {
    IMAGES.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn mapped(image: &LoadedImage) {
    let image = Arc::new(image.clone());
    let mut images = images();
    images.retain(|m| m.image.base() != image.base());
    images.push(Mapped {
        image,
        attached: false,
        quiet: false,
    });
}

/// Its `DLL_PROCESS_ATTACH` is done. The list keeps attach order, the
/// Windows loader's initialisation order.
pub(crate) fn attached(base: usize) {
    let mut images = images();
    if let Some(i) = images.iter().position(|m| m.image.base() == base) {
        let mut m = images.remove(i);
        m.attached = true;
        images.push(m);
    }
}

pub(crate) fn detached(base: usize) {
    if let Some(m) = images().iter_mut().find(|m| m.image.base() == base) {
        m.attached = false;
    }
}

pub(crate) fn unmapped(base: usize) {
    images().retain(|m| m.image.base() != base);
}

/// `DisableThreadLibraryCalls` for a title image; `None` if `base` is not
/// one. Fails, as on Windows, for an image with static TLS.
pub fn disable_thread_calls(base: usize) -> Option<bool> {
    let mut images = images();
    let m = images.iter_mut().find(|m| m.image.base() == base)?;
    if m.image.info().tls.is_some() {
        return Some(false);
    }
    m.quiet = true;
    Some(true)
}

/// Thread notices on the calling thread: starts in initialisation order,
/// ends in reverse, as the Windows loader does.
fn notify_all(reason: u32) {
    // A snapshot: title code may map and attach another DLL from here.
    let mut notify: Vec<_> = images()
        .iter()
        .filter(|m| m.attached)
        .map(|m| (m.image.clone(), !m.quiet))
        .collect();
    if reason == DLL_THREAD_DETACH {
        notify.reverse();
    }
    for (image, dll_main) in notify {
        // SAFETY: attached images have their imports bound.
        unsafe { image.notify(reason, dll_main, std::ptr::null_mut()) };
    }
}

/// `ExitProcess`'s `DLL_PROCESS_DETACH`: every attached image, DLLs in
/// reverse initialisation order, then the exe's TLS callbacks; they hear of
/// no more threads. Ending the other threads first is the caller's.
pub fn process_detach() {
    let attached: Vec<_> = images()
        .iter_mut()
        .filter(|m| m.attached)
        .map(|m| {
            m.attached = false;
            m.image.clone()
        })
        .collect();
    let (dlls, exes): (Vec<_>, Vec<_>) =
        attached.into_iter().rev().partition(|i| i.info().is_dll());
    for image in dlls.iter().chain(&exes) {
        // SAFETY: attached images are bound. A non-null third argument
        // tells the entry points the process is ending.
        unsafe {
            image.notify(
                DLL_PROCESS_DETACH,
                true,
                std::ptr::without_provenance_mut(1),
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gs(offset: usize) -> usize {
        let value: usize;
        // SAFETY: reads the calling thread's TEB, which the caller attached.
        unsafe {
            std::arch::asm!("mov {}, gs:[{}]", out(reg) value, in(reg) offset, options(nostack, readonly))
        };
        value
    }

    /// Writes (TEB, thread id, stack base and limit as distances from a
    /// local, x87 control word) to `out`.
    unsafe extern "win64" fn read_teb(out: *mut c_void) -> u32 {
        let mut x87: u16 = 0;
        // SAFETY: stores the x87 control word into a local.
        unsafe { std::arch::asm!("fnstcw [{}]", in(reg) &mut x87, options(nostack)) };
        let local = &x87 as *const u16 as usize;
        let seen = [
            gs(0x30),
            gs(0x48),
            gs(0x08).wrapping_sub(local),
            local.wrapping_sub(gs(0x10)),
            x87 as usize,
        ];
        // SAFETY: the caller's array.
        unsafe { *(out as *mut [usize; 5]) = seen };
        7
    }

    unsafe extern "win64" fn exits(_: *mut c_void) -> u32 {
        exit_current(42);
    }

    #[test]
    fn spawned_threads_have_a_teb_and_windows_fpu() {
        let mut seen = [0usize; 5];
        let (tx, rx) = std::sync::mpsc::channel();
        let out = &raw mut seen as usize;
        let spawned = spawn(
            read_teb,
            out as *mut c_void,
            256 << 10,
            Box::new(move |code| tx.send(code).unwrap()),
        )
        .unwrap();
        assert_eq!(rx.recv().unwrap(), 7);
        let [teb, tid, to_base, above_limit, x87] = seen;
        assert_ne!(teb, 0);
        assert_eq!(tid, spawned.id as usize);
        // Both bounds within the 256 KiB asked for, on either side.
        assert!(to_base > 0 && to_base < 256 << 10, "{to_base:#x}");
        assert!(
            above_limit > 0 && above_limit < 256 << 10,
            "{above_limit:#x}"
        );
        assert_eq!(x87, 0x027F);
    }

    #[test]
    fn exit_thread_resumes_after_the_guest_frame() {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(
            exits,
            std::ptr::null_mut(),
            0,
            Box::new(move |code| tx.send(code).unwrap()),
        )
        .unwrap();
        assert_eq!(rx.recv().unwrap(), 42);
    }

    #[test]
    fn host_threads_attach_and_detach() {
        std::thread::spawn(|| {
            assert!(teb::current().is_null());
            attach_current();
            let teb = teb::current();
            assert_eq!(gs(0x30), teb as usize);
            teb::set_last_error(87);
            assert_eq!(gs(0x68) as u32, 87);
            attach_current();
            assert_eq!(teb::current(), teb);
        })
        .join()
        .unwrap();
    }
}
