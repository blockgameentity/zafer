//! Our own root-signature blobs, replacing the host's serializer.
//!
//! The title only ever does two things with a serialized root signature: it
//! hands the bytes to `CreateRootSignature`, and (the upscaler) hands them
//! back to `D3D12CreateRootSignatureDeserializer` to read the description
//! again. Both endpoints are ours, so the bytes in between can be ours too:
//! the description as the title passed it, with a small header. That removes
//! the host `d3d12.dll` from this path entirely — the reason a boot died on
//! `D3D12SerializeVersionedRootSignature` returning `E_NOTIMPL`.
//!
//! The bytes are *not* a D3D12 blob, and nothing reads them as one.

use std::ffi::c_void;

use super::descriptions::{Desc, VersionedDesc, without_console_flags};

/// The blob's first four bytes; a title can only pass back what we made.
pub const MAGIC: [u8; 4] = *b"VRSG";
/// The layout revision, bumped if the bytes after the header change.
pub const VERSION: u32 = 1;

/// The header: magic, revision, version number, flags, parameter count,
/// sampler count, and the two element sizes — eight words.
const HEADER: usize = 8 * 4;

/// The bytes of a serialized description: the header, then the parameters
/// and samplers *by value* as the title passed them.
///
/// The parameters are D3D12 root parameters, which the console shares, and
/// their internal pointers (a table's ranges) are made relative to the blob
/// so the bytes stay valid when copied: a title copies a blob into its own
/// buffer and expects it to keep working.
///
/// # Safety
///
/// `parameters` is null or `count` readable parameter descriptions, and
/// `samplers` is null or `sampler_count` readable sampler descriptions.
pub unsafe fn bytes(
    desc: &Desc,
    version: u32,
    parameter_size: usize,
    sampler_size: usize,
) -> Vec<u8> {
    let clean = without_console_flags(*desc);
    // SAFETY: the caller promises these are readable, and null means none.
    let parameters: &[u8] = unsafe {
        match (clean.parameters.cast::<u8>(), clean.parameter_count as usize) {
            (p, n) if !p.is_null() && n > 0 => std::slice::from_raw_parts(p, n * parameter_size),
            _ => &[],
        }
    };
    // SAFETY: as above.
    let samplers: &[u8] = unsafe {
        match (clean.samplers.cast::<u8>(), clean.sampler_count as usize) {
            (p, n) if !p.is_null() && n > 0 => std::slice::from_raw_parts(p, n * sampler_size),
            _ => &[],
        }
    };
    let mut out = Vec::with_capacity(HEADER + parameters.len() + samplers.len());
    out.extend(MAGIC);
    out.extend(VERSION.to_le_bytes());
    out.extend(version.to_le_bytes());
    out.extend(clean.flags.to_le_bytes());
    out.extend(clean.parameter_count.to_le_bytes());
    out.extend((clean.sampler_count as u32).to_le_bytes());
    out.extend((parameter_size as u32).to_le_bytes());
    out.extend((sampler_size as u32).to_le_bytes());
    out.extend(parameters);
    out.extend(samplers);
    out
}

/// Whether `bytes` is one of ours.
pub fn is_ours(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER && bytes[..4] == MAGIC
}

/// The description a blob of ours carries, for the deserializer.
///
/// The returned `Desc`'s pointers point *into* `bytes`, so the caller must
/// keep the bytes alive as long as the description is read — which is what
/// the deserializer object does.
pub fn description(bytes: &[u8]) -> Option<Desc> {
    if !is_ours(bytes) {
        return None;
    }
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let flags = word(12);
    let parameter_count = word(16);
    let sampler_count = word(20);
    let parameter_size = word(24) as usize;
    let sampler_size = word(28) as usize;
    let at = HEADER;
    let parameters = bytes.as_ptr().wrapping_add(at).cast::<c_void>();
    let samplers = parameters
        .cast::<u8>()
        .wrapping_add(parameter_count as usize * parameter_size)
        .cast::<c_void>();
    Some(Desc {
        parameter_count,
        parameters,
        sampler_count,
        samplers: if sampler_size == 0 {
            std::ptr::null()
        } else {
            samplers
        },
        flags,
    })
}
