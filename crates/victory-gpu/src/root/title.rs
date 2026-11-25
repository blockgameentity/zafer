//! Our blobs and deserializers as objects the title can call (`win64`).
//!
//! These used to wrap a host object and copy its bytes out. Now the bytes are
//! ours from the start ([`super::own`]), so an object here owns a `Vec<u8>`
//! and, for a deserializer, the description pointing into it.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use super::own;
use crate::object::{HRESULT, Iid, S_OK};

/// IUnknown, then the object's accessors (a blob's two, a deserializer's one).
#[repr(C)]
struct Vtable {
    query: extern "win64" fn(*mut Title, *const Iid, *mut *mut c_void) -> HRESULT,
    add_ref: extern "win64" fn(*mut Title) -> u32,
    release: extern "win64" fn(*mut Title) -> u32,
    first: extern "win64" fn(*mut Title) -> *const c_void,
    second: extern "win64" fn(*mut Title) -> usize,
}

#[repr(C)]
struct Title {
    vtable: &'static Vtable,
    refs: AtomicU32,
    /// The blob's bytes, or the bytes a deserializer reads its description
    /// from. Owned here, so `description`'s pointers stay valid.
    bytes: Vec<u8>,
}

extern "win64" fn query(this: *mut Title, _: *const Iid, out: *mut *mut c_void) -> HRESULT {
    add_ref(this);
    // SAFETY: the title's out pointer.
    unsafe { *out = this.cast() };
    S_OK
}

extern "win64" fn add_ref(this: *mut Title) -> u32 {
    // SAFETY: a live object of ours.
    unsafe { (*this).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

extern "win64" fn release(this: *mut Title) -> u32 {
    // SAFETY: a live object of ours; the last reference frees it.
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left
    }
}

extern "win64" fn pointer(this: *mut Title) -> *const c_void {
    // SAFETY: a live object of ours.
    unsafe { (*this).bytes.as_ptr().cast() }
}

extern "win64" fn size(this: *mut Title) -> usize {
    // SAFETY: as above.
    unsafe { (*this).bytes.len() }
}

extern "win64" fn desc(this: *mut Title) -> *const c_void {
    // SAFETY: a live object of ours, whose bytes outlive the description.
    unsafe { own::description(&(*this).bytes) }
        .map(|d| &d as *const _ as *const c_void)
        .unwrap_or(std::ptr::null())
}

static BLOB: Vtable = Vtable {
    query,
    add_ref,
    release,
    first: pointer,
    second: size,
};

static DESERIALIZER: Vtable = Vtable {
    query,
    add_ref,
    release,
    first: desc,
    second: size,
};

fn hand_out(blob: Title) -> *mut c_void {
    Box::into_raw(Box::new(blob)).cast()
}

/// Replaces the title's out pointers with blobs of ours carrying `bytes`.
///
/// # Safety
///
/// Each is null or points at a writable `*mut c_void`.
pub(super) unsafe fn hand(bytes: &[u8], blob: *mut *mut c_void, error: *mut *mut c_void) {
    for at in [blob, error] {
        // SAFETY: the caller's pointers.
        let Some(out) = (unsafe { at.as_mut() }) else {
            continue;
        };
        *out = hand_out(Title {
            vtable: &BLOB,
            refs: AtomicU32::new(1),
            bytes: bytes.to_vec(),
        });
    }
}

/// Writes a deserializer of ours over `bytes` at `out`.
///
/// # Safety
///
/// `out` is writable, and `bytes` outlives nothing on our side: it is copied.
pub(super) unsafe fn deserialize(bytes: &[u8], _iid: *const Iid, out: *mut *mut c_void) {
    // SAFETY: the caller's pointer.
    unsafe {
        *out = hand_out(Title {
            vtable: &DESERIALIZER,
            refs: AtomicU32::new(1),
            bytes: bytes.to_vec(),
        })
    };
}
