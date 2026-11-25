//! Our serialization, round-tripped without a host `d3d12.dll`.
//!
//! These are the tests that matter for the Linux boot: the old path could
//! only be exercised on a machine with vkd3d-proton, so the boot-time failure
//! (`E_NOTIMPL`) had nothing pinning it.

use super::descriptions::{Desc, VersionedDesc};
use super::own;

/// A description with `count` 16-byte parameters, all zero.
fn description(parameters: &[[u32; 4]]) -> Desc {
    Desc {
        parameter_count: parameters.len() as u32,
        parameters: parameters.as_ptr().cast(),
        sampler_count: 0,
        samplers: std::ptr::null(),
        flags: 0,
    }
}

#[test]
fn a_blob_is_ours_and_reads_back_its_description() {
    let parameters = [[0u32, 1, 2, 3], [4, 5, 6, 7]];
    let desc = description(&parameters);
    // SAFETY: the description's pointers are to live locals.
    let bytes = unsafe { own::bytes(&desc, 2, 16, 52) };

    assert!(own::is_ours(&bytes), "a blob of ours is recognisable");
    let read = own::description(&bytes).expect("a description");
    assert_eq!(read.parameter_count, 2);
    assert_eq!(read.sampler_count, 0);
    assert_eq!(read.flags, 0);

    // The parameters came back byte for byte, at the blob's end.
    // SAFETY: the description's pointers are into `bytes`, which is alive.
    let back = unsafe {
        std::slice::from_raw_parts(read.parameters.cast::<u32>(), read.parameter_count as usize * 4)
    };
    assert_eq!(back, parameters.concat().as_slice());
}

#[test]
fn a_blob_that_is_not_ours_is_refused() {
    assert!(!own::is_ours(b"DXBC"));
    assert!(!own::is_ours(b""));
    assert!(own::description(b"DXBC not a blob").is_none());
}

#[test]
fn console_flags_are_dropped_from_the_blob() {
    let parameters = [[0u32, 0, 0, 0]];
    let mut desc = description(&parameters);
    desc.flags = 0x0100_0002;
    // SAFETY: the description's pointers are to live locals.
    let bytes = unsafe { own::bytes(&desc, 1, 16, 52) };
    let read = own::description(&bytes).expect("a description");
    assert_eq!(read.flags, 0x0000_0002, "console-only bits are not stored");
}

#[test]
fn an_empty_description_still_serializes() {
    let desc = Desc {
        parameter_count: 0,
        parameters: std::ptr::null(),
        sampler_count: 0,
        samplers: std::ptr::null(),
        flags: 0,
    };
    // SAFETY: no parameters or samplers, so no pointers are read.
    let bytes = unsafe { own::bytes(&desc, 1, 16, 52) };
    assert!(own::is_ours(&bytes));
    let read = own::description(&bytes).expect("a description");
    assert_eq!(read.parameter_count, 0);
    assert_eq!(read.sampler_count, 0);
}

#[test]
fn a_versioned_description_carries_its_version() {
    let parameters = [[0u32; 4]];
    let desc = description(&parameters);
    let versioned = VersionedDesc { version: 3, desc };
    // SAFETY: the description's pointers are to live locals.
    let bytes = unsafe { own::bytes(&versioned.desc, versioned.version, 16, 52) };
    // The version is the blob's own third word.
    let stored = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    assert_eq!(stored, 3);
}
