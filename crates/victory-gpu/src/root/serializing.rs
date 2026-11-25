//! Our own serializers and deserializer.
//!
//! These used to be reached through the export of the host's `d3d12.dll` in
//! System32 (on Linux, vkd3d-proton). That made the root-signature path
//! depend on a library a Linux machine need not have: the boot died when
//! `D3D12SerializeVersionedRootSignature` answered `E_NOTIMPL` and the title
//! threw. Both endpoints are ours ([`super::own`]), so the bytes in between
//! are ours as well.

use std::ffi::c_void;

use super::*;
use super::own;

/// The size of a D3D12 root parameter as the title passes it (the union
/// makes a table four dwords, a constant one and a descriptor one).
const PARAMETER_SIZE: usize = 16;
/// `D3D12_SAMPLER_DESC`: filter, four addresses, comparison, four bounds.
const SAMPLER_SIZE: usize = 52;

/// `d3d12_xs!D3D12SerializeRootSignature`. Ignoring `version` is correct:
/// the description is stored as given, and the only reader is our own
/// deserializer.
pub extern "win64" fn serialize(
    desc: *const Desc,
    version: u32,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let bytes = unsafe { own::bytes(&desc, version, PARAMETER_SIZE, SAMPLER_SIZE) };
    tracing::debug!(
        bytes = bytes.len(),
        parameters = desc.parameter_count,
        version,
        "root signature serialized"
    );
    // SAFETY: the title's out pointers, which it may have left null.
    unsafe { title::hand(&bytes, blob, error) };
    S_OK
}

/// `d3d12_xs!D3D12SerializeVersionedRootSignature`.
pub extern "win64" fn serialize_versioned(
    desc: *const VersionedDesc,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let bytes = unsafe { own::bytes(&desc.desc, desc.version, PARAMETER_SIZE, SAMPLER_SIZE) };
    tracing::debug!(
        bytes = bytes.len(),
        parameters = desc.desc.parameter_count,
        version = desc.version,
        "versioned root signature serialized"
    );
    // SAFETY: the title's out pointers, which it may have left null.
    unsafe { title::hand(&bytes, blob, error) };
    S_OK
}

/// `d3d12_xs!D3D12CreateRootSignatureDeserializer`: the blob is ours, so the
/// description it carries is the one the title wrote, read straight back.
pub extern "win64" fn create_deserializer(
    blob: *const u8,
    size: usize,
    iid: *const Iid,
    out: *mut *mut c_void,
) -> HRESULT {
    if blob.is_null() || out.is_null() || size == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes its blob and its length.
    let bytes = unsafe { std::slice::from_raw_parts(blob, size) };
    if !own::is_ours(bytes) {
        // A blob from the host, if one ever reaches us: nothing to read.
        tracing::warn!(size, "root signature blob is not ours");
        return E_INVALIDARG;
    }
    // SAFETY: `out` is the title's writable pointer.
    unsafe { title::deserialize(bytes, iid, out) };
    S_OK
}
