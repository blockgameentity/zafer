//! The host's blobs and deserializers as objects the title can call
//! (`win64`), on Linux where the host's are SysV.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::ID3D12RootSignatureDeserializer;
use windows::core::Interface;

use crate::object::{HRESULT, S_OK};

/// IUnknown, then the object's one getter (the blob's two).
#[repr(C)]
struct Vtable {
    query: extern "win64" fn(*mut Title, *const c_void, *mut *mut c_void) -> HRESULT,
    add_ref: extern "win64" fn(*mut Title) -> u32,
    release: extern "win64" fn(*mut Title) -> u32,
    first: extern "win64" fn(*mut Title) -> *const c_void,
    second: extern "win64" fn(*mut Title) -> usize,
}

#[repr(C)]
struct Title {
    vtable: &'static Vtable,
    refs: AtomicU32,
    /// The blob's bytes; empty for a deserializer.
    bytes: Vec<u8>,
    /// The deserializer's description, which `host` owns.
    desc: *const c_void,
    _host: Option<ID3D12RootSignatureDeserializer>,
}

extern "win64" fn query(this: *mut Title, _: *const c_void, out: *mut *mut c_void) -> HRESULT {
    // Any interface: the title only asks for the one it was given.
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
    // SAFETY: as above.
    unsafe { (*this).desc }
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

fn hand_out(title: Title) -> *mut c_void {
    Box::into_raw(Box::new(title)).cast()
}

/// Replaces the host blobs at `blob` and `error` (each null or not)
/// with copies of ours.
///
/// # Safety
///
/// Each is null or points at null or a host blob reference, now ours.
pub(super) unsafe fn blobs(blob: *mut *mut c_void, error: *mut *mut c_void) {
    for at in [blob, error] {
        // SAFETY: the caller's pointers.
        let Some(host) = (unsafe { at.as_mut() }).filter(|h| !h.is_null()) else {
            continue;
        };
        // SAFETY: a host blob reference, now released by the drop.
        let blob = unsafe { ID3DBlob::from_raw(*host) };
        // SAFETY: the blob's own bytes.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            )
        };
        *host = hand_out(Title {
            vtable: &BLOB,
            refs: AtomicU32::new(1),
            bytes: bytes.to_vec(),
            desc: std::ptr::null(),
            _host: None,
        });
    }
}

/// Replaces the host deserializer at `out` with one of ours.
///
/// # Safety
///
/// As for [`blobs`], with a deserializer.
pub(super) unsafe fn deserializer(out: *mut *mut c_void) {
    // SAFETY: the caller's pointer.
    let Some(host) = (unsafe { out.as_mut() }).filter(|h| !h.is_null()) else {
        return;
    };
    // SAFETY: a host deserializer reference, now held by our object.
    let host_object = unsafe { ID3D12RootSignatureDeserializer::from_raw(*host) };
    // SAFETY: a plain getter; the description lives as long as the object.
    let desc = unsafe { host_object.GetRootSignatureDesc() }.cast();
    *host = hand_out(Title {
        vtable: &DESERIALIZER,
        refs: AtomicU32::new(1),
        bytes: Vec::new(),
        desc,
        _host: Some(host_object),
    });
}
