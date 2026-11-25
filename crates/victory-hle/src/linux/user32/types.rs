//! The window structures, their constants and the global desktop.
use super::*;

pub(crate) type Hwnd = usize;
pub(crate) type WndProc = extern "win64" fn(Hwnd, u32, usize, isize) -> isize;

pub(crate) const WM_CREATE: u32 = 0x0001;
pub(crate) const WM_DESTROY: u32 = 0x0002;
pub(crate) const WM_SIZE: u32 = 0x0005;
pub(crate) const WM_CLOSE: u32 = 0x0010;
pub(crate) const WM_ACTIVATEAPP: u32 = 0x001C;
pub(crate) const WM_NCCREATE: u32 = 0x0081;
pub(crate) const WM_NCDESTROY: u32 = 0x0082;

pub(crate) const ERROR_INVALID_PARAMETER: u32 = 87;
pub(crate) const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
pub(crate) const ERROR_CANNOT_FIND_WND_CLASS: u32 = 1407;
pub(crate) const ERROR_INVALID_WINDOW_HANDLE: u32 = 1400;

pub(crate) const PM_REMOVE: u32 = 1;

/// `WNDCLASSEXA`.
#[repr(C)]
pub struct WndClassExA {
    pub(crate) size: u32,
    pub(crate) style: u32,
    pub(crate) proc: Option<WndProc>,
    pub(crate) class_extra: i32,
    pub(crate) window_extra: i32,
    pub(crate) instance: usize,
    pub(crate) icon: usize,
    pub(crate) cursor: usize,
    pub(crate) background: usize,
    pub(crate) menu_name: *const c_char,
    pub(crate) class_name: *const c_char,
    pub(crate) small_icon: usize,
}

/// `CREATESTRUCTA`.
#[repr(C)]
pub struct CreateStructA {
    pub(crate) params: *mut c_void,
    pub(crate) instance: usize,
    pub(crate) menu: usize,
    pub(crate) parent: Hwnd,
    pub(crate) cy: i32,
    pub(crate) cx: i32,
    pub(crate) y: i32,
    pub(crate) x: i32,
    pub(crate) style: i32,
    pub(crate) name: *const c_char,
    pub(crate) class: *const c_char,
    pub(crate) ex_style: u32,
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

pub(crate) struct Window {
    pub(crate) proc: WndProc,
    pub(crate) thread: u32,
    pub(crate) parent: Hwnd,
    pub(crate) size: (i32, i32),
    pub(crate) visible: bool,
}

#[derive(Default)]
pub(crate) struct Desktop {
    pub(crate) classes: HashMap<String, (u16, WndProc)>,
    pub(crate) next_atom: u16,
    pub(crate) windows: HashMap<Hwnd, Window>,
    pub(crate) next_window: Hwnd,
    /// Posted messages, by thread id.
    pub(crate) queues: HashMap<u32, VecDeque<Msg>>,
}

pub(crate) static DESKTOP: Mutex<Option<Desktop>> = Mutex::new(None);

pub(crate) fn desktop<R>(f: impl FnOnce(&mut Desktop) -> R) -> R {
    let mut d = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
    f(d.get_or_insert_with(|| Desktop {
        next_atom: 0xC000,
        next_window: 0x1_0010,
        ..Default::default()
    }))
}

pub(crate) fn tick() -> u32 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_millis() as u32
}

/// A class name, or `#atom` for `MAKEINTATOM`.
pub(crate) fn class_key(name: *const c_char) -> Option<String> {
    if name.is_null() {
        return None;
    }
    if (name as usize) < 0x1_0000 {
        return Some(format!("#{}", name as usize));
    }
    // SAFETY: a NUL-terminated name from the title.
    Some(unsafe { CStr::from_ptr(name) }.to_string_lossy().to_ascii_lowercase())
}
