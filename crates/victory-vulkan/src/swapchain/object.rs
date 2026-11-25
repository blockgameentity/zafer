//! The swapchain object: created from the policy, acquired from and
//! presented through.
//!
//! Creation cannot be decided in the abstract — the surface's formats,
//! present modes and capabilities are the machine's — but *how* to decide is
//! ([`super`]), and this only carries the decisions out. The three results
//! Vulkan returns that are not failures (`SUBOPTIMAL_KHR`, `NOT_READY`,
//! `OUT_OF_DATE`) are classified here so a caller cannot mistake them for
//! errors, which is the mistake that turns a resize into a crash.

use ash::vk;

use super::{choose_format, choose_present_mode, composite_alpha, enclosing_extent, image_count};

/// What a swapchain can tell its caller after an acquire or a present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The image is ours to draw into, or was presented.
    Ok,
    /// Presented, but the surface no longer matches this swapchain: draw one
    /// more frame and then rebuild.
    Suboptimal,
    /// The swapchain is out of date and must be rebuilt before anything
    /// else; every image is now invalid.
    OutOfDate,
    /// Nothing was presented because the queue was full (`MAILBOX` and
    /// `IMMEDIATE` only). Not an error, and worth retrying.
    NotReady,
}

/// The surface to present on, and the window's current size.
///
/// Grouped because they always travel together and they are what the caller
/// — the window layer — actually knows; the rest of what
/// `vkCreateSwapchainKHR` wants is the machine's, and is discovered here.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// The `VkSurfaceKHR` SDL or the window layer created.
    pub handle: vk::SurfaceKHR,
    /// The window's client area, which the surface may outlive.
    pub width: u32,
    /// As `width`.
    pub height: u32,
}

/// The swapchain and the images behind it.
pub struct Swapchain {
    device: ash::Device,
    loader: ash::khr::swapchain::Device,
    handle: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    format: vk::SurfaceFormatKHR,
    extent: vk::Extent2D,
}

impl Swapchain {
    /// Creates a swapchain on `surface` from what it reports.
    ///
    /// The policy functions refuse to invent values: a surface with no
    /// format, or a window with a zero extent, is an error rather than a
    /// guess, because a guess is a black screen that looks like a shader
    /// bug.
    pub fn new(
        instance: &ash::Instance,
        surface_loader: &ash::khr::surface::Instance,
        device: &ash::Device,
        physical: vk::PhysicalDevice,
        surface: Surface,
        old: Option<vk::SwapchainKHR>,
    ) -> Result<Swapchain, String> {
        let Surface { handle: surface, width, height } = surface;
        // SAFETY: the surface belongs to `instance`, which the caller owns.
        let formats = unsafe {
            surface_loader.get_physical_device_surface_formats(physical, surface)
        }
        .map_err(|e| format!("surface formats: {e}"))?;
        let format = choose_format(&formats);

        // SAFETY: as above.
        let modes = unsafe {
            surface_loader.get_physical_device_surface_present_modes(physical, surface)
        }
        .map_err(|e| format!("surface present modes: {e}"))?;
        let present_mode = choose_present_mode(&modes);

        // SAFETY: as above.
        let caps = unsafe {
            surface_loader.get_physical_device_surface_capabilities(physical, surface)
        }
        .map_err(|e| format!("surface capabilities: {e}"))?;

        // A zero-sized window is legal (a minimised one), and Vulkan refuses
        // a zero extent. The policy clamps to the surface's minimum, which is
        // what the window will come back to; the caller rebuiled on resize.
        let extent = enclosing_extent(width, height, caps.min_image_extent, caps.max_image_extent);
        if extent.width == 0 || extent.height == 0 {
            return Err("the window has no area".to_owned());
        }
        let images = image_count(caps.min_image_count, caps.max_image_count);

        let create = vk::SwapchainCreateInfoKHR::default()
            .surface(surface)
            .min_image_count(images)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(composite_alpha(caps.supported_composite_alpha))
            .present_mode(present_mode)
            .clipped(true)
            .old_swapchain(old.unwrap_or(vk::SwapchainKHR::null()));

        let loader = ash::khr::swapchain::Device::load(instance, device);
        // SAFETY: every handle in `create` is valid for `device`.
        let handle = unsafe { loader.create_swapchain(&create, None) }
            .map_err(|e| format!("vkCreateSwapchainKHR: {e}"))?;

        // SAFETY: the swapchain was just created.
        let images = unsafe { loader.get_swapchain_images(handle) }
            .map_err(|e| format!("vkGetSwapchainImagesKHR: {e}"))?;

        Ok(Swapchain {
            device: device.clone(),
            loader,
            handle,
            images,
            format,
            extent,
        })
    }

    /// The swapchain handle, for a present.
    pub fn handle(&self) -> vk::SwapchainKHR {
        self.handle
    }

    /// The format the images were created with.
    pub fn format(&self) -> vk::SurfaceFormatKHR {
        self.format
    }

    /// The size every image has.
    pub fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    /// The images, in the order Vulkan indexes them.
    pub fn images(&self) -> &[vk::Image] {
        &self.images
    }

    /// Acquires the next image, classifying the results that are not
    /// failures.
    pub fn acquire(&self, signal: vk::Semaphore, fence: vk::Fence) -> Result<(u32, Status), String> {
        // SAFETY: the swapchain and the semaphore are the caller's valid
        // handles.
        match unsafe { self.loader.acquire_next_image(self.handle, u64::MAX, signal, fence) } {
            Ok((index, false)) => Ok((index, Status::Ok)),
            Ok((index, true)) => Ok((index, Status::Suboptimal)),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok((0, Status::OutOfDate)),
            Err(e) => Err(format!("vkAcquireNextImageKHR: {e}")),
        }
    }

    /// Presents `index` on `queue`, classifying the results that are not
    /// failures.
    pub fn present(&self, queue: vk::Queue, index: u32, wait: vk::Semaphore) -> Result<Status, String> {
        let swapchains = [self.handle];
        let indices = [index];
        let waits = [wait];
        let present = vk::PresentInfoKHR::default()
            .swapchains(&swapchains)
            .image_indices(&indices)
            .wait_semaphores(&waits);
        // SAFETY: the queue, swapchain and semaphore are the caller's valid
        // handles, and the image index came from this swapchain.
        match unsafe { self.loader.queue_present(queue, &present) } {
            Ok(false) => Ok(Status::Ok),
            Ok(true) => Ok(Status::Suboptimal),
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok(Status::OutOfDate),
            Err(vk::Result::NOT_READY) => Ok(Status::NotReady),
            Err(e) => Err(format!("vkQueuePresentKHR: {e}")),
        }
    }
}

impl Drop for Swapchain {
    fn drop(&mut self) {
        // SAFETY: the swapchain was created by this object and the device
        // outlives it (we hold a clone, so it cannot have been dropped).
        unsafe { self.loader.destroy_swapchain(self.handle, None) };
        let _ = &self.device;
    }
}
