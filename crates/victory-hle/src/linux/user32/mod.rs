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

mod message;
mod types;
mod window;

pub(crate) use message::*;
pub(crate) use types::*;
pub(crate) use window::*;

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::time::Instant;

use victory_loader::linux::{teb, thread as guest_thread};

use crate::{HleModule, hle_export};

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
mod tests;
