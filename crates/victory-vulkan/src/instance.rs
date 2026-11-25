//! Adapters on this machine: whether a loader is installed, what it lists,
//! and the instance extensions and surface the window layer will need.
//!
//! Everything here goes through `ash`, so the FFI layouts this module used
//! to spell out by hand are gone: a `vk::PhysicalDeviceProperties` is ash's
//! own type, and its `device_name` is a `[c_char; 256]`.

use std::ffi::c_char;

use ash::vk;

use crate::loader::entry;

/// One adapter the backend could use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    pub api_version: (u32, u32, u32),
    pub driver_version: u32,
    pub device_type: &'static str,
    pub vendor_id: u32,
    pub device_id: u32,
    /// Queue families with graphics, compute and transfer.
    pub queue_families: usize,
}

/// Whether a Vulkan loader is installed.
pub fn available() -> bool {
    entry().is_ok()
}

/// The adapters this machine has, or why the query failed. No device is
/// created and nothing is allocated: an instance is made, the physical
/// devices read, and the instance destroyed.
pub fn probe() -> Result<Vec<Info>, String> {
    let entry = entry().map_err(str::to_owned)?;
    // The instance asks for the *loader's* version, not a hardcoded one.
    //
    // Vulkan defines "supported" as `min(loader, driver)`, and `ash` decides
    // which core tables to resolve from the number an instance requested. A
    // hardcoded floor therefore silently caps what we can reach: asking for
    // 1.1 while enabling a `KHR` alias of a later-core entry point loads no
    // pointer at all (that was a real crash, see the 1.4 entry in
    // `docs/decisions/`). Asking for what the loader offers is both correct
    // and forward compatible — a 1.4 loader gives us 1.4 tables, and the
    // driver-side check below is what actually gates use.
    // SAFETY: the loader is open, and `vkEnumerateInstanceVersion` is a query
    // that allocates nothing and has no other precondition.
    let offered = unsafe { entry.try_enumerate_instance_version() }
        .ok()
        .flatten()
        .filter(|v| *v > vk::API_VERSION_1_1)
        .unwrap_or(vk::API_VERSION_1_1);
    let app = vk::ApplicationInfo::default()
        .application_name(c"victory")
        .application_version(1)
        .engine_name(c"victory")
        .engine_version(1)
        .api_version(offered);
    let create_info = vk::InstanceCreateInfo::default().application_info(&app);
    // SAFETY: `create_info` describes a valid instance and the loader is
    // open for the life of the process.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|e| format!("vkCreateInstance failed: {e}"))?;
    // SAFETY: an instance this function just created.
    let devices = unsafe { instance.enumerate_physical_devices() };
    let described = devices
        .map_err(|e| format!("vkEnumeratePhysicalDevices failed: {e}"))?
        .into_iter()
        .map(|device| describe(&instance, device))
        .collect();
    // SAFETY: the instance came from `vkCreateInstance` and nothing else in
    // this function uses it afterwards.
    unsafe { instance.destroy_instance(None) };
    Ok(described)
}

/// What one adapter says about itself.
fn describe(instance: &ash::Instance, device: vk::PhysicalDevice) -> Info {
    // SAFETY: a physical device the instance listed; the properties are a
    // plain output struct.
    let props = unsafe { instance.get_physical_device_properties(device) };
    // SAFETY: as above, for the queue family list.
    let families = unsafe { instance.get_physical_device_queue_family_properties(device) };
    let wanted = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER;
    Info {
        name: name_of(&props.device_name),
        api_version: split_api_version(props.api_version),
        driver_version: props.driver_version,
        device_type: device_type_name(props.device_type),
        vendor_id: props.vendor_id,
        device_id: props.device_id,
        queue_families: families.iter().filter(|f| f.queue_flags.contains(wanted)).count(),
    }
}

/// A NUL-terminated device name as text.
fn name_of(raw: &[c_char; vk::MAX_PHYSICAL_DEVICE_NAME_SIZE]) -> String {
    raw.iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8 as char)
        .collect()
}

/// A packed Vulkan version as major, minor and patch.
pub fn split_api_version(version: u32) -> (u32, u32, u32) {
    (
        vk::api_version_major(version),
        vk::api_version_minor(version),
        vk::api_version_patch(version),
    )
}

/// An adapter's kind, for the log.
pub fn device_type_name(kind: vk::PhysicalDeviceType) -> &'static str {
    match kind {
        vk::PhysicalDeviceType::INTEGRATED_GPU => "integrated",
        vk::PhysicalDeviceType::DISCRETE_GPU => "discrete",
        vk::PhysicalDeviceType::VIRTUAL_GPU => "virtual",
        vk::PhysicalDeviceType::CPU => "cpu",
        _ => "unknown",
    }
}

/// The instance extensions the loader offers, for the log and for the
/// surface the window layer will need.
pub fn instance_extensions() -> Result<Vec<String>, String> {
    let entry = entry().map_err(str::to_owned)?;
    // SAFETY: a layer-less enumeration of the loader's own extensions.
    let properties = unsafe { entry.enumerate_instance_extension_properties(None) }
        .map_err(|e| format!("vkEnumerateInstanceExtensionProperties failed: {e}"))?;
    Ok(properties
        .iter()
        .map(|p| p.extension_name_as_c_str().map(|s| s.to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default())
}

/// The loader's own version, if it reports one.
pub fn loader_version() -> Option<(u32, u32, u32)> {
    let entry = entry().ok()?;
    // SAFETY: the call writes one packed version and takes no arguments.
    unsafe { entry.try_enumerate_instance_version() }
        .ok()
        .flatten()
        .map(split_api_version)
}

/// The name the window layer will ask the loader for.
pub const SURFACE_EXTENSION: &std::ffi::CStr = ash::khr::surface::NAME;
