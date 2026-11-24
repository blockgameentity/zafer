//! ucrtbase: the heap (`api-ms-win-crt-heap`). Blocks are plain host
//! `malloc` blocks (16-byte aligned on x86-64, as the UCRT's), so a block
//! from here and one from the kernel32 heap can be told apart only by
//! which `free` the title calls. `_aligned_*` blocks keep the UCRT's
//! layout: the base pointer sits in the word below the aligned one.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use super::ucrt_runtime::{EINVAL, ENOMEM, fail, invalid};
use crate::{HleModule, hle_export};

/// `_set_new_handler`'s `_PNH`: `int (*)(size_t)`.
pub(crate) static NEW_HANDLER: AtomicUsize = AtomicUsize::new(0);
/// `_set_new_mode(1)`: a failed `malloc` calls the new handler and retries.
static NEW_MODE: AtomicI32 = AtomicI32::new(0);

/// `_HEAP_MAXREQ`.
const MAX_REQUEST: usize = usize::MAX & !0x1F;

/// Whether the title's new handler freed something; 0 without one.
pub extern "win64" fn _callnewh(size: usize) -> i32 {
    let handler = NEW_HANDLER.load(Ordering::Acquire);
    if handler == 0 {
        return 0;
    }
    // SAFETY: the `_PNH` the title registered.
    let handler: extern "win64" fn(usize) -> i32 = unsafe { std::mem::transmute(handler) };
    i32::from(handler(size) != 0)
}

pub extern "win64" fn _set_new_mode(mode: i32) -> i32 {
    if mode != 0 && mode != 1 {
        invalid(EINVAL);
        return -1;
    }
    NEW_MODE.swap(mode, Ordering::AcqRel)
}

/// As `_malloc_base`: while it fails, new mode is on and the handler
/// frees something, again; then null and ENOMEM.
fn with_new_handler(size: usize, allocate: impl Fn() -> *mut c_void) -> *mut c_void {
    loop {
        if size <= MAX_REQUEST {
            let block = allocate();
            if !block.is_null() {
                return block;
            }
        }
        if NEW_MODE.load(Ordering::Acquire) == 0 || _callnewh(size) == 0 {
            fail(ENOMEM);
            return std::ptr::null_mut();
        }
    }
}

pub extern "win64" fn malloc(size: usize) -> *mut c_void {
    // SAFETY: plain allocation.
    with_new_handler(size, || unsafe { libc::malloc(size.max(1)) })
}

pub extern "win64" fn free(block: *mut c_void) {
    // SAFETY: null or a block of this heap, per the C contract.
    unsafe { libc::free(block) }
}

pub extern "win64" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        fail(ENOMEM);
        return std::ptr::null_mut();
    };
    // SAFETY: plain allocation.
    with_new_handler(total, || unsafe { libc::calloc(1, total.max(1)) })
}

/// A size of 0 frees the block and returns null, as the UCRT's.
pub extern "win64" fn realloc(block: *mut c_void, size: usize) -> *mut c_void {
    if block.is_null() {
        return malloc(size);
    }
    if size == 0 {
        free(block);
        return std::ptr::null_mut();
    }
    // SAFETY: a block of this heap, per the C contract.
    with_new_handler(size, || unsafe { libc::realloc(block, size) })
}

/// The block's usable size, which can exceed what was asked for.
pub extern "win64" fn _msize(block: *mut c_void) -> usize {
    if block.is_null() {
        invalid(EINVAL);
        return usize::MAX;
    }
    // SAFETY: a block of this heap, per the C contract.
    unsafe { libc::malloc_usable_size(block) }
}

/// `_aligned_offset_malloc`: `block + offset` is aligned to `alignment`.
pub extern "win64" fn _aligned_offset_malloc(
    size: usize,
    alignment: usize,
    offset: usize,
) -> *mut c_void {
    const PTR: usize = 8;
    if !alignment.is_power_of_two() || (offset != 0 && offset >= size) {
        invalid(EINVAL);
        return std::ptr::null_mut();
    }
    let mask = alignment.max(PTR) - 1;
    let gap = offset.wrapping_neg() & (PTR - 1);
    let overhead = PTR + gap + mask;
    if size > usize::MAX - overhead {
        fail(ENOMEM);
        return std::ptr::null_mut();
    }
    let base = malloc(size + overhead);
    if base.is_null() {
        return base;
    }
    let user = ((base as usize + overhead + offset) & !mask) - offset;
    // SAFETY: the word below `user - gap` lies inside the block's overhead.
    unsafe { (((user - gap) & !(PTR - 1)) as *mut usize).sub(1).write(base as usize) };
    user as *mut c_void
}

pub extern "win64" fn _aligned_malloc(size: usize, alignment: usize) -> *mut c_void {
    _aligned_offset_malloc(size, alignment, 0)
}

pub extern "win64" fn _aligned_free(block: *mut c_void) {
    if block.is_null() {
        return;
    }
    // SAFETY: an `_aligned_*` block keeps its base in the word below.
    let base = unsafe { ((block as usize & !7) as *const usize).sub(1).read() };
    free(base as *mut c_void);
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "ucrt-heap",
        dlls: &["ucrtbase.dll", "api-ms-win-crt-heap-*"],
        exports: vec![
            hle_export!(_callnewh),
            hle_export!(_set_new_mode),
            hle_export!(malloc),
            hle_export!(free),
            hle_export!(calloc),
            hle_export!(realloc),
            hle_export!(_msize),
            hle_export!(_aligned_malloc),
            hle_export!(_aligned_offset_malloc),
            hle_export!(_aligned_free),
            crate::HleExport {
                name: "_malloc_base",
                address: malloc as *const () as usize,
            },
            crate::HleExport {
                name: "_calloc_base",
                address: calloc as *const () as usize,
            },
            crate::HleExport {
                name: "_realloc_base",
                address: realloc as *const () as usize,
            },
            crate::HleExport {
                name: "_free_base",
                address: free as *const () as usize,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::ucrt_runtime::errno;

    #[test]
    fn blocks_are_16_aligned_and_resize() {
        let p = malloc(3);
        assert_eq!(p as usize % 16, 0);
        assert!(_msize(p) >= 3);
        let p = realloc(p, 100);
        assert!(!p.is_null());
        assert!(realloc(p, 0).is_null());
        let z = calloc(4, 8).cast::<u64>();
        // SAFETY: 32 zeroed bytes.
        assert!(unsafe { std::slice::from_raw_parts(z, 4) }.iter().all(|&v| v == 0));
        free(z.cast());
        assert!(calloc(usize::MAX, 2).is_null());
        assert_eq!(errno(), ENOMEM);
        assert!(malloc(usize::MAX).is_null());
    }

    #[test]
    fn aligned_blocks_honour_alignment_and_offset() {
        for alignment in [1, 8, 16, 64, 4096] {
            let p = _aligned_malloc(100, alignment);
            assert_eq!(p as usize % alignment, 0);
            // SAFETY: 100 bytes of ours.
            unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0xAB, 100) };
            _aligned_free(p);
        }
        let p = _aligned_offset_malloc(64, 32, 4);
        assert_eq!((p as usize + 4) % 32, 0);
        _aligned_free(p);
        _aligned_free(std::ptr::null_mut());
        assert!(_aligned_malloc(8, 24).is_null());
        assert_eq!(errno(), EINVAL);
    }

    static CALLS: AtomicI32 = AtomicI32::new(0);
    extern "win64" fn handler(_: usize) -> i32 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        0 // freed nothing: the allocation gives up
    }

    #[test]
    fn new_mode_calls_the_new_handler() {
        let old = NEW_HANDLER.swap(handler as *const () as usize, Ordering::AcqRel);
        assert_eq!(_callnewh(1), 0);
        assert_eq!(_set_new_mode(1), 0);
        assert!(malloc(usize::MAX - 8).is_null());
        assert_eq!(_set_new_mode(0), 1);
        assert_eq!(_set_new_mode(5), -1);
        assert!(CALLS.load(Ordering::Relaxed) >= 2);
        NEW_HANDLER.store(old, Ordering::Release);
    }
}
