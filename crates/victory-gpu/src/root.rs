//! Root signatures.
//!
//! Serializing goes to the host's `D3D12SerializeRootSignature` (or its
//! versioned form): the description is laid out as on PC, and the blob it
//! returns works as the console's, whose two methods sit in the same slots.
//! The console's own flags (bit 24 and up) are dropped first. On Linux the
//! host's objects are SysV COM, so what it returns is copied into objects
//! of ours with the title's calling convention ([`title`]).
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;

use crate::object::{
    self, E_INVALIDARG, E_NOTIMPL, HRESULT, Iid, Object, ROOT_SIGNATURE, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

const ROOT_SIGNATURE_IIDS: [Iid; 3] = [
    guid(
        0xc54a6b66,
        0x72df,
        0x4ee8,
        [0x8b, 0xe5, 0xa9, 0x46, 0xa1, 0x42, 0x92, 0x14],
    ),
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

const CONSOLE_FLAGS: u32 = 0xff00_0000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Desc {
    parameter_count: u32,
    parameters: *const c_void,
    sampler_count: u32,
    samplers: *const c_void,
    flags: u32,
}

/// Every version's description starts with the fields of [`Desc`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionedDesc {
    version: u32,
    desc: Desc,
}

type Serialize =
    unsafe extern "system" fn(*const Desc, u32, *mut *mut c_void, *mut *mut c_void) -> HRESULT;
type SerializeVersioned =
    unsafe extern "system" fn(*const VersionedDesc, *mut *mut c_void, *mut *mut c_void) -> HRESULT;

/// An export of the host's `d3d12.dll` in System32 (Linux: vkd3d-proton),
/// looked up once.
fn host_export(cache: &OnceLock<Option<usize>>, name: &CStr) -> Option<usize> {
    *cache.get_or_init(|| {
        #[cfg(not(windows))]
        return match victory_shader::hostlib::d3d12_export(name) {
            Ok(f) => Some(f as usize),
            Err(e) => {
                tracing::warn!("{e}");
                None
            }
        };
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::LibraryLoader::{
                GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
            };
            let dll: Vec<u16> = "d3d12.dll".encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated name; System32 only.
            let module = unsafe {
                LoadLibraryExW(
                    dll.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            };
            if module.is_null() {
                return None;
            }
            // SAFETY: a loaded module and a NUL-terminated name.
            unsafe { GetProcAddress(module, name.as_ptr().cast()) }.map(|f| f as usize)
        }
    })
}

fn without_console_flags(desc: Desc) -> Desc {
    if desc.flags & CONSOLE_FLAGS != 0 {
        tracing::debug!(
            flags = format_args!("{:#x}", desc.flags),
            "console root signature flags dropped"
        );
    }
    Desc {
        flags: desc.flags & !CONSOLE_FLAGS,
        ..desc
    }
}

/// `d3d12_xs!D3D12SerializeRootSignature`.
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
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12SerializeRootSignature") else {
        tracing::warn!("no host d3d12.dll to serialize root signatures");
        return E_NOTIMPL;
    };
    // SAFETY: the export has this signature; it takes what the title passed.
    let hr = unsafe {
        std::mem::transmute::<usize, Serialize>(address)(
            &without_console_flags(desc),
            version,
            blob,
            error,
        )
    };
    #[cfg(not(windows))]
    // SAFETY: the host filled in (or left null) the title's out pointers.
    unsafe {
        title::blobs(blob, error)
    };
    hr
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
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12SerializeVersionedRootSignature") else {
        tracing::warn!("no host d3d12.dll to serialize root signatures");
        return E_NOTIMPL;
    };
    let desc = VersionedDesc {
        desc: without_console_flags(desc.desc),
        ..desc
    };
    // SAFETY: the export has this signature; it takes what the title passed.
    let hr =
        unsafe { std::mem::transmute::<usize, SerializeVersioned>(address)(&desc, blob, error) };
    #[cfg(not(windows))]
    // SAFETY: the host filled in (or left null) the title's out pointers.
    unsafe {
        title::blobs(blob, error)
    };
    hr
}

type Deserialize =
    unsafe extern "system" fn(*const u8, usize, *const Iid, *mut *mut c_void) -> HRESULT;

/// `d3d12_xs!D3D12CreateRootSignatureDeserializer`: our blobs are the
/// host's serialization, so the host's deserializer reads them, and its
/// object (IUnknown, then the description getter) is laid out as the
/// console's. Grand Theft Auto VI's upscaler asks with the PC interface id.
pub extern "win64" fn create_deserializer(
    blob: *const u8,
    size: usize,
    iid: *const Iid,
    out: *mut *mut c_void,
) -> HRESULT {
    if blob.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    static FUNCTION: OnceLock<Option<usize>> = OnceLock::new();
    let Some(address) = host_export(&FUNCTION, c"D3D12CreateRootSignatureDeserializer") else {
        tracing::warn!("no host d3d12.dll to deserialize root signatures");
        return E_NOTIMPL;
    };
    // SAFETY: the export has this signature; it takes what the title passed.
    let hr = unsafe { std::mem::transmute::<usize, Deserialize>(address)(blob, size, iid, out) };
    #[cfg(not(windows))]
    // SAFETY: the host filled in (or left null) the title's out pointer.
    unsafe {
        title::deserializer(out)
    };
    hr
}

/// The host's blobs and deserializers as objects the title can call
/// (`win64`), on Linux where the host's are SysV.
#[cfg(not(windows))]
mod title {
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
                std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
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
}

#[repr(C)]
struct RootSignature {
    object: Object,
    /// The serialized form, kept for translating pipelines later.
    blob: Vec<u8>,
}

/// Device method.
pub(crate) extern "win64" fn create(
    _: This,
    _nodes: u32,
    blob: *const u8,
    size: usize,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if blob.is_null() || size == 0 {
        return E_INVALIDARG;
    }
    // SAFETY: the title passes `size` bytes.
    let blob = unsafe { std::slice::from_raw_parts(blob, size) }.to_vec();
    tracing::debug!(size, "root signature");
    let root = Box::new(RootSignature {
        object: Object::new(&FUNCTIONS),
        blob,
    });
    object::hand_out(ROOT_SIGNATURE, root, iid, &ROOT_SIGNATURE_IIDS, out)
}

extern "win64" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &ROOT_SIGNATURE_IIDS).then_some(this);
    object::answer(ROOT_SIGNATURE, iid, found, out)
}
extern "win64" fn release(this: This) -> u32 {
    object::release_boxed::<RootSignature>(this)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<ROOT_SIGNATURE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
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
}
