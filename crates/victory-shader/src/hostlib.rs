//! Host libraries on Linux, loaded at run time: the host D3D12
//! (vkd3d-proton built for Linux: `libvkd3d-proton-d3d12.so` and the
//! `libvkd3d-proton-d3d12core.so` it opens) and anything else we only
//! call through its C API. Their COM methods are SysV, which is what
//! `extern "system"` (and so the `windows` crate's vtables) means here.
//!
//! `VICTORY_VKD3D` names the directory holding vkd3d-proton's libraries,
//! or `libvkd3d-proton-d3d12.so` itself; unset, the dynamic loader's
//! search path is used (`docs/linux.md`, GPU).

use std::ffi::{CStr, CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const D3D12: &str = "libvkd3d-proton-d3d12.so";
const CORE: &str = "libvkd3d-proton-d3d12core.so";

/// A library opened for good (never closed).
#[derive(Clone, Copy)]
pub struct Library(usize);

impl Library {
    /// `path` (a file, or a name the dynamic loader searches for).
    pub fn open(path: &Path) -> Result<Library, String> {
        let name = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| format!("{}: NUL in path", path.display()))?;
        // SAFETY: a NUL-terminated path.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(last_error());
        }
        Ok(Library(handle as usize))
    }

    /// The address of export `name`.
    pub fn symbol(self, name: &CStr) -> Result<*mut c_void, String> {
        // SAFETY: an open library and a NUL-terminated name.
        let address = unsafe { libc::dlsym(self.0 as *mut c_void, name.as_ptr()) };
        if address.is_null() {
            return Err(format!("no {}", name.to_string_lossy()));
        }
        Ok(address)
    }
}

fn last_error() -> String {
    // SAFETY: dlerror returns null or a NUL-terminated message.
    let text = unsafe { libc::dlerror() };
    if text.is_null() {
        return "not found".into();
    }
    // SAFETY: as above.
    unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned()
}

/// vkd3d-proton's `libvkd3d-proton-d3d12.so`, opened once.
pub fn d3d12() -> Result<Library, &'static str> {
    static LIBRARY: OnceLock<Result<Library, String>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let (core, d3d12) = match std::env::var_os("VICTORY_VKD3D").map(PathBuf::from) {
                Some(p) if p.is_dir() => (p.join(CORE), p.join(D3D12)),
                Some(p) => (p.with_file_name(CORE), p),
                None => (CORE.into(), D3D12.into()),
            };
            // The d3d12 library opens the core by its soname, which an
            // installed copy (no run path) only finds already loaded.
            let opened = Library::open(&core).and_then(|_| Library::open(&d3d12));
            opened.map_err(|e| {
                format!(
                    "no host D3D12: cannot load vkd3d-proton ({e}); build it for Linux and \
                     set VICTORY_VKD3D to its library directory (docs/linux.md, GPU)"
                )
            })
        })
        .as_ref()
        .copied()
        .map_err(String::as_str)
}

/// The address of `name` in the host D3D12.
pub fn d3d12_export(name: &CStr) -> Result<*mut c_void, String> {
    d3d12()?.symbol(name)
}

/// `D3D12CreateDevice` on the default adapter: its HRESULT, or why the
/// host D3D12 is missing.
///
/// # Safety
///
/// `iid` and `device` as `D3D12CreateDevice` takes them.
pub unsafe fn create_device(
    level: i32,
    iid: *const c_void,
    device: *mut *mut c_void,
) -> Result<i32, String> {
    type Create =
        unsafe extern "system" fn(*mut c_void, i32, *const c_void, *mut *mut c_void) -> i32;
    let f = d3d12_export(c"D3D12CreateDevice")?;
    // SAFETY: the export has this signature; the caller's pointers.
    Ok(unsafe {
        std::mem::transmute::<*mut c_void, Create>(f)(std::ptr::null_mut(), level, iid, device)
    })
}

/// `D3D12SerializeVersionedRootSignature`: its HRESULT, or why the host
/// D3D12 is missing.
///
/// # Safety
///
/// The arguments as `D3D12SerializeVersionedRootSignature` takes them.
pub unsafe fn serialize_versioned_root_signature(
    desc: *const c_void,
    blob: *mut *mut c_void,
    error: *mut *mut c_void,
) -> Result<i32, String> {
    type Serialize =
        unsafe extern "system" fn(*const c_void, *mut *mut c_void, *mut *mut c_void) -> i32;
    let f = d3d12_export(c"D3D12SerializeVersionedRootSignature")?;
    // SAFETY: the export has this signature; the caller's pointers.
    Ok(unsafe { std::mem::transmute::<*mut c_void, Serialize>(f)(desc, blob, error) })
}

/// DXC's `libdxcompiler.so`, only at `VICTORY_DXCOMPILER` (Arch's
/// 1.10.2605 build asserts on every shader, so none is found unasked),
/// opened once.
fn dxcompiler() -> Result<Library, &'static str> {
    static LIBRARY: OnceLock<Result<Library, String>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            let path = std::env::var_os("VICTORY_DXCOMPILER")
                .ok_or("no HLSL compiler: set VICTORY_DXCOMPILER to DXC's libdxcompiler.so")?;
            Library::open(Path::new(&path)).map_err(|e| {
                format!(
                    "no HLSL compiler: cannot load {} ({e})",
                    Path::new(&path).display()
                )
            })
        })
        .as_ref()
        .copied()
        .map_err(String::as_str)
}

/// Compiles HLSL `source` with DXC (`dxc -E entry -T target -Vd`): the
/// DXIL container, or the compiler's messages. DXC's COM is SysV here and
/// its wide strings are 32-bit (`wchar_t`).
pub fn compile_hlsl(source: &str, entry: &str, target: &str) -> Result<Vec<u8>, String> {
    type Object = *mut *const usize;
    type Create = unsafe extern "system" fn(*const [u8; 16], *const [u8; 16], *mut Object) -> i32;
    type Compile = unsafe extern "system" fn(
        Object,
        *const [usize; 3],
        *const *const u32,
        u32,
        *mut c_void,
        *const [u8; 16],
        *mut Object,
    ) -> i32;
    type Get = unsafe extern "system" fn(Object, *mut i32) -> i32;
    type Blob = unsafe extern "system" fn(Object, *mut Object) -> i32;
    type Bytes = unsafe extern "system" fn(Object) -> *const u8;
    type Size = unsafe extern "system" fn(Object) -> usize;
    type Release = unsafe extern "system" fn(Object) -> u32;
    const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
        let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
        [
            a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5],
            d[6], d[7],
        ]
    }
    const CLSID_COMPILER: [u8; 16] = guid(
        0x73e2_2d93,
        0xe6ce,
        0x47f3,
        [0xb5, 0xbf, 0xf0, 0x66, 0x4f, 0x39, 0xc1, 0xb0],
    );
    const IID_COMPILER3: [u8; 16] = guid(
        0x228b_4687,
        0x5a6a,
        0x4730,
        [0x90, 0x0c, 0x97, 0x02, 0xb2, 0x20, 0x3f, 0x54],
    );
    const IID_RESULT: [u8; 16] = guid(
        0x5834_6cda,
        0xdde7,
        0x4497,
        [0x94, 0x61, 0x6f, 0x87, 0xaf, 0x5e, 0x06, 0x59],
    );
    /// The method in `slot` of `object`.
    unsafe fn method<F: Copy>(object: Object, slot: usize) -> F {
        // SAFETY: the caller names a slot of the object's interface.
        unsafe { std::mem::transmute_copy(&*(*object).add(slot)) }
    }
    let library = dxcompiler()?;
    // SAFETY: the export has this signature.
    let create: Create = unsafe { std::mem::transmute(library.symbol(c"DxcCreateInstance")?) };
    let wide = |s: &str| s.chars().map(u32::from).chain([0]).collect::<Vec<u32>>();
    let args: Vec<Vec<u32>> = ["-E", entry, "-T", target, "-Vd"].map(wide).into();
    let pointers: Vec<*const u32> = args.iter().map(|a| a.as_ptr()).collect();
    // `DxcBuffer`: pointer, size, encoding (`DXC_CP_UTF8`).
    let buffer = [source.as_ptr() as usize, source.len(), 65001];
    // SAFETY: IDxcCompiler3::Compile (slot 3) with live arguments; then
    // IDxcResult's GetStatus (3), GetResult (4) and GetErrorBuffer (5), and
    // IDxcBlob's GetBufferPointer (3) and GetBufferSize (4). Every object
    // we get is released.
    unsafe {
        let mut compiler: Object = std::ptr::null_mut();
        if create(&CLSID_COMPILER, &IID_COMPILER3, &mut compiler) < 0 || compiler.is_null() {
            return Err("DxcCreateInstance(compiler) failed".into());
        }
        let mut result: Object = std::ptr::null_mut();
        let hr = method::<Compile>(compiler, 3)(
            compiler,
            &buffer,
            pointers.as_ptr(),
            pointers.len() as u32,
            std::ptr::null_mut(),
            &IID_RESULT,
            &mut result,
        );
        method::<Release>(compiler, 2)(compiler);
        if hr < 0 || result.is_null() {
            return Err(format!("DXC Compile failed: {hr:#x}"));
        }
        let mut status = 0;
        method::<Get>(result, 3)(result, &mut status);
        let mut blob: Object = std::ptr::null_mut();
        let slot = if status < 0 { 5 } else { 4 };
        method::<Blob>(result, slot)(result, &mut blob);
        method::<Release>(result, 2)(result);
        let bytes = if blob.is_null() {
            Vec::new()
        } else {
            let at = method::<Bytes>(blob, 3)(blob);
            let len = method::<Size>(blob, 4)(blob);
            let bytes = std::slice::from_raw_parts(at, len).to_vec();
            method::<Release>(blob, 2)(blob);
            bytes
        };
        if status < 0 {
            return Err(String::from_utf8_lossy(&bytes)
                .trim_end_matches('\0')
                .trim()
                .into());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn hlsl_compiles_to_dxil() {
        let source = "RWByteAddressBuffer o : register(u0);\n\
                      [RootSignature(\"UAV(u0)\")] [numthreads(1, 1, 1)]\n\
                      void main() { o.Store(0, 7); }";
        match super::compile_hlsl(source, "main", "cs_6_0") {
            Ok(dxil) => assert_eq!(&dxil[..4], b"DXBC"),
            Err(e) if e.starts_with("no HLSL compiler") => eprintln!("{e}; skipped"),
            Err(e) => panic!("{e}"),
        }
        assert!(super::compile_hlsl("not hlsl", "main", "cs_6_0").is_err_and(|e| !e.is_empty()));
    }
}
