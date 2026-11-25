//! The window seam: layouts, creation, show and the message queue.
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
