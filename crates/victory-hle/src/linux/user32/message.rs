//! The message queue: peeking, dispatching and posting from the host.
use super::*;

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
