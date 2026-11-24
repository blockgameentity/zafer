//! A title DLL without a CRT: one export the hle-probe imports, and a
//! `DllMain` that counts its process attaches.
#![no_std]

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

static ATTACHES: AtomicU32 = AtomicU32::new(0);

/// The name hle-probe imports; answers with the attach count plus a marker.
#[unsafe(no_mangle)]
pub extern "system" fn GetFileVersionInfoSizeW(_: *const u16, _: *mut u32) -> u32 {
    0x5EED_0000 | ATTACHES.load(Ordering::Relaxed)
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_: *mut c_void, reason: u32, _: *mut c_void) -> i32 {
    if reason == 1 {
        ATTACHES.fetch_add(1, Ordering::Relaxed);
    }
    1
}
