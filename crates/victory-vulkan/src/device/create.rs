//! Creating the device: an instance, the first usable adapter, and one
//! queue.
//!
//! The instance is owned here because a `VkDevice` cannot outlive it, and
//! dropping this type closes both in the reverse of the order they were
//! opened.

use std::ffi::CStr;

use ash::vk;

use super::queue;

/// Which optional device extensions the backend got, so a caller can tell a
/// fast path from the fallback rather than assuming.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extensions {
    /// `VK_KHR_buffer_device_address`: a heap can report a GPU address.
    pub buffer_device_address: bool,
    /// `VK_EXT_descriptor_buffer`: a descriptor heap can live in a buffer.
    pub descriptor_buffer: bool,
    /// `VK_KHR_swapchain`: a surface can be presented through the device.
    ///
    /// Without it `ash`'s swapchain functions are never loaded, and calling
    /// one is a panic rather than an error — which is exactly what a window
    /// on a real driver hit.
    pub swapchain: bool,
}

/// The device extensions this adapter reports.
fn available_extensions(instance: &ash::Instance, physical: vk::PhysicalDevice) -> Vec<String> {
    // SAFETY: a physical device the caller's instance listed; the call fills
    // an output list.
    let Ok(props) = (unsafe { instance.enumerate_device_extension_properties(physical) }) else {
        return Vec::new();
    };
    props
        .into_iter()
        .filter_map(|p| Some(p.extension_name_as_c_str().ok()?.to_string_lossy().into_owned()))
        .collect()
}
use crate::loader::entry;

/// A logical device, its one queue, and the instance they came from.
pub struct Device {
    /// Which optional device extensions were enabled.
    pub extensions: Extensions,
    instance: ash::Instance,
    device: ash::Device,
    physical: vk::PhysicalDevice,
    queue_family: u32,
    queue: vk::Queue,
}

impl Device {
    /// The logical device, for everything that creates or submits.
    pub fn device(&self) -> &ash::Device {
        &self.device
    }

    /// The adapter this device runs on.
    pub fn physical(&self) -> vk::PhysicalDevice {
        self.physical
    }

    /// The instance the device and its adapter came from.
    ///
    /// Needed for everything asked of the adapter rather than the device:
    /// its memory types, its limits, and the query a heap makes.
    pub fn instance(&self) -> &ash::Instance {
        &self.instance
    }

    /// The instance as a raw `VkInstance`, for a window layer that only
    /// knows C handles (SDL makes the surface, and takes the number).
    pub fn instance_handle(&self) -> u64 {
        use ash::vk::Handle;
        self.instance.handle().as_raw()
    }

    /// A window surface on our own Wayland surface.
    ///
    /// `display` and `surface` are `wl_display`/`wl_surface` handles; the
    /// extension is loaded here so a caller that never makes a surface never
    /// touches it.
    pub fn wayland_surface(&self, display: u64, surface: u64) -> Result<u64, String> {
        use ash::vk::Handle;
        let loader =
            ash::khr::wayland_surface::Instance::load(crate::loader::entry()?, &self.instance);
        let info = vk::WaylandSurfaceCreateInfoKHR::default()
            .display(display as *mut vk::wl_display)
            .surface(surface as *mut vk::wl_surface);
        // SAFETY: both handles are a live `wl_display` and its `wl_surface`
        // (the window layer keeps them for the process's life), and the
        // instance had `VK_KHR_wayland_surface` enabled.
        unsafe { loader.create_wayland_surface(&info, None) }
            .map(|s| s.as_raw())
            .map_err(|e| format!("vkCreateWaylandSurfaceKHR: {e}"))
    }

    /// The one queue every submission goes through.
    pub fn queue(&self) -> vk::Queue {
        self.queue
    }

    /// The family [`queue`](Self::queue) belongs to.
    pub fn queue_family(&self) -> u32 {
        self.queue_family
    }

    /// The adapter's name, as the driver reports it.
    ///
    /// The one line a bug report needs: which GPU the backend actually chose
    /// decides what a rendering fault even means.
    pub fn name(&self) -> String {
        // SAFETY: the adapter belongs to the instance this device came from.
        let props = unsafe {
            self.instance
                .get_physical_device_properties(self.physical)
        };
        super::create::device_name(&props)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: `device` was created by `vkCreateDevice` on `instance` and
        // nothing has destroyed it since; it is the only device this struct
        // made, so no second destroy can race or repeat.
        unsafe { self.device.destroy_device(None) };
        // SAFETY: every device created from the instance is gone just above,
        // which is what `vkDestroyInstance` requires, and no handle from it
        // is used afterwards.
        unsafe { self.instance.destroy_instance(None) };
    }
}

/// The adapter and family the device will be made on.
struct Chosen {
    physical: vk::PhysicalDevice,
    family: u32,
}

/// Creates an instance, picks the first adapter with a graphics and compute
/// queue family, and creates the device with that one queue at priority 1.0.
///
/// `request` names the application in the instance, and `required` lists the
/// instance extensions to enable (the surface ones the window layer needs).
///
/// `Ok(None)` means there is no loader or no suitable adapter: the machine
/// cannot run the backend, which is an answer rather than an error. `Err`
/// means Vulkan was there and refused something.
pub fn create(request: &CStr, required: &[&CStr]) -> Result<Option<Device>, String> {
    let Ok(entry) = entry() else {
        // No loader installed: nothing to create, and nothing wrong.
        return Ok(None);
    };

    // 1.2 is the floor, not 1.1: `vkGetBufferDeviceAddress` and
    // `vkGetBufferDeviceAddressKHR` are the *same* entry point, and `ash`
    // resolves the 1.2 tables only when the instance asked for 1.2 or later.
    // Asking for 1.1 while enabling the KHR extension by name gave an `ash`
    // panic ("Unable to load get_buffer_device_address") on a real driver,
    // which is how this was found.
    let app = vk::ApplicationInfo::default()
        .application_name(request)
        .application_version(1)
        .engine_name(c"victory")
        .engine_version(1)
        .api_version(vk::make_api_version(0, 1, 2, 0));
    // Vulkan wants the extension names as a slice of raw pointers.
    let extensions: Vec<*const std::os::raw::c_char> =
        required.iter().map(|name| name.as_ptr()).collect();
    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&extensions);
    // SAFETY: `create_info` describes a valid instance, the strings it
    // borrows outlive the call, and the loader is open for the process's
    // life.
    let instance = unsafe { entry.create_instance(&create_info, None) }
        .map_err(|e| format!("vkCreateInstance failed: {e}"))?;

    tracing::debug!(
        required = required.len(),
        "instance created; looking for an adapter"
    );
    let chosen = match pick(&instance) {
        Ok(Some(chosen)) => chosen,
        // No adapter can take the backend: close what we opened and say so.
        Ok(None) => {
            tracing::warn!("no adapter has a graphics and compute queue family");
            close(&instance);
            return Ok(None);
        }
        Err(e) => {
            close(&instance);
            return Err(e);
        }
    };

    let (logical, queue, extensions) = match open(&instance, &chosen) {
        Ok(opened) => opened,
        Err(e) => {
            close(&instance);
            return Err(e);
        }
    };

    Ok(Some(Device {
        extensions,
        instance,
        device: logical,
        physical: chosen.physical,
        queue_family: chosen.family,
        queue,
    }))
}

/// Closes an instance no device was created from.
fn close(instance: &ash::Instance) {
    // SAFETY: the caller holds the only reference to an instance this module
    // created, and no device made from it is alive at this point.
    unsafe { instance.destroy_instance(None) };
}

/// The first adapter with a graphics and compute queue family.
fn pick(instance: &ash::Instance) -> Result<Option<Chosen>, String> {
    // SAFETY: an instance this function's caller created; the call only
    // fills an output list of handles.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("vkEnumeratePhysicalDevices failed: {e}"))?;
    // Enumerating in order and taking the first usable adapter would take
    // whatever the driver happened to list first, and on a laptop or a
    // desktop with an iGPU that is the iGPU. A console's GPU is a
    // discrete-class part, so the console's work belongs on one: rank the
    // candidates and keep the best instead of the first.
    let mut best: Option<(u8, Chosen)> = None;
    for physical in devices {
        // SAFETY: a physical device the instance just listed, and the
        // properties are a plain output structure.
        let props = unsafe { instance.get_physical_device_properties(physical) };
        let rank = rank_of(props.device_type);
        // SAFETY: as above, for the queue families.
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let Some(family) = queue::family(&families) else {
            tracing::debug!(
                name = %device_name(&props),
                "adapter has no graphics+compute family; skipped"
            );
            continue;
        };
        let name = device_name(&props);
        tracing::debug!(%name, kind = crate::instance::device_type_name(props.device_type), rank, "adapter considered");
        if best.as_ref().is_some_and(|(chosen, _)| *chosen <= rank) {
            continue;
        }
        best = Some((rank, Chosen { physical, family }));
    }
    match best {
        Some((rank, chosen)) => {
            tracing::info!(
                name = %device_name(&unsafe {
                    // SAFETY: an adapter the instance listed above.
                    instance.get_physical_device_properties(chosen.physical)
                }),
                rank,
                family = chosen.family,
                "adapter chosen"
            );
            Ok(Some(chosen))
        }
        None => Ok(None),
    }
}

/// How much this backend prefers an adapter of `kind`: lower is better.
///
/// The ordering is the console's, not the desktop's. A discrete GPU is what
/// a console has. An integrated one is a real GPU and has unified memory,
/// which the console's memory model actually matches, so it is next. A
/// virtual or CPU adapter runs the code correctly and slowly, so it is the
/// last resort rather than a failure — a machine with no GPU still runs the
/// tests, which is what makes them usable in CI.
pub(crate) fn rank_of(kind: vk::PhysicalDeviceType) -> u8 {
    match kind {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 3,
        _ => 4,
    }
}

/// An adapter's name, as the driver reports it, without the trailing NULs.
pub(crate) fn device_name(props: &vk::PhysicalDeviceProperties) -> String {
    let end = props
        .device_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(props.device_name.len());
    String::from_utf8_lossy(unsafe {
        // SAFETY: `device_name` is a `[c_char; 256]` this function owns a
        // reference to, and `end` is bounded by its length.
        std::slice::from_raw_parts(props.device_name.as_ptr().cast::<u8>(), end)
    })
    .into_owned()
}

/// The logical device with its single queue.
fn open(
    instance: &ash::Instance,
    chosen: &Chosen,
) -> Result<(ash::Device, vk::Queue, Extensions), String> {
    // The create info borrows the priority, so it lives until the device is
    // made.
    let priorities = [queue::PRIORITY];
    let queue_info = [queue::create_info(chosen.family, &priorities)];

    // The backend asks for two extensions, and *asks for them here*: a
    // function ash only resolves when the extension is enabled is a hard
    // panic if it is called anyway, which is exactly the bug a real driver
    // found (`vkGetBufferDeviceAddress` on a device that had enabled
    // nothing). Both are optional, so each is enabled only when the adapter
    // reports it, and `Device::extensions` records which came back.
    let available = available_extensions(instance, chosen.physical);
    let wanted = [
        vk::KHR_SWAPCHAIN_NAME,
        vk::KHR_BUFFER_DEVICE_ADDRESS_NAME,
        vk::EXT_DESCRIPTOR_BUFFER_NAME,
    ];
    // Kept as `&'static CStr` so the pointers below live as long as the
    // create info that borrows them.
    let enabled: Vec<&'static CStr> = wanted
        .into_iter()
        .filter(|name| available.iter().any(|a| name.to_string_lossy() == *a))
        .collect();
    let enabled_ptrs: Vec<*const std::os::raw::c_char> =
        enabled.iter().map(|name| name.as_ptr()).collect();

    let mut features = vk::PhysicalDeviceBufferDeviceAddressFeatures::default();
    if enabled.contains(&vk::KHR_BUFFER_DEVICE_ADDRESS_NAME) {
        features = features.buffer_device_address(true);
    }
    let mut create_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(&queue_info)
        .enabled_extension_names(&enabled_ptrs);
    if features.buffer_device_address == vk::TRUE {
        // `ash` master has no `push_next`: `p_next` is a raw pointer on the
        // struct. `features` is a local that outlives the call below, which
        // is exactly what the chain requires.
        create_info.p_next =
            std::ptr::from_ref(&features).cast::<std::ffi::c_void>();
    }

    // SAFETY: `create_info` names one family that exists on this adapter, a
    // priority list and an extension-name list that outlive the call, and a
    // feature struct that outlives it too. Every extension named was
    // reported by this adapter.
    let device = unsafe { instance.create_device(chosen.physical, &create_info, None) }
        .map_err(|e| format!("vkCreateDevice failed: {e}"))?;
    let extensions = Extensions {
        buffer_device_address: features.buffer_device_address == vk::TRUE,
        descriptor_buffer: enabled.contains(&vk::EXT_DESCRIPTOR_BUFFER_NAME),
        swapchain: enabled.contains(&vk::KHR_SWAPCHAIN_NAME),
    };
    if !extensions.swapchain {
        // A device that cannot present is not one this backend can use; say
        // it here rather than panicking inside the first swapchain call.
        // SAFETY: the device was created above and nothing else holds it.
        unsafe { device.destroy_device(None) };
        return Err("the adapter does not offer VK_KHR_swapchain".to_owned());
    }
    // The two extension bits decide whether a heap can carry an address and
    // whether a descriptor heap can live in a buffer, so which way they went
    // is the first thing to look at when a title's pointer model misbehaves.
    tracing::info!(
        buffer_device_address = extensions.buffer_device_address,
        descriptor_buffer = extensions.descriptor_buffer,
        "device created"
    );
    // SAFETY: the queue belongs to a device this function just created, and
    // index 0 was asked for by its only queue create info.
    let queue = unsafe { device.get_device_queue(chosen.family, 0) };
    Ok((device, queue, extensions))
}
