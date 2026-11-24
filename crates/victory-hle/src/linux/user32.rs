//! Windows and their messages (`user32`, `ext-ms-win-rtcore-ntuser-*`): the
//! title registers a class, creates its one window and pumps messages
//! with `PeekMessageA` and `DispatchMessageA`.
//!
//! A window here is a handle, its class's procedure and a message queue
//! of the thread that made it; nothing is drawn. The Linux window that
//! shows the title's frames belongs to `victory-gpu`: whoever owns it
//! finds the title's window with [`main_window`] and posts input and
//! life-cycle messages (`WM_CLOSE`, `WM_ACTIVATEAPP`, `WM_SIZE`) to it
//! with [`post_message`], as the desktop would on Windows.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![allow(clippy::too_many_arguments)]
#![allow(dead_code)] // the window seam is for the GPU slice

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::time::Instant;

use victory_loader::linux::{teb, thread as guest_thread};

use crate::{HleModule, hle_export};

pub(crate) type Hwnd = usize;
type WndProc = extern "win64" fn(Hwnd, u32, usize, isize) -> isize;

pub(crate) const WM_CREATE: u32 = 0x0001;
pub(crate) const WM_DESTROY: u32 = 0x0002;
pub(crate) const WM_SIZE: u32 = 0x0005;
pub(crate) const WM_CLOSE: u32 = 0x0010;
pub(crate) const WM_ACTIVATEAPP: u32 = 0x001C;
pub(crate) const WM_NCCREATE: u32 = 0x0081;
pub(crate) const WM_NCDESTROY: u32 = 0x0082;

const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
const ERROR_CANNOT_FIND_WND_CLASS: u32 = 1407;
const ERROR_INVALID_WINDOW_HANDLE: u32 = 1400;

const PM_REMOVE: u32 = 1;

/// `WNDCLASSEXA`.
#[repr(C)]
pub struct WndClassExA {
    size: u32,
    style: u32,
    proc: Option<WndProc>,
    class_extra: i32,
    window_extra: i32,
    instance: usize,
    icon: usize,
    cursor: usize,
    background: usize,
    menu_name: *const c_char,
    class_name: *const c_char,
    small_icon: usize,
}

/// `CREATESTRUCTA`.
#[repr(C)]
pub struct CreateStructA {
    params: *mut c_void,
    instance: usize,
    menu: usize,
    parent: Hwnd,
    cy: i32,
    cx: i32,
    y: i32,
    x: i32,
    style: i32,
    name: *const c_char,
    class: *const c_char,
    ex_style: u32,
}

/// `MSG`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Msg {
    pub(crate) hwnd: Hwnd,
    pub(crate) message: u32,
    pub(crate) wparam: usize,
    pub(crate) lparam: isize,
    pub(crate) time: u32,
    pub(crate) pt: [i32; 2],
    pub(crate) private: u32,
}

struct Window {
    proc: WndProc,
    thread: u32,
    parent: Hwnd,
    size: (i32, i32),
    visible: bool,
}

#[derive(Default)]
struct Desktop {
    classes: HashMap<String, (u16, WndProc)>,
    next_atom: u16,
    windows: HashMap<Hwnd, Window>,
    next_window: Hwnd,
    /// Posted messages, by thread id.
    queues: HashMap<u32, VecDeque<Msg>>,
}

static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

fn desktop<R>(f: impl FnOnce(&mut Desktop) -> R) -> R {
    let mut d = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
    f(d.get_or_insert_with(|| Desktop {
        next_atom: 0xC000,
        next_window: 0x1_0010,
        ..Default::default()
    }))
}

fn tick() -> u32 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_millis() as u32
}

/// A class name, or `#atom` for `MAKEINTATOM`.
fn class_key(name: *const c_char) -> Option<String> {
    if name.is_null() {
        return None;
    }
    if (name as usize) < 0x1_0000 {
        return Some(format!("#{}", name as usize));
    }
    // SAFETY: a NUL-terminated name from the title.
    Some(unsafe { CStr::from_ptr(name) }.to_string_lossy().to_ascii_lowercase())
}

pub extern "win64" fn RegisterClassExA(class: *const WndClassExA) -> u16 {
    // SAFETY: the title's WNDCLASSEXA, or null.
    let Some(c) = (unsafe { class.as_ref() }) else {
        teb::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let (Some(proc), Some(name)) = (c.proc, class_key(c.class_name)) else {
        teb::set_last_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    desktop(|d| {
        if d.classes.contains_key(&name) {
            teb::set_last_error(ERROR_CLASS_ALREADY_EXISTS);
            return 0;
        }
        let atom = d.next_atom;
        d.next_atom += 1;
        d.classes.insert(format!("#{atom}"), (atom, proc));
        d.classes.insert(name, (atom, proc));
        atom
    })
}

pub extern "win64" fn CreateWindowExA(
    ex_style: u32,
    class: *const c_char,
    name: *const c_char,
    style: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: Hwnd,
    menu: usize,
    instance: usize,
    params: *mut c_void,
) -> Hwnd {
    let Some(proc) = class_key(class).and_then(|k| desktop(|d| d.classes.get(&k).map(|c| c.1))) else {
        teb::set_last_error(ERROR_CANNOT_FIND_WND_CLASS);
        return 0;
    };
    let hwnd = desktop(|d| {
        let h = d.next_window;
        d.next_window += 2;
        d.windows.insert(
            h,
            Window {
                proc,
                thread: guest_thread::current_id(),
                parent,
                size: (width, height),
                visible: false,
            },
        );
        h
    });
    let cs = CreateStructA {
        params,
        instance,
        menu,
        parent,
        cy: height,
        cx: width,
        y,
        x,
        style: style as i32,
        name,
        class,
        ex_style,
    };
    let lparam = &cs as *const CreateStructA as isize;
    // As on Windows: WM_NCCREATE may refuse the window, WM_CREATE with -1.
    if proc(hwnd, WM_NCCREATE, 0, lparam) == 0 || proc(hwnd, WM_CREATE, 0, lparam) == -1 {
        destroy(hwnd);
        return 0;
    }
    tracing::info!(hwnd, width, height, "title window created");
    hwnd
}

fn destroy(hwnd: Hwnd) {
    let Some(proc) = desktop(|d| d.windows.get(&hwnd).map(|w| w.proc)) else {
        return;
    };
    proc(hwnd, WM_DESTROY, 0, 0);
    proc(hwnd, WM_NCDESTROY, 0, 0);
    desktop(|d| {
        d.windows.remove(&hwnd);
        for q in d.queues.values_mut() {
            q.retain(|m| m.hwnd != hwnd);
        }
    });
}

pub extern "win64" fn DefWindowProcA(hwnd: Hwnd, message: u32, _wparam: usize, _lparam: isize) -> isize {
    match message {
        WM_NCCREATE => 1,
        WM_CLOSE => {
            destroy(hwnd);
            0
        }
        _ => 0,
    }
}

/// Whether the window was visible before.
pub extern "win64" fn ShowWindow(hwnd: Hwnd, command: i32) -> i32 {
    desktop(|d| match d.windows.get_mut(&hwnd) {
        Some(w) => std::mem::replace(&mut w.visible, command != 0) as i32,
        None => {
            teb::set_last_error(ERROR_INVALID_WINDOW_HANDLE);
            0
        }
    })
}

pub extern "win64" fn PeekMessageA(out: *mut Msg, hwnd: Hwnd, first: u32, last: u32, remove: u32) -> i32 {
    if out.is_null() {
        return 0;
    }
    let me = guest_thread::current_id();
    let all = first == 0 && last == 0;
    let found = desktop(|d| {
        let q = d.queues.get_mut(&me)?;
        let i = q.iter().position(|m| {
            let window = match hwnd {
                0 => true,
                usize::MAX => m.hwnd == 0,
                h => m.hwnd == h,
            };
            window && (all || (first..=last).contains(&m.message))
        })?;
        if remove & PM_REMOVE != 0 { q.remove(i) } else { q.get(i).copied() }
    });
    match found {
        Some(m) => {
            // SAFETY: checked non-null; the title's MSG.
            unsafe { out.write(m) };
            1
        }
        None => 0,
    }
}

/// No character messages: keyboard text comes through GameInput.
pub extern "win64" fn TranslateMessage(_msg: *const Msg) -> i32 {
    0
}

pub extern "win64" fn DispatchMessageA(msg: *const Msg) -> isize {
    // SAFETY: the title's MSG, or null.
    let Some(m) = (unsafe { msg.as_ref() }) else {
        return 0;
    };
    match desktop(|d| d.windows.get(&m.hwnd).map(|w| w.proc)) {
        Some(proc) => proc(m.hwnd, m.message, m.wparam, m.lparam),
        None => 0,
    }
}

/// The title's first top-level window and its size, once it exists.
pub(crate) fn main_window() -> Option<(Hwnd, i32, i32)> {
    desktop(|d| {
        d.windows
            .iter()
            .filter(|(_, w)| w.parent == 0)
            .min_by_key(|(h, _)| **h)
            .map(|(&h, w)| (h, w.size.0, w.size.1))
    })
}

/// Queues `message` for `hwnd`'s thread, which gets it from its next
/// `PeekMessageA`; false if there is no such window.
pub(crate) fn post_message(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> bool {
    let time = tick();
    desktop(|d| {
        let Some(thread) = d.windows.get(&hwnd).map(|w| w.thread) else {
            return false;
        };
        d.queues.entry(thread).or_default().push_back(Msg {
            hwnd,
            message,
            wparam,
            lparam,
            time,
            pt: [0; 2],
            private: 0,
        });
        true
    })
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "user32",
        dlls: &["user32.dll", "ext-ms-win-rtcore-ntuser-*"],
        exports: vec![
            hle_export!(RegisterClassExA),
            hle_export!(CreateWindowExA),
            hle_export!(DefWindowProcA),
            hle_export!(ShowWindow),
            hle_export!(PeekMessageA),
            hle_export!(TranslateMessage),
            hle_export!(DispatchMessageA),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEEN: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    static PARAM: AtomicU32 = AtomicU32::new(0);

    extern "win64" fn proc(hwnd: Hwnd, message: u32, wparam: usize, lparam: isize) -> isize {
        SEEN.lock().unwrap().push(message);
        if message == WM_CREATE {
            // SAFETY: WM_CREATE's lparam is the CREATESTRUCTA.
            let cs = unsafe { &*(lparam as *const CreateStructA) };
            PARAM.store(cs.params as u32, Ordering::Relaxed);
            assert_eq!((cs.cx, cs.cy), (1920, 1080));
        }
        DefWindowProcA(hwnd, message, wparam, lparam)
    }

    #[test]
    fn layouts_are_windows() {
        assert_eq!(size_of::<WndClassExA>(), 80);
        assert_eq!(std::mem::offset_of!(WndClassExA, class_name), 64);
        assert_eq!(size_of::<CreateStructA>(), 80);
        assert_eq!(size_of::<Msg>(), 48);
        assert_eq!(std::mem::offset_of!(Msg, time), 32);
    }

    #[test]
    fn a_window_gets_created_shown_and_its_posted_messages() {
        let class = WndClassExA {
            size: 80,
            style: 0,
            proc: Some(proc),
            class_extra: 0,
            window_extra: 0,
            instance: 0,
            icon: 0,
            cursor: 0,
            background: 0,
            menu_name: std::ptr::null(),
            class_name: c"victory test".as_ptr(),
            small_icon: 0,
        };
        let atom = RegisterClassExA(&class);
        assert_ne!(atom, 0);
        assert_eq!(RegisterClassExA(&class), 0);
        let missing = CreateWindowExA(0, c"nope".as_ptr(), std::ptr::null(), 0, 0, 0, 1, 1, 0, 0, 0, std::ptr::null_mut());
        assert_eq!((missing, teb::last_error()), (0, ERROR_CANNOT_FIND_WND_CLASS));

        let hwnd = CreateWindowExA(0, atom as usize as *const c_char, c"t".as_ptr(), 0, 0, 0, 1920, 1080, 0, 0, 0, 42 as *mut c_void);
        assert_ne!(hwnd, 0);
        assert_eq!(*SEEN.lock().unwrap(), [WM_NCCREATE, WM_CREATE]);
        assert_eq!(PARAM.load(Ordering::Relaxed), 42);
        assert_eq!(ShowWindow(hwnd, 5), 0);
        assert_eq!(ShowWindow(hwnd, 5), 1);
        assert_eq!(main_window().map(|w| w.0), Some(hwnd));

        let mut m = Msg { hwnd: 0, message: 0, wparam: 0, lparam: 0, time: 0, pt: [0; 2], private: 0 };
        assert_eq!(PeekMessageA(&mut m, 0, 0, 0, PM_REMOVE), 0);
        assert!(post_message(hwnd, WM_ACTIVATEAPP, 1, 0));
        assert!(post_message(hwnd, WM_CLOSE, 0, 0));
        // A range filter skips the first; peeking without removing keeps it.
        assert_eq!(PeekMessageA(&mut m, 0, WM_CLOSE, WM_CLOSE, 0), 1);
        assert_eq!(m.message, WM_CLOSE);
        assert_eq!(PeekMessageA(&mut m, 0, 0, 0, PM_REMOVE), 1);
        assert_eq!(m.message, WM_ACTIVATEAPP);
        assert_eq!(TranslateMessage(&m), 0);
        DispatchMessageA(&m);
        assert_eq!(PeekMessageA(&mut m, hwnd, 0, 0, PM_REMOVE), 1);
        // DefWindowProc closes the window: WM_DESTROY, then it is gone.
        DispatchMessageA(&m);
        assert_eq!(SEEN.lock().unwrap()[2..], [WM_ACTIVATEAPP, WM_CLOSE, WM_DESTROY, WM_NCDESTROY]);
        assert!(!post_message(hwnd, WM_CLOSE, 0, 0));
    }
}
