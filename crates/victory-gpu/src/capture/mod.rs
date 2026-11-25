//! Presented frames written to PNG files, to see what reaches the screen:
//! with `VICTORY_CAPTURE=<folder>`, every frame whose number is a power of
//! two (the frames the log reports) is saved as `frame-<n>.png`, at half
//! size. Only in builds with diagnostics.
#[cfg(target_os = "windows")]
pub(crate) mod dump;
#[cfg(target_os = "windows")]
pub(crate) mod png;
#[cfg(target_os = "windows")]
pub(crate) mod probe;
#[cfg(target_os = "windows")]
pub(crate) mod readback;
#[cfg(target_os = "windows")]
pub(crate) mod write;

#[cfg(target_os = "windows")]
pub(crate) use dump::*;
#[cfg(target_os = "windows")]
pub(crate) use png::*;
#[cfg(target_os = "windows")]
pub(crate) use probe::*;
#[cfg(target_os = "windows")]
pub(crate) use readback::*;
#[cfg(target_os = "windows")]
pub(crate) use write::*;
