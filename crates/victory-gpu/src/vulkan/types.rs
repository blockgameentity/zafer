//! The Vulkan backend's FFI types.
//!
//! Hand-written `#[repr(C)]` layouts and the constants the loader and
//! instance code use. Vulkan 0.62 does not build on Linux (the `windows`
//! crate), so the bindings are generated for Windows only
//! (`crates/victory-windows`); this module is the narrow, explicit
//! alternative for Vulkan and nothing more. Sizes are asserted so a wrong
//! layout cannot pass silently.
#![allow(dead_code)]

use std::ffi::{c_char, c_void};

/// `VK_SUCCESS`.
pub const VK_SUCCESS: i32 = 0;
/// `VK_ERROR_EXTENSION_NOT_PRESENT`.
pub const VK_ERROR_EXTENSION_NOT_PRESENT: i32 = -7;

/// `VK_KHR_SURFACE`'s and friends' names, as the C strings the API takes.
pub const VK_KHR_SURFACE_EXTENSION_NAME: &[u8] = b"VK_KHR_surface\0";
#[cfg(unix)]
pub const VK_KHR_XLIB_SURFACE_EXTENSION_NAME: &[u8] = b"VK_KHR_xlib_surface\0";
#[cfg(unix)]
pub const VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME: &[u8] = b"VK_KHR_wayland_surface\0";
#[cfg(windows)]
pub const VK_KHR_WIN32_SURFACE_EXTENSION_NAME: &[u8] = b"VK_KHR_win32_surface\0";

/// `VK_STRUCTURE_TYPE_APPLICATION_INFO`.
pub const VK_STRUCTURE_TYPE_APPLICATION_INFO: u32 = 0;
/// `VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO`.
pub const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: u32 = 1;
/// `VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO`.
pub const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: u32 = 2;
/// `VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO`.
pub const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: u32 = 3;

/// `VK_MAKE_API_VERSION(0, major, minor, 0)`.
pub const fn api_version(major: u32, minor: u32) -> u32 {
    major << 22 | minor << 12
}

/// The major, minor and patch of a packed `VkApiVersion`.
pub fn split_api_version(version: u32) -> (u32, u32, u32) {
    (
        (version >> 22) & 0x7F,
        (version >> 12) & 0x3FF,
        version & 0xFFF,
    )
}

/// `VkApplicationInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkApplicationInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub p_application_name: *const c_char,
    pub application_version: u32,
    pub p_engine_name: *const c_char,
    pub engine_version: u32,
    pub api_version: u32,
}
const _: () = assert!(size_of::<VkApplicationInfo>() == 48);

/// `VkInstanceCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkInstanceCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub p_application_info: *const VkApplicationInfo,
    pub enabled_layer_count: u32,
    pub pp_enabled_layer_names: *const *const c_char,
    pub enabled_extension_count: u32,
    pub pp_enabled_extension_names: *const *const c_char,
}
const _: () = assert!(size_of::<VkInstanceCreateInfo>() == 64);

/// `VkDeviceQueueCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDeviceQueueCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub queue_family_index: u32,
    pub queue_count: u32,
    pub p_queue_priorities: *const f32,
}
const _: () = assert!(size_of::<VkDeviceQueueCreateInfo>() == 40);

/// `VkDeviceCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDeviceCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub queue_create_info_count: u32,
    pub p_queue_create_infos: *const VkDeviceQueueCreateInfo,
    pub enabled_layer_count: u32,
    pub pp_enabled_layer_names: *const *const c_char,
    pub enabled_extension_count: u32,
    pub pp_enabled_extension_names: *const *const c_char,
    pub p_enabled_features: *const VkPhysicalDeviceFeatures,
}
const _: () = assert!(size_of::<VkDeviceCreateInfo>() == 72);

/// `VkPhysicalDeviceFeatures`: 55 `VkBool32` flags, in the header's order.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPhysicalDeviceFeatures {
    pub flags: [u32; 55],
}

impl Default for VkPhysicalDeviceFeatures {
    fn default() -> Self {
        VkPhysicalDeviceFeatures { flags: [0; 55] }
    }
}
const _: () = assert!(size_of::<VkPhysicalDeviceFeatures>() == 220);

/// `VkPhysicalDeviceLimits`'s size; only ever copied as bookkeeping.
pub const PHYSICAL_DEVICE_LIMITS_SIZE: usize = 504;

/// `VkPhysicalDeviceProperties`.
#[repr(C)]
pub struct VkPhysicalDeviceProperties {
    pub api_version: u32,
    pub driver_version: u32,
    pub vendor_id: u32,
    pub device_id: u32,
    pub device_type: u32,
    pub device_name: [c_char; 256],
    /// The header's padding before `limits` (alignment of its 64-bit
    /// members).
    pub gap: [u8; 20],
    /// `VkPhysicalDeviceLimits`, kept as bytes: nothing reads inside it yet.
    pub limits: [u8; PHYSICAL_DEVICE_LIMITS_SIZE],
    /// `VkPhysicalDeviceSparseProperties`.
    pub sparse_properties: [u32; 5],
    /// The header's tail padding.
    pub tail: [u8; 4],
}
const _: () = assert!(size_of::<VkPhysicalDeviceProperties>() == 824);
const _: () = assert!(std::mem::offset_of!(VkPhysicalDeviceProperties, device_name) == 20);
const _: () = assert!(std::mem::offset_of!(VkPhysicalDeviceProperties, limits) == 296);
const _: () = assert!(std::mem::offset_of!(VkPhysicalDeviceProperties, sparse_properties) == 800);

/// `VkExtensionProperties`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtensionProperties {
    pub extension_name: [c_char; 256],
    pub spec_version: u32,
}
const _: () = assert!(size_of::<VkExtensionProperties>() == 260);

/// `VkQueueFamilyProperties`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkQueueFamilyProperties {
    pub queue_flags: u32,
    pub queue_count: u32,
    pub timestamp_valid_bits: u32,
    pub min_image_transfer_granularity: [u32; 3],
}
const _: () = assert!(size_of::<VkQueueFamilyProperties>() == 24);

/// `VK_QUEUE_GRAPHICS_BIT`.
pub const VK_QUEUE_GRAPHICS_BIT: u32 = 0x1;
/// `VK_QUEUE_COMPUTE_BIT`.
pub const VK_QUEUE_COMPUTE_BIT: u32 = 0x2;
/// `VK_QUEUE_TRANSFER_BIT`.
pub const VK_QUEUE_TRANSFER_BIT: u32 = 0x4;

/// `VkPhysicalDeviceType` names, for the log.
pub fn device_type_name(device_type: u32) -> &'static str {
    match device_type {
        0 => "other",
        1 => "integrated GPU",
        2 => "discrete GPU",
        3 => "virtual GPU",
        4 => "CPU",
        _ => "unknown",
    }
}

/// The device-level entry points the instance code needs.
pub mod device_functions {
    use std::ffi::c_void;

    pub type EnumeratePhysicalDevices =
        unsafe extern "system" fn(*mut c_void, *mut u32, *mut *mut c_void) -> i32;
    pub type GetPhysicalDeviceProperties =
        unsafe extern "system" fn(*mut c_void, *mut super::VkPhysicalDeviceProperties);
    pub type GetPhysicalDeviceQueueFamilyProperties =
        unsafe extern "system" fn(*mut c_void, *mut u32, *mut super::VkQueueFamilyProperties);
    pub type CreateDevice =
        unsafe extern "system" fn(*mut c_void, *const super::VkDeviceCreateInfo, *const c_void, *mut *mut c_void) -> i32;
    pub type DestroyInstance = unsafe extern "system" fn(*mut c_void, *const c_void);
    pub type DestroyDevice = unsafe extern "system" fn(*mut c_void, *const c_void);
    pub type GetDeviceProcAddr =
        unsafe extern "system" fn(*mut c_void, *const std::ffi::c_char) -> *mut c_void;
}
