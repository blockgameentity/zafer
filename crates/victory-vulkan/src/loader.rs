//! The Vulkan loader, opened at run time.
//!
//! Vulkan has no stable link-time entry point: the loader is a library the
//! host provides (`libvulkan.so.1`, `vulkan-1.dll`) and everything goes
//! through `vkGetInstanceProcAddr`. `ash` opens it with `libloading`, so the
//! backend runs on a machine with no Vulkan at all: [`entry`] says so
//! instead of failing to load, and every test skips cleanly.
//!
//! The loader is opened once and kept for the life of the process, because
//! an `ash::Entry` owns the library handle every later call goes through.

use std::sync::OnceLock;

use ash::Entry;

/// The loader, or why it could not be opened.
static ENTRY: OnceLock<Result<Entry, String>> = OnceLock::new();

/// The loader, opened on first use and kept.
///
/// The error is a message rather than a type because the only decision any
/// caller makes is whether Vulkan is installed at all.
pub fn entry() -> Result<&'static Entry, &'static str> {
    ENTRY
        .get_or_init(|| {
            // SAFETY: `Entry::load` opens the loader and looks up the
            // global entry points; nothing else in the process has to be
            // alive for it, and the result is kept for the process's life.
            unsafe { Entry::load() }.map_err(|e| format!("no Vulkan loader: {e}"))
        })
        .as_ref()
        .map_err(String::as_str)
}
