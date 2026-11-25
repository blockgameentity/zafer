//! Host input on Linux: gamepads and the keyboard, as SDL3 reports them.
//!
//! Split from `linux.rs` only to keep each file small; everything here goes
//! through the SDL thread in the parent module, whose state (`super::sdl`)
//! the calls below read.

use std::ffi::{c_int, c_void};

use sdl3_sys::everything::SDL_SCANCODE_COUNT;

use super::sdl;

/// A host gamepad as SDL3 reports it: button `i` (`SDL_GamepadButton`) is
/// bit `i`; axes by `SDL_GamepadAxis` (sticks with down positive, triggers
/// 0 to 32767).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u32,
    pub axes: [i16; 6],
}

/// The gamepad in `slot`, `None` when the slot is empty (or no SDL3).
pub fn pad(slot: usize) -> Option<Pad> {
    let sdl = sdl()?;
    let handle = sdl
        .pads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot)
        .copied()??
        .1;
    let pad = handle as *mut c_void;
    let mut state = Pad::default();
    // SAFETY: an open gamepad; SDL locks its joysticks for these reads.
    unsafe {
        for button in 0..15 {
            if (sdl.api.gamepad_button)(pad, button) {
                state.buttons |= 1 << button;
            }
        }
        for (axis, value) in state.axes.iter_mut().enumerate() {
            *value = (sdl.api.gamepad_axis)(pad, axis as c_int);
        }
    }
    Some(state)
}

/// Sets `slot`'s motor speeds (low frequency, high frequency) until the
/// next call.
pub fn rumble(slot: usize, low: u16, high: u16) {
    let Some(sdl) = sdl() else { return };
    let Some(Some((_, handle))) = sdl
        .pads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot)
        .copied()
    else {
        return;
    };
    // SAFETY: an open gamepad. SDL needs a duration; the title renews it.
    unsafe { (sdl.api.rumble_gamepad)(handle as *mut c_void, low, high, 1000) };
}

/// SDL scan codes (USB HID usages) held down while our window has the
/// keyboard focus.
pub fn keys() -> Vec<u16> {
    let Some(sdl) = sdl() else { return Vec::new() };
    let ours = sdl.window.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: SDL's own state arrays, valid for its lifetime.
    unsafe {
        if ours == 0 || (sdl.api.keyboard_focus)() as usize != ours {
            return Vec::new();
        }
        let mut count = 0;
        let state = (sdl.api.keyboard_state)(&mut count);
        let count = (count.max(0) as usize).min(SDL_SCANCODE_COUNT.0 as usize);
        (0..count as u16)
            .filter(|&i| *state.add(usize::from(i)))
            .collect()
    }
}
