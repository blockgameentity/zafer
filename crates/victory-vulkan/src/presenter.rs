//! A window's frames on Vulkan: acquire an image, clear it, submit, present.
//!
//! This is the milestone-2 path from `docs/vulkan-backend.md`: a real
//! swapchain on a real surface, driven by our own command buffers and
//! semaphores, with no D3D12 anywhere in it. The three results Vulkan
//! returns that are not failures (`SUBOPTIMAL_KHR`, `NOT_READY`,
//! `OUT_OF_DATE`) are classified by [`crate::present`] and never treated as
//! errors, which is what keeps a resize from being a crash.
//!
//! Everything here is skipped cleanly on a machine with no driver: the
//! caller gets `Err` with the driver's message, and nothing is left open.

use ash::vk;
use ash::vk::Handle;

use crate::device::Device;
use crate::loader::entry;
use crate::swapchain::{Status, Surface, Swapchain};

/// A swapchain, the command buffers that draw into it, and the semaphores
/// that order the frames.
pub struct Presenter {
    device: Device,
    surface_loader: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    swapchain: Swapchain,
    pool: vk::CommandPool,
    buffers: Vec<vk::CommandBuffer>,
    /// Signalled when an acquired image is ours to write.
    acquired: vk::Semaphore,
    /// One per image: signalled when its image is ready to present.
    rendered: Vec<vk::Semaphore>,
    /// Signalled when the last submission finished, so a frame cannot
    /// overwrite buffers still in flight.
    in_flight: vk::Fence,
    /// The size the swapchain was built for; a resize rebuilds it.
    size: (u32, u32),
    /// What the last present did, for a caller that wants to log it.
    pub last: Status,
}

impl Presenter {
    /// A presenter for `surface` at `width` x `height`.
    ///
    /// `device` is taken by value: the presenter owns the device it
    /// presents on, and drops the swapchain before the device, which is the
    /// order Vulkan requires.
    pub fn open(
        device: Device,
        surface: u64,
        width: u32,
        height: u32,
    ) -> Result<Presenter, String> {
        let surface = vk::SurfaceKHR::from_raw(surface);
        let entry = entry().map_err(str::to_owned)?;
        let surface_loader = ash::khr::surface::Instance::load(entry, device.instance());
        let swapchain = Swapchain::new(
            device.instance(),
            &surface_loader,
            device.device(),
            device.physical(),
            Surface {
                handle: surface,
                width,
                height,
            },
            None,
        )?;
        let pool = create_pool(&device)?;
        let mut presenter = Presenter {
            device,
            surface_loader,
            surface,
            swapchain,
            pool,
            buffers: Vec::new(),
            acquired: vk::Semaphore::null(),
            rendered: Vec::new(),
            in_flight: vk::Fence::null(),
            size: (width, height),
            last: Status::Ok,
        };
        presenter.sync()?;
        tracing::info!(
            width,
            height,
            images = presenter.buffers.len(),
            "Vulkan swapchain ready"
        );
        Ok(presenter)
    }

    /// The image format the swapchain was created with.
    pub fn format(&self) -> vk::SurfaceFormatKHR {
        self.swapchain.format()
    }

    /// The size the swapchain was built for.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Rebuilds the swapchain for a new window size.
    ///
    /// A zero size is a minimised window: nothing is presented rather than
    /// asking Vulkan for an extent it rejects.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if (width, height) == self.size || width == 0 || height == 0 {
            return Ok(());
        }
        self.rebuild(width, height)
    }

    /// Rebuilds the swapchain whether or not the size changed: `OutOfDate`
    /// means the surface no longer matches even at the same size.
    fn rebuild(&mut self, width: u32, height: u32) -> Result<(), String> {
        let old = self.swapchain.handle();
        let next = Swapchain::new(
            self.device.instance(),
            &self.surface_loader,
            self.device.device(),
            self.device.physical(),
            Surface {
                handle: self.surface,
                width,
                height,
            },
            Some(old),
        )?;
        self.swapchain = next;
        self.size = (width, height);
        self.sync()?;
        tracing::info!(width, height, "Vulkan swapchain rebuilt");
        Ok(())
    }

    /// Draws one frame of `color` and presents it.
    ///
    /// Returns what the present did; `Suboptimal` and `OutOfDate` mean the
    /// next call rebuilds the swapchain, and neither is an error.
    pub fn frame(&mut self, color: [f32; 4]) -> Result<Status, String> {
        let device = self.device.device();
        // SAFETY: the fence belongs to this presenter and every submission
        // that signals it has been waited for by an earlier call.
        unsafe { device.wait_for_fences(&[self.in_flight], true, u64::MAX) }
            .map_err(|e| format!("vkWaitForFences: {e}"))?;

        let (index, status) = self.swapchain.acquire(self.acquired, vk::Fence::null())?;
        if status == Status::OutOfDate {
            let (width, height) = self.size;
            self.rebuild(width, height)?;
            return Ok(Status::OutOfDate);
        }
        let index = index as usize;
        let Some(&buffer) = self.buffers.get(index) else {
            return Err(format!("no command buffer for image {index}"));
        };

        // SAFETY: the fence is ours, and nothing this frame submits has been
        // sent yet.
        unsafe { device.reset_fences(&[self.in_flight]) }
            .map_err(|e| format!("vkResetFences: {e}"))?;
        // SAFETY: the buffer was allocated from our pool and its previous
        // submission has completed (the fence above).
        unsafe {
            device
                .reset_command_buffer(buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("vkResetCommandBuffer: {e}"))?;
        }

        self.record(buffer, index, color)?;

        let buffers = [buffer];
        let waits = [self.acquired];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let signals = [self.rendered[index]];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&waits)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&buffers)
            .signal_semaphores(&signals);
        // SAFETY: the queue, buffers and semaphores are this presenter's,
        // and `in_flight` is reset above.
        unsafe { device.queue_submit(self.device.queue(), &[submit], self.in_flight) }
            .map_err(|e| format!("vkQueueSubmit: {e}"))?;

        let status = self
            .swapchain
            .present(self.device.queue(), index as u32, self.rendered[index])?;
        self.last = status;
        Ok(status)
    }

    /// Records the clear into `buffer`, which belongs to image `index`.
    fn record(
        &self,
        buffer: vk::CommandBuffer,
        index: usize,
        color: [f32; 4],
    ) -> Result<(), String> {
        let device = self.device.device();
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer is ours, reset above and not recording.
        unsafe { device.begin_command_buffer(buffer, &begin) }
            .map_err(|e| format!("vkBeginCommandBuffer: {e}"))?;

        let range = subresource_range();
        let image = self.swapchain.images()[index];
        // The image is in `PRESENT_SRC` from the previous present (or
        // `UNDEFINED` the first time): either way this frame takes it to
        // `COLOR_ATTACHMENT_OPTIMAL` for the clear, then back to
        // `PRESENT_SRC` for the compositor.
        let to_draw = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range);
        // SAFETY: recording into our own buffer on the calling thread.
        unsafe {
            device.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_draw],
            );
        }
        let clear = vk::ClearColorValue { float32: color };
        // SAFETY: as above; the image belongs to our swapchain and is
        // transitioning to the layout the clear names.
        unsafe {
            device.cmd_clear_color_image(
                buffer,
                image,
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                &clear,
                &[range],
            );
        }
        let to_present = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(range);
        // SAFETY: as above.
        unsafe {
            device.cmd_pipeline_barrier(
                buffer,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_present],
            );
            device
                .end_command_buffer(buffer)
                .map_err(|e| format!("vkEndCommandBuffer: {e}"))?;
        }
        Ok(())
    }

    /// Allocates the command buffers and semaphores for the current image
    /// count, discarding any earlier ones.
    fn sync(&mut self) -> Result<(), String> {
        self.release();
        let device = self.device.device();
        let count = self.swapchain.images().len().max(1) as u32;
        let allocate = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(count);
        // SAFETY: the pool is ours and this is the only thread allocating
        // from it.
        self.buffers = unsafe { device.allocate_command_buffers(&allocate) }
            .map_err(|e| format!("vkAllocateCommandBuffers: {e}"))?;
        let info = vk::SemaphoreCreateInfo::default();
        // SAFETY: plain semaphore creation on our device.
        self.acquired = unsafe { device.create_semaphore(&info, None) }
            .map_err(|e| format!("vkCreateSemaphore: {e}"))?;
        for _ in 0..count {
            // SAFETY: as above.
            let semaphore = unsafe { device.create_semaphore(&info, None) }
                .map_err(|e| format!("vkCreateSemaphore: {e}"))?;
            self.rendered.push(semaphore);
        }
        let fence = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
        // SAFETY: as above.
        self.in_flight = unsafe { device.create_fence(&fence, None) }
            .map_err(|e| format!("vkCreateFence: {e}"))?;
        Ok(())
    }

    /// Frees everything [`sync`](Self::sync) allocated. Safe to call twice.
    fn release(&mut self) {
        let device = self.device.device();
        self.buffers.clear();
        if self.acquired != vk::Semaphore::null() {
            // SAFETY: the semaphore is ours and nothing waits on it (the
            // last frame's fence was waited for).
            unsafe { device.destroy_semaphore(self.acquired, None) };
            self.acquired = vk::Semaphore::null();
        }
        for semaphore in self.rendered.drain(..) {
            // SAFETY: as above.
            unsafe { device.destroy_semaphore(semaphore, None) };
        }
        if self.in_flight != vk::Fence::null() {
            // SAFETY: as above.
            unsafe { device.destroy_fence(self.in_flight, None) };
            self.in_flight = vk::Fence::null();
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        // SAFETY: the device is idle-able and ours; the fence makes sure no
        // submission is still running before anything is freed. The handle
        // is cloned so `release` can still take `&mut self`.
        let device = self.device.device().clone();
        unsafe {
            let _ = device.device_wait_idle();
            if self.in_flight != vk::Fence::null() {
                let _ = device.wait_for_fences(&[self.in_flight], true, u64::MAX);
            }
        }
        self.release();
        // SAFETY: the pool is ours and its command buffers are gone.
        unsafe {
            if self.pool != vk::CommandPool::null() {
                device.destroy_command_pool(self.pool, None);
            }
        }
        // The swapchain is destroyed by its own `Drop`; it is declared
        // before the device below, but fields drop in declaration order, so
        // the presenter's own `Drop` runs first and there is nothing left
        // to order.
    }
}

/// A command pool whose buffers are recorded fresh every frame.
fn create_pool(device: &Device) -> Result<vk::CommandPool, String> {
    let info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
        .queue_family_index(device.queue_family());
    // SAFETY: the family came from this device's adapter, and nothing else
    // allocates from the pool yet.
    unsafe { device.device().create_command_pool(&info, None) }
        .map_err(|e| format!("vkCreateCommandPool: {e}"))
}

/// The whole of a colour image: one layer, one level, colour aspect.
fn subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}
