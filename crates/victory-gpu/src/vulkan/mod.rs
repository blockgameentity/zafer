//! The Vulkan backend: the host graphics API we are moving to.
//!
//! The console's memory model is Vulkan's (a GPU address is a CPU address,
//! memory is unified), and D3D12-on-Linux already means vkd3d-proton
//! translating a full API on top of Vulkan. This module is additive: the
//! D3D12 host stays the working path while the backend is built, and the
//! design and work order are in `docs/vulkan-backend.md`.
//!
//! What exists today is the part under everything else: opening the loader
//! at run time and enumerating adapters. That alone tells a run whether
//! the machine can take the backend, and the tests skip cleanly when there
//! is no driver.

pub mod loader;
pub mod types;

use std::ffi::c_void;

use loader::Loader;
use types::{VkApplicationInfo, VkExtensionProperties, VkInstanceCreateInfo, VkPhysicalDeviceProperties, VkQueueFamilyProperties, VK_QUEUE_COMPUTE_BIT, VK_QUEUE_GRAPHICS_BIT, VK_QUEUE_TRANSFER_BIT, VK_STRUCTURE_TYPE_APPLICATION_INFO, VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, VK_SUCCESS, api_version, device_type_name, split_api_version};

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
    Loader::get().is_ok()
}

/// The adapters this machine has, or why the query failed. No device is
/// created and nothing is allocated: an instance is made, the physical
/// devices read, and the instance destroyed.
pub fn probe() -> Result<Vec<Info>, String> {
    let loader = Loader::get().map_err(str::to_owned)?;
    let app_name = c"victory";
    let app = VkApplicationInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: std::ptr::null(),
        p_application_name: app_name.as_ptr(),
        application_version: 1,
        p_engine_name: app_name.as_ptr(),
        engine_version: 1,
        // 1.1 is the floor: timeline semaphores and the extensions the
        // backend needs are core there.
        api_version: api_version(1, 1),
    };
    let create_info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: std::ptr::null(),
        flags: 0,
        p_application_info: &app,
        enabled_layer_count: 0,
        pp_enabled_layer_names: std::ptr::null(),
        enabled_extension_count: 0,
        pp_enabled_extension_names: std::ptr::null(),
    };
    let mut instance: *mut c_void = std::ptr::null_mut();
    // SAFETY: `create_info` describes a valid instance and the loader is
    // open for the life of the process.
    let created = unsafe {
        (loader.create_instance)(
            (&raw const create_info).cast(),
            std::ptr::null(),
            &mut instance,
        )
    };
    if created != VK_SUCCESS {
        return Err(format!("vkCreateInstance failed (VkResult {created})"));
    }
    // SAFETY: the name is NUL-terminated and the loader is open.
    let enumerate = unsafe {
        (loader.get_instance_proc_addr)(instance, c"vkEnumeratePhysicalDevices".as_ptr())
    };
    // SAFETY: the name is NUL-terminated and the loader is open.
    let properties = unsafe {
        (loader.get_instance_proc_addr)(instance, c"vkGetPhysicalDeviceProperties".as_ptr())
    };
    // SAFETY: the name is NUL-terminated and the loader is open.
    let queues = unsafe {
        (loader.get_instance_proc_addr)(
            instance,
            c"vkGetPhysicalDeviceQueueFamilyProperties".as_ptr(),
        )
    };
    // SAFETY: the name is NUL-terminated and the loader is open.
    let destroy = unsafe { (loader.get_instance_proc_addr)(instance, c"vkDestroyInstance".as_ptr()) };
    if enumerate.is_null() || properties.is_null() || queues.is_null() || destroy.is_null() {
        // SAFETY: an instance from `vkCreateInstance`.
        unsafe { destroy_instance(loader, instance) };
        return Err("the Vulkan loader lacks the instance entry points".into());
    }
    // SAFETY: the signatures are Vulkan's own, looked up from a live
    // instance.
    let (enumerate, properties, queues): (
        types::device_functions::EnumeratePhysicalDevices,
        types::device_functions::GetPhysicalDeviceProperties,
        types::device_functions::GetPhysicalDeviceQueueFamilyProperties,
    ) = unsafe {
        (
            std::mem::transmute(enumerate),
            std::mem::transmute(properties),
            std::mem::transmute(queues),
        )
    };
    let mut found = probe_adapters(instance, enumerate, properties, queues);
    // SAFETY: an instance from `vkCreateInstance`; nothing else uses it.
    unsafe { destroy_instance(loader, instance) };
    if let Ok(list) = &mut found {
        list.sort_by(|a, b| a.name.cmp(&b.name));
    }
    found
}

fn probe_adapters(
    instance: *mut c_void,
    enumerate: types::device_functions::EnumeratePhysicalDevices,
    properties: types::device_functions::GetPhysicalDeviceProperties,
    queues: types::device_functions::GetPhysicalDeviceQueueFamilyProperties,
) -> Result<Vec<Info>, String> {
    // SAFETY: the call fills a count; the instance is live.
    let mut count = 0u32;
    let listed = unsafe { enumerate(instance, &mut count, std::ptr::null_mut()) };
    if listed != VK_SUCCESS || count == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the handles were asked for above.
    let mut devices: Vec<*mut c_void> = vec![std::ptr::null_mut(); count as usize];
    // SAFETY: the buffer holds `count` handles and the instance is live.
    let listed = unsafe { enumerate(instance, &mut count, devices.as_mut_ptr()) };
    if listed != VK_SUCCESS {
        return Err(format!("vkEnumeratePhysicalDevices failed (VkResult {listed})"));
    }
    devices.truncate(count as usize);
    Ok(devices
        .into_iter()
        .map(|device| {
            // SAFETY: a physical device the instance listed.
            let mut props: VkPhysicalDeviceProperties = unsafe { std::mem::zeroed() };
            // SAFETY: writes the device's properties, as Vulkan specifies.
            unsafe { properties(device, &mut props) };
            let name = props
                .device_name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8 as char)
                .collect::<String>();
            // SAFETY: the call fills a count; the device is live.
            let mut families = 0u32;
            unsafe { queues(device, &mut families, std::ptr::null_mut()) };
            let mut list = vec![VkQueueFamilyProperties::default(); families as usize];
            // SAFETY: the buffer holds `families` entries.
            unsafe { queues(device, &mut families, list.as_mut_ptr()) };
            list.truncate(families as usize);
            Info {
                name,
                api_version: split_api_version(props.api_version),
                driver_version: props.driver_version,
                device_type: device_type_name(props.device_type),
                vendor_id: props.vendor_id,
                device_id: props.device_id,
                queue_families: list
                    .iter()
                    .filter(|f| {
                        f.queue_flags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT) != 0
                    })
                    .count(),
            }
        })
        .collect())
}

///
/// # Safety
/// `instance` came from `vkCreateInstance` and is not used afterwards.
unsafe fn destroy_instance(loader: &Loader, instance: *mut c_void) {
    // SAFETY: the name is NUL-terminated and the loader is open.
    let destroy =
        unsafe { (loader.get_instance_proc_addr)(instance, c"vkDestroyInstance".as_ptr()) };
    let Some(destroy) = (unsafe {
        (!destroy.is_null()).then(|| {
            std::mem::transmute::<*mut c_void, types::device_functions::DestroyInstance>(destroy)
        })
    }) else {
        return;
    };
    // SAFETY: per the caller; no callback is needed.
    unsafe { destroy(instance, std::ptr::null()) };
}

/// The instance extensions the loader offers, for the log and for the
/// surface the window layer will need.
pub fn instance_extensions() -> Result<Vec<String>, String> {
    let loader = Loader::get().map_err(str::to_owned)?;
    let mut count = 0u32;
    // SAFETY: a layer-less query that only fills the count.
    let listed = unsafe {
        (loader.enumerate_instance_extension_properties)(std::ptr::null(), &mut count, std::ptr::null_mut())
    };
    if listed != VK_SUCCESS {
        return Err(format!("vkEnumerateInstanceExtensionProperties failed ({listed})"));
    }
    let mut props = vec![
        VkExtensionProperties { extension_name: [0; 256], spec_version: 0 };
        count as usize
    ];
    // SAFETY: the buffer holds `count` entries of the size the API expects.
    let listed = unsafe {
        (loader.enumerate_instance_extension_properties)(
            std::ptr::null(),
            &mut count,
            props.as_mut_ptr().cast(),
        )
    };
    if listed != VK_SUCCESS {
        return Err(format!("vkEnumerateInstanceExtensionProperties failed ({listed})"));
    }
    Ok(props
        .iter()
        .map(|p| {
            p.extension_name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8 as char)
                .collect()
        })
        .collect())
}

/// The loader's own version, if it reports one.
pub fn loader_version() -> Option<(u32, u32, u32)> {
    let loader = Loader::get().ok()?;
    let mut version = 0u32;
    // SAFETY: the call writes one packed version.
    unsafe { (loader.enumerate_instance_version?)(&mut version) };
    Some(split_api_version(version))
}

/// The name the window layer will ask the loader for.
#[cfg(unix)]
pub const SURFACE_EXTENSION: &[u8] = types::VK_KHR_SURFACE_EXTENSION_NAME;
#[cfg(windows)]
pub const SURFACE_EXTENSION: &[u8] = types::VK_KHR_SURFACE_EXTENSION_NAME;

#[cfg(test)]
mod tests {
    use super::*;

    /// One Vulkan query at a time: some ICDs are not safe to create and
    /// destroy instances with from several threads at once, and these tests
    /// all drive the same loader.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn availability_matches_probing() {
        let _serial = serial();
        if !available() {
            // No driver here: probing must say why, not panic.
            assert!(probe().is_err());
            return;
        }
        let probed = probe().expect("a loader with no adapters still probes");
        assert!(probed.len() < 64, "an implausible adapter count");
    }

    #[test]
    fn a_probed_adapter_describes_itself() {
        let _serial = serial();
        let Ok(adapters) = probe() else {
            return; // no Vulkan here
        };
        for a in adapters {
            assert!(!a.name.is_empty(), "{a:?}");
            assert!(a.api_version.0 >= 1, "{a:?}");
            assert_ne!(a.device_type, "unknown", "{a:?}");
        }
    }

    #[test]
    fn the_loader_reports_a_vulkan_version() {
        let _serial = serial();
        if !available() {
            return;
        }
        let version = loader_version().expect("a loader that is open reports its version");
        assert!(version.0 >= 1, "{version:?}");
    }

    #[test]
    fn instance_extensions_include_surface_when_a_driver_is_present() {
        let _serial = serial();
        let Ok(extensions) = instance_extensions() else {
            return;
        };
        assert!(
            extensions.is_empty() || extensions.iter().any(|e| e.starts_with("VK_KHR_surface")),
            "{extensions:?}"
        );
    }

    #[test]
    fn the_ffi_layouts_are_the_headers() {
        assert_eq!(split_api_version(api_version(1, 3)), (1, 3, 0));
        assert_eq!(size_of::<VkQueueFamilyProperties>(), 24);
        assert_eq!(size_of::<VkExtensionProperties>(), 260);
    }
}
