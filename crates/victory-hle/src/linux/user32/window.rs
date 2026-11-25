//! Creating, destroying and showing the title's window.
use super::*;

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

pub(crate) fn destroy(hwnd: Hwnd) {
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
