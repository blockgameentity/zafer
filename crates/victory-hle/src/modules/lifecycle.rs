//! The title's life cycle. A console title is never told it is closing:
//! quitting it or launching a new game first suspends it (its suspend
//! routines run, and it has a second to save) and then ends the process.
//! Closing the title's window does the same here (on Linux, so do SIGINT,
//! SIGTERM and SIGHUP; the window's owner raises SIGTERM when the user
//! closes it); a new launch then starts `victory` again, which waits for
//! this process to be gone.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{self as wm, HMENU};
#[cfg(windows)]
use windows_sys::core::{PCSTR, PCWSTR, w};

use crate::{HleModule, hle_export};

type Routine = extern "win64" fn(quiesced: u8, context: *mut c_void);

/// Registration id, routine and context of each suspend routine.
static ROUTINES: Mutex<Vec<(usize, Routine, usize)>> = Mutex::new(Vec::new());

fn routines() -> MutexGuard<'static, Vec<(usize, Routine, usize)>> {
    ROUTINES.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long the console lets suspend routines run before it ends the title.
const SUSPEND_DEADLINE: Duration = Duration::from_secs(1);

const ERROR_INVALID_PARAMETER: u32 = 87;

pub extern "win64" fn RegisterAppStateChangeNotification(
    routine: Option<Routine>,
    context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    let Some(routine) = routine else {
        return ERROR_INVALID_PARAMETER;
    };
    if registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    routines().push((id, routine, context as usize));
    // SAFETY: checked non-null; the title passes room for its registration.
    unsafe { registration.write(id) };
    0
}

pub extern "win64" fn UnregisterAppStateChangeNotification(registration: usize) {
    routines().retain(|&(id, ..)| id != registration);
}

/// A console title is never constrained, so the routine is never called.
pub extern "win64" fn RegisterAppConstrainedChangeNotification(
    routine: Option<Routine>,
    _context: *mut c_void,
    registration: *mut usize,
) -> u32 {
    if routine.is_none() || registration.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    // SAFETY: checked non-null; the title passes room for its registration.
    unsafe { registration.write(1) };
    0
}

pub extern "win64" fn UnregisterAppConstrainedChangeNotification(_registration: usize) {}

#[cfg(windows)]
/// The window procedure a title window had before ours.
const TITLE_PROC: PCWSTR = w!("victory.wndproc");

#[cfg(windows)]
pub extern "win64" fn CreateWindowExW(
    ex_style: u32,
    class: PCWSTR,
    name: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExW(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    hwnd
}

#[cfg(windows)]
/// See [`CreateWindowExW`].
pub extern "win64" fn CreateWindowExA(
    ex_style: u32,
    class: PCSTR,
    name: PCSTR,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    menu: HMENU,
    instance: HINSTANCE,
    param: *const c_void,
) -> HWND {
    let (x, y, width, height) = fit(parent, x, y, width, height);
    // SAFETY: the title's own arguments, forwarded (the size fitted).
    let hwnd = unsafe {
        wm::CreateWindowExA(
            ex_style, class, name, style, x, y, width, height, parent, menu, instance, param,
        )
    };
    hook(hwnd);
    hwnd
}

#[cfg(windows)]
/// A console title sizes its window to its render target (Grand Theft
/// Auto VI: 3840x2160 on a Series X), which can exceed the host's screen.
/// A top-level window larger than the work area is shrunk to fit, keeping
/// its proportions, and centred; what it shows is scaled to it (the swap
/// chain stretches).
fn fit(parent: HWND, x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    let mut area = RECT::default();
    // SAFETY: `area` receives the primary monitor's work area.
    let known =
        unsafe { wm::SystemParametersInfoW(wm::SPI_GETWORKAREA, 0, (&raw mut area).cast(), 0) };
    let fitted = fit_into(&area, width, height);
    if !parent.is_null() || known == 0 || fitted == (width, height) {
        return (x, y, width, height);
    }
    tracing::info!(width, height, to = ?fitted, "title window fitted to the screen");
    let (w, h) = fitted;
    let left = area.left + (area.right - area.left - w) / 2;
    let top = area.top + (area.bottom - area.top - h) / 2;
    (left, top, w, h)
}

#[cfg(windows)]
/// `width` x `height` scaled down to fit `area`, or unchanged if it fits
/// (or is `CW_USEDEFAULT`).
fn fit_into(area: &RECT, width: i32, height: i32) -> (i32, i32) {
    let (aw, ah) = (area.right - area.left, area.bottom - area.top);
    if width <= 0 || height <= 0 || aw <= 0 || ah <= 0 || (width <= aw && height <= ah) {
        return (width, height);
    }
    let scale = f64::min(
        f64::from(aw) / f64::from(width),
        f64::from(ah) / f64::from(height),
    );
    (
        (f64::from(width) * scale) as i32,
        (f64::from(height) * scale) as i32,
    )
}

#[cfg(windows)]
/// Puts our procedure in front of the title's.
fn hook(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    // SAFETY: a window this thread just created. The title's procedure is
    // stored before ours can run.
    unsafe {
        let title = wm::GetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC);
        wm::SetPropW(hwnd, TITLE_PROC, title as *mut c_void);
        wm::SetWindowLongPtrW(hwnd, wm::GWLP_WNDPROC, window_proc as *const () as isize);
    }
}

#[cfg(windows)]
extern "win64" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A user close (close button, Alt+F4, window or taskbar menu); a
    // console title never sees one, nor a WM_CLOSE from it.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_CLOSE as usize {
        end_title("window closed", Box::new(|| {}));
        return 0;
    }
    // Alt or F10 puts a window into menu mode: the system runs a modal menu
    // loop on the thread that owns the window, which is the title's. The
    // title then stops (no frames, no input) until a key ends the loop, and
    // nothing shows it, since the window has no menu bar; Grand Theft Auto
    // VI aborts itself about half a minute later. A console has no window
    // menu. The mouse still opens it from the title bar.
    if message == wm::WM_SYSCOMMAND && wparam & 0xFFF0 == wm::SC_KEYMENU as usize {
        return 0;
    }
    // SAFETY: the procedure `hook` stored, called the way the system would.
    unsafe {
        let title = wm::GetPropW(hwnd, TITLE_PROC);
        if message == wm::WM_NCDESTROY {
            wm::RemovePropW(hwnd, TITLE_PROC);
        }
        let title = std::mem::transmute::<*mut c_void, wm::WNDPROC>(title);
        wm::CallWindowProcW(title, hwnd, message, wparam, lparam)
    }
}

/// Suspends the title as the console does when it ends it, runs `then`
/// (once), then ends the process. Returns at once; later calls do nothing.
pub(crate) fn end_title(why: &'static str, then: Box<dyn FnOnce() + Send>) {
    static ENDING: AtomicBool = AtomicBool::new(false);
    if ENDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let suspend = routines().clone();
    tracing::info!(routines = suspend.len(), "{why}: suspending the title");
    // Whoever comes first (the routines or the deadline) runs `then`; the
    // other waits for it, so the process never ends halfway through.
    let then = std::sync::Arc::new(Mutex::new(Some(then)));
    let finish = move |how: &str| {
        let mut then = then.lock().unwrap_or_else(|e| e.into_inner());
        tracing::info!("{how}; ending the title");
        if let Some(then) = then.take() {
            then();
        }
        end();
    };
    let late = finish.clone();
    // Not on the calling thread: titles hand the suspend to their window
    // thread and wait for it inside the routine.
    std::thread::spawn(move || {
        // Title code: on Linux the thread needs a TEB first.
        #[cfg(target_os = "linux")]
        victory_loader::linux::thread::attach_current();
        for (_, routine, context) in suspend {
            routine(1, context as *mut c_void);
        }
        finish("title suspended");
    });
    std::thread::spawn(move || {
        std::thread::sleep(SUSPEND_DEADLINE);
        late("title did not finish suspending in time");
    });
}

/// Set in the environment of a relaunched `victory`: what identifies the
/// process it replaces (Windows: an inherited process handle; Linux: its
/// pid).
const RELAUNCHED_FROM: &str = "VICTORY_RELAUNCHED_FROM";

/// Starts `victory` again with `args` (after the title has suspended); the
/// new process waits until this one has ended.
pub(crate) fn relaunch(args: Vec<std::ffi::OsString>) {
    let me = this_process();
    let spawned = std::env::current_exe().and_then(|exe| {
        let mut command = std::process::Command::new(exe);
        command.args(&args);
        if let Some(me) = me {
            command.env(RELAUNCHED_FROM, me.to_string());
        }
        command.spawn()
    });
    match spawned {
        Ok(child) => tracing::info!(pid = child.id(), ?args, "relaunched"),
        Err(e) => tracing::warn!(%e, "relaunch failed; the title just ends"),
    }
}

/// An inheritable duplicate of our own process handle.
#[cfg(windows)]
fn this_process() -> Option<usize> {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    let mut me: HANDLE = std::ptr::null_mut();
    // SAFETY: an inheritable duplicate of our own process handle.
    let duplicated = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut me,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } != 0;
    duplicated.then_some(me as usize)
}

#[cfg(target_os = "linux")]
fn this_process() -> Option<usize> {
    Some(std::process::id() as usize)
}

/// [`RELAUNCHED_FROM`], taken out of our environment.
fn previous() -> Option<usize> {
    let value = std::env::var(RELAUNCHED_FROM).ok()?.parse().ok()?;
    // SAFETY: removing our own variable before any other thread reads it.
    unsafe { std::env::remove_var(RELAUNCHED_FROM) };
    Some(value)
}

/// In a relaunched `victory`: waits (30 s at most) for the process it
/// replaces to be gone, so the two never share the title's files.
#[cfg(windows)]
pub fn wait_for_previous() {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let Some(handle) = previous() else {
        return;
    };
    let handle = handle as HANDLE;
    // SAFETY: the inherited process handle the previous process gave us.
    unsafe {
        WaitForSingleObject(handle, 30_000);
        CloseHandle(handle);
    }
}

/// See the Windows twin; a pidfd turns readable when the process ends.
#[cfg(target_os = "linux")]
pub fn wait_for_previous() {
    let Some(pid) = previous() else {
        return;
    };
    // SAFETY: pidfd_open takes a pid and no flags.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as i32;
    if fd < 0 {
        return; // already gone
    }
    let mut ended = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd over the descriptor we just opened, closed after.
    unsafe {
        libc::poll(&mut ended, 1, 30_000);
        libc::close(fd);
    }
}

/// SIGINT, SIGTERM and SIGHUP end the title as the console ends it
/// ([`end_title`]), as closing its window does on Windows; the Linux
/// window's owner raises SIGTERM when the user closes it. Install before
/// the title starts (SDL then leaves these signals alone).
#[cfg(target_os = "linux")]
pub fn end_on_signals() {
    use std::sync::atomic::AtomicI32;
    static PIPE: AtomicI32 = AtomicI32::new(-1);
    extern "C" fn on_signal(signal: libc::c_int) {
        let byte = signal as u8;
        // SAFETY: write(2) is async-signal-safe; the pipe is never closed.
        unsafe { libc::write(PIPE.load(Ordering::Relaxed), (&raw const byte).cast(), 1) };
    }
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills the two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        tracing::warn!("no signal pipe; signals end the process at once");
        return;
    }
    PIPE.store(fds[1], Ordering::Relaxed);
    let read = fds[0];
    let watcher = std::thread::Builder::new()
        .name("victory signals".into())
        .spawn(move || {
            let mut signal = 0u8;
            // SAFETY: reads one byte into a local; retried on EINTR.
            while unsafe { libc::read(read, (&raw mut signal).cast(), 1) } != 1 {}
            let why = match i32::from(signal) {
                libc::SIGINT => "SIGINT",
                libc::SIGHUP => "SIGHUP",
                _ => "SIGTERM",
            };
            end_title(why, Box::new(|| {}));
        });
    if watcher.is_err() {
        return;
    }
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: a handler that only writes to the pipe; zeroed is a valid
        // empty sigaction.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

fn end() {
    // Compiled host pipelines kept for the next run.
    #[cfg(windows)]
    victory_gpu::pipeline::save_pipeline_cache();
    // SAFETY: ends this process, as the console ends a suspended title.
    #[cfg(windows)]
    unsafe {
        TerminateProcess(GetCurrentProcess(), 0)
    };
    // SAFETY: as above; no exit handlers run, as with TerminateProcess.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::_exit(0)
    };
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "lifecycle",
        dlls: &[
            "api-ms-win-core-psm-appnotify-*",
            "ext-ms-win-rtcore-ntuser-window-*",
        ],
        exports: vec![
            hle_export!(RegisterAppStateChangeNotification),
            hle_export!(UnregisterAppStateChangeNotification),
            hle_export!(RegisterAppConstrainedChangeNotification),
            hle_export!(UnregisterAppConstrainedChangeNotification),
            #[cfg(windows)]
            hle_export!(CreateWindowExW),
            #[cfg(windows)]
            hle_export!(CreateWindowExA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "win64" fn noop(_: u8, _: *mut c_void) {}

    #[test]
    fn registrations_come_and_go() {
        let (mut a, mut b) = (0, 0);
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut a),
            0
        );
        assert_eq!(
            RegisterAppStateChangeNotification(Some(noop), std::ptr::null_mut(), &mut b),
            0
        );
        UnregisterAppStateChangeNotification(a);
        let left: Vec<_> = routines().iter().map(|r| r.0).collect();
        assert_eq!(left, [b]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sigterm_suspends_the_title_then_ends_the_process() {
        extern "win64" fn routine(quiesced: u8, pipe: *mut c_void) {
            // SAFETY: one byte to the test's pipe.
            unsafe { libc::write(pipe as usize as i32, (&raw const quiesced).cast(), 1) };
        }
        let mut fds = [0; 2];
        // SAFETY: pipe fills the two descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: the child installs the handler, registers and raises; it
        // leaves only through `_exit`.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            end_on_signals();
            let mut id = 0;
            RegisterAppStateChangeNotification(Some(routine), fds[1] as usize as *mut c_void, &mut id);
            // SAFETY: signals ourselves; the handler only writes a pipe.
            unsafe { libc::raise(libc::SIGTERM) };
            std::thread::sleep(Duration::from_secs(10));
            // SAFETY: ends the child, never reached if the signal worked.
            unsafe { libc::_exit(3) };
        }
        let mut status = 0;
        let started = std::time::Instant::now();
        // SAFETY: waits for our own child without blocking.
        while unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == 0 {
            if started.elapsed() > Duration::from_secs(20) {
                // SAFETY: our own child.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("the child did not end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0, "{status:#x}");
        let mut quiesced = 0u8;
        // SAFETY: reads the byte the routine wrote.
        assert_eq!(unsafe { libc::read(fds[0], (&raw mut quiesced).cast(), 1) }, 1);
        assert_eq!(quiesced, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_relaunch_waits_for_the_previous_process() {
        let mut child = std::process::Command::new("sleep").arg("0.3").spawn().unwrap();
        // SAFETY: no other test reads this variable.
        unsafe { std::env::set_var(RELAUNCHED_FROM, child.id().to_string()) };
        let started = std::time::Instant::now();
        wait_for_previous();
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(250) && waited < Duration::from_secs(10), "{waited:?}");
        assert!(child.try_wait().unwrap().is_some());
        assert!(std::env::var(RELAUNCHED_FROM).is_err());
        // Gone (and reaped) already: returns at once.
        // SAFETY: as above.
        unsafe { std::env::set_var(RELAUNCHED_FROM, child.id().to_string()) };
        wait_for_previous();
    }

    #[cfg(windows)]
    #[test]
    fn oversized_windows_shrink_to_the_screen() {
        let area = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(fit_into(&area, 3840, 2160), (1848, 1040));
        assert_eq!(fit_into(&area, 1280, 720), (1280, 720));
        assert_eq!(
            fit_into(&area, wm::CW_USEDEFAULT, 0),
            (wm::CW_USEDEFAULT, 0)
        );
    }

    #[cfg(windows)]
    #[test]
    fn hooked_windows_still_reach_the_title_procedure() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("before"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        let mut text = [0u16; 16];
        // SAFETY: our window; the buffer holds 16 characters.
        unsafe {
            wm::SetWindowTextW(hwnd, w!("after"));
            let n = wm::GetWindowTextW(hwnd, text.as_mut_ptr(), 16);
            assert_eq!(String::from_utf16_lossy(&text[..n as usize]), "after");
            assert!(!wm::GetPropW(hwnd, TITLE_PROC).is_null());
            wm::DestroyWindow(hwnd);
        }
    }

    #[cfg(windows)]
    thread_local! {
        /// The system commands `recording` received, low four bits cleared.
        static COMMANDS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// Stands in for a title's procedure: notes system commands and acts
    /// on none of them (no menu loop, no minimising).
    #[cfg(windows)]
    extern "system" fn recording(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == wm::WM_SYSCOMMAND {
            COMMANDS.with(|c| c.borrow_mut().push(wparam & 0xFFF0));
            return 0;
        }
        // SAFETY: the system's default procedure, called as the system would.
        unsafe { wm::DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    #[cfg(windows)]
    #[test]
    fn the_keyboard_menu_never_reaches_the_title() {
        let null = std::ptr::null_mut();
        let hwnd = CreateWindowExW(
            0,
            w!("STATIC"),
            w!("menu"),
            0,
            0,
            0,
            10,
            10,
            null,
            null,
            null,
            std::ptr::null(),
        );
        assert!(!hwnd.is_null());
        // SAFETY: our window, owned by this thread, so each message is
        // handled before SendMessageW returns. `recording` stands in for the
        // title's procedure and the original is put back before the window
        // is destroyed.
        unsafe {
            let title = wm::GetPropW(hwnd, TITLE_PROC);
            wm::SetPropW(hwnd, TITLE_PROC, recording as *const () as *mut c_void);
            let command = |c: u32, key: u8| {
                wm::SendMessageW(hwnd, wm::WM_SYSCOMMAND, c as usize, LPARAM::from(key))
            };
            command(wm::SC_KEYMENU, 0); // Alt or F10
            command(wm::SC_KEYMENU, b' '); // Alt+Space
            command(wm::SC_KEYMENU | 1, 0); // the low bits are the system's
            command(wm::SC_MINIMIZE, 0);
            wm::SetPropW(hwnd, TITLE_PROC, title);
            wm::DestroyWindow(hwnd);
        }
        let seen = COMMANDS.with(|c| c.borrow().clone());
        assert_eq!(seen, [wm::SC_MINIMIZE as usize]);
    }
}
