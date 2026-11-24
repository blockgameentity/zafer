//! The Vulkan loader, opened at run time.
//!
//! Vulkan has no stable link-time entry point: the loader is a library the
//! host provides (`libvulkan.so.1`, `vulkan-1.dll`) and everything goes
//! through `vkGetInstanceProcAddr`. So it is opened the way vkd3d-proton is
//! ([`victory_shader::hostlib`]): `dlopen`/`LoadLibrary` at run time, and a
//! clear error when it is not there, so a machine without a driver still
//! runs the D3D12 backend and the tests.
#![allow(dead_code)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

/// The global entry points the loader must have.
pub struct Loader {
    pub create_instance: unsafe extern "system" fn(*const c_void, *const c_void, *mut *mut c_void) -> i32,
    pub get_instance_proc_addr: unsafe extern "system" fn(*mut c_void, *const c_char) -> *mut c_void,
    pub enumerate_instance_extension_properties:
        unsafe extern "system" fn(*const c_char, *mut u32, *mut c_void) -> i32,
    /// Vulkan 1.1 and later; absent on an ancient loader.
    pub enumerate_instance_version: Option<unsafe extern "system" fn(*mut u32) -> i32>,
    /// Kept open for the life of the process (the loader's handles stay
    /// valid while any of its functions is still called).
    _library: Library,
}

/// One opened shared library, closed when the process ends.
struct Library(*mut c_void);

// SAFETY: the handle is only used to look symbols up, and the library is
// never closed while the process runs (`Drop` does not unload it either).
unsafe impl Send for Library {}
// SAFETY: as above.
unsafe impl Sync for Library {}

impl Drop for Library {
    fn drop(&mut self) {
        // Deliberately leaked: `vkGetInstanceProcAddr` results outlive any
        // `Loader` clone and a driver is not safe to unload.
    }
}

static LOADER: OnceLock<Result<Loader, String>> = OnceLock::new();

impl Loader {
    /// The loader, or why there is none (cached: opening it twice would
    /// run the driver's constructors twice).
    pub fn get() -> Result<&'static Loader, &'static str> {
        match LOADER.get_or_init(Self::open) {
            Ok(l) => Ok(l),
            Err(e) => Err(e.as_str()),
        }
    }

    fn open() -> Result<Loader, String> {
        let library = open_library()?;
        // SAFETY: the names are NUL-terminated and the loader exports them.
        unsafe {
            let create_instance = symbol(library.0, c"vkCreateInstance")?;
            let get_instance_proc_addr = symbol(library.0, c"vkGetInstanceProcAddr")?;
            let enumerate = symbol(library.0, c"vkEnumerateInstanceExtensionProperties")?;
            let version = symbol(library.0, c"vkEnumerateInstanceVersion").ok();
            Ok(Loader {
                create_instance: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(*const c_void, *const c_void, *mut *mut c_void) -> i32,
                >(create_instance),
                get_instance_proc_addr: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(*mut c_void, *const c_char) -> *mut c_void,
                >(get_instance_proc_addr),
                enumerate_instance_extension_properties: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "system" fn(*const c_char, *mut u32, *mut c_void) -> i32,
                >(enumerate),
                enumerate_instance_version: version.map(|v| {
                    std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "system" fn(*mut u32) -> i32,
                    >(v)
                }),
                _library: library,
            })
        }
    }
}

/// Opens the loader.
#[cfg(unix)]
fn open_library() -> Result<Library, String> {
    // The 1-suffix name first: the runtime package usually has no
    // unversioned symlink.
    for name in ["libvulkan.so.1", "libvulkan.so"] {
        let Ok(name) = std::ffi::CString::new(name) else {
            continue;
        };
        // SAFETY: a NUL-terminated path; the handle is ours.
        let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if !handle.is_null() {
            return Ok(Library(handle));
        }
    }
    Err("no Vulkan loader (libvulkan.so.1); install a Vulkan driver".into())
}

/// Opens the loader.
#[cfg(windows)]
fn open_library() -> Result<Library, String> {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
    let name: Vec<u16> = "vulkan-1.dll\0".encode_utf16().collect();
    // SAFETY: a NUL-terminated name; a null return is a missing library.
    let handle = unsafe { LoadLibraryW(name.as_ptr()) };
    if handle.is_null() {
        return Err("no Vulkan loader (vulkan-1.dll); install a Vulkan driver".into());
    }
    let _ = FreeLibrary; // the library is kept open for the process
    Ok(Library(handle as *mut c_void))
}

/// Looks one symbol up in an opened library.
///
/// # Safety
/// `name` is NUL-terminated.
#[cfg(unix)]
unsafe fn symbol(library: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: an open handle and a NUL-terminated name.
    let found = unsafe { libc::dlsym(library, name.as_ptr()) };
    if found.is_null() {
        Err(format!("the Vulkan loader has no {}", name.to_string_lossy()))
    } else {
        Ok(found)
    }
}

/// Looks one symbol up in an opened library.
///
/// # Safety
/// `name` is NUL-terminated.
#[cfg(windows)]
unsafe fn symbol(library: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    let bytes = name.to_bytes();
    let mut wide: Vec<u16> = bytes.iter().map(|&b| u16::from(b)).collect();
    wide.push(0);
    // SAFETY: an open module and a NUL-terminated name.
    let found = unsafe { GetProcAddress(library as *mut c_void as _, wide.as_ptr()) };
    match found {
        Some(f) => Ok(f as *mut c_void),
        None => Err(format!("the Vulkan loader has no {}", name.to_string_lossy())),
    }
}
