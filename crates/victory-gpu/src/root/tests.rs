//! The host round trips: serialize here, then read back or create.
use super::*;
use crate::object::{S_OK, slot};

/// Without vkd3d-proton (Linux) there is no host to serialize on.
fn no_host() -> bool {
    #[cfg(not(windows))]
    if let Err(e) = victory_shader::hostlib::d3d12() {
        eprintln!("{e}; skipped");
        return true;
    }
    false
}

/// One 32-bit constant at b0, serialized by the host, then created.
#[test]
fn host_serialized_blobs_create_root_signatures() {
    if no_host() {
        return;
    }
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_ROOT_CONSTANTS, D3D12_ROOT_PARAMETER, D3D12_ROOT_PARAMETER_0,
        D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS, D3D12_SHADER_VISIBILITY_ALL,
    };
    let parameter = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: 0,
                RegisterSpace: 0,
                Num32BitValues: 1,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    let desc = Desc {
        parameter_count: 1,
        parameters: (&raw const parameter).cast(),
        sampler_count: 0,
        samplers: std::ptr::null(),
        flags: 0x0100_0001,
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize(&desc, 1, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    let data = slot::<extern "win64" fn(This) -> *const u8>(blob, 0x18)(blob);
    let size = slot::<extern "win64" fn(This) -> usize>(blob, 0x20)(blob);
    assert!(size > 0);
    let null = std::ptr::null();
    let mut root = null;
    assert_eq!(
        create(null, 0, data, size, &ROOT_SIGNATURE_IIDS[0], &mut root),
        S_OK
    );
    assert_eq!(slot::<extern "win64" fn(This) -> u32>(root, 0x10)(root), 0);
    slot::<extern "win64" fn(This) -> u32>(blob, 0x10)(blob);
}

/// The host's deserializer reads our blobs back (the upscaler's call).
#[test]
fn our_blobs_deserialize_on_the_host() {
    if no_host() {
        return;
    }
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_ROOT_SIGNATURE_DESC, ID3D12RootSignatureDeserializer,
    };
    use windows::core::Interface;
    let desc = VersionedDesc {
        version: 1,
        desc: Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 1,
        },
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    let data = slot::<extern "win64" fn(This) -> *const u8>(blob, 0x18)(blob);
    let size = slot::<extern "win64" fn(This) -> usize>(blob, 0x20)(blob);
    let iid: Iid = ID3D12RootSignatureDeserializer::IID.to_u128().to_be_bytes();
    let iid = guid(
        u32::from_be_bytes(iid[0..4].try_into().unwrap()),
        u16::from_be_bytes(iid[4..6].try_into().unwrap()),
        u16::from_be_bytes(iid[6..8].try_into().unwrap()),
        iid[8..16].try_into().unwrap(),
    );
    let mut out = std::ptr::null_mut();
    assert_eq!(create_deserializer(data, size, &iid, &mut out), S_OK);
    // As the title calls it: the description getter, then Release.
    let out = out.cast_const().cast::<Object>();
    let get = slot::<extern "win64" fn(This) -> *const D3D12_ROOT_SIGNATURE_DESC>(out, 0x18);
    // SAFETY: valid while the deserializer lives.
    let read = unsafe { &*get(out) };
    assert_eq!((read.NumParameters, read.Flags.0), (0, 1));
    assert_eq!(slot::<extern "win64" fn(This) -> u32>(out, 0x10)(out), 0);
    slot::<extern "win64" fn(This) -> u32>(blob, 0x10)(blob);
}

/// Version 1.0 through the versioned entry point, with a console flag.
#[test]
fn versioned_descriptions_serialize_on_the_host() {
    if no_host() {
        return;
    }
    let desc = VersionedDesc {
        version: 1,
        desc: Desc {
            parameter_count: 0,
            parameters: std::ptr::null(),
            sampler_count: 0,
            samplers: std::ptr::null(),
            flags: 0x0200_0001,
        },
    };
    let (mut blob, mut error) = (std::ptr::null_mut(), std::ptr::null_mut());
    assert_eq!(serialize_versioned(&desc, &mut blob, &mut error), S_OK);
    let blob = blob.cast_const().cast::<Object>();
    assert!(slot::<extern "win64" fn(This) -> usize>(blob, 0x20)(blob) > 0);
    slot::<extern "win64" fn(This) -> u32>(blob, 0x10)(blob);
    assert_eq!(
        serialize_versioned(std::ptr::null(), &mut blob.cast_mut().cast(), &mut error),
        E_INVALIDARG
    );
}
