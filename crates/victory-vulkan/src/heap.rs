//! A heap of GPU memory with a stable CPU pointer.
//!
//! This is where the console's memory model does not bend to Vulkan's. A
//! `VkDeviceMemory` is a handle; the console hands a title an address it
//! can dereference and pass to a shader. So a heap allocates one
//! `VkDeviceMemory`, maps it for life when the class is host-visible, and
//! answers [`Heap::pointer`] with a CPU address — which on a heap that
//! carries a buffer device address is also the address a shader sees, via
//! [`Heap::address`].
//!
//! Suballocation is [`crate::resource::place`]: the heap is a bump pointer
//! and the console's granularity does the rounding, so a title's
//! `VirtualAlloc` becomes an offset rather than a `VkDeviceMemory` of its
//! own. That is what keeps a title's thousands of small allocations from
//! exhausting the driver's allocation limit.

use std::ptr::NonNull;

use ash::vk;

use crate::resource::{Class, reservation_size};

/// A heap of device memory and the CPU pointer that stands for it.
pub struct Heap {
    memory: vk::DeviceMemory,
    size: u64,
    /// Where the CPU sees it, when the class is host-visible. `None` for a
    /// heap only the GPU touches, which has no CPU address at all.
    mapped: Option<NonNull<u8>>,
    /// A buffer whose address covers the heap, when it needs one.
    ///
    /// The console's GPU addresses are real pointers; Vulkan's equivalent is
    /// `vkGetBufferDeviceAddress`, which requires *a buffer*. One buffer
    /// spanning the whole heap gives every suballocation an address by
    /// offset, so a title's pointer arithmetic still lands on the right
    /// byte.
    buffer: Option<vk::Buffer>,
    /// The `vk::DeviceAddress` [`Heap::buffer`] reports at offset zero.
    base_address: u64,
    class: Class,
}

impl Heap {
    /// Allocates `bytes` of `class` memory, rounded to the console's
    /// granularity, and maps it when the class is host-visible.
    ///
    /// # Safety
    ///
    /// `memory_type` must be a memory type index the device reports, and it
    /// must have the properties `class` requires — checked by
    /// [`crate::resource::missing`]. The caller also owns the guarantee
    /// that nothing else frees `device` while this heap lives.
    pub unsafe fn new(
        device: &ash::Device,
        memory_type: u32,
        class: Class,
        bytes: u64,
        addressable: bool,
    ) -> Result<Heap, String> {
        let size = reservation_size(bytes);
        let info = vk::MemoryAllocateInfo::default()
            .allocation_size(size)
            .memory_type_index(memory_type);
        // A heap that carries an address needs the flag at allocation time:
        // Vulkan will not hand out an address for memory allocated without
        // it.
        // `addressable` is false when the device did not enable
        // `VK_KHR_buffer_device_address`, and then asking for an address is
        // an ash panic rather than a Vulkan error, so the decision is made
        // here from what the device actually got.
        let mut flags = vk::MemoryAllocateFlagsInfo::default();
        if class.needs_address() && addressable {
            flags = flags.flags(vk::MemoryAllocateFlags::DEVICE_ADDRESS);
            flags.p_next = std::ptr::null_mut();
        }
        let info = if class.needs_address() && addressable {
            let mut info = info;
            info.p_next = (&flags as *const vk::MemoryAllocateFlagsInfo).cast();
            info
        } else {
            info
        };
        // SAFETY: the caller vouches for `memory_type`, and `info` describes
        // one allocation with an outliving descriptor.
        let memory = unsafe { device.allocate_memory(&info, None) }.map_err(|e| {
            tracing::error!(size, memory_type, "vkAllocateMemory failed: {e}");
            format!("vkAllocateMemory({size} bytes) failed: {e}")
        })?;
        tracing::debug!(
            size,
            memory_type,
            class = ?class,
            addressable = class.needs_address() && addressable,
            "heap allocated"
        );

        // The mapping is kept for the process's life for a host-visible
        // class: the console's model says the pointer never moves, and an
        // unmap/remap would break every address a title already holds.
        let mapped = if class.is_mapped() {
            // SAFETY: `memory` was just allocated with these properties and
            // is not freed while this heap lives.
            let p = unsafe { device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) }
                .map_err(|e| format!("vkMapMemory({size} bytes) failed: {e}"))?;
            NonNull::new(p.cast::<u8>())
        } else {
            None
        };

        let (buffer, base_address) = if class.needs_address() && addressable {
            match create_address_buffer(device, size) {
                Ok((buffer, base)) => (Some(buffer), base),
                Err(e) => {
                    // Undo what was made, so a failure here leaves nothing
                    // behind for the caller to track.
                    // SAFETY: both were created above and are unused.
                    unsafe {
                        if let Some(p) = mapped {
                            device.unmap_memory(memory);
                            let _ = p;
                        }
                        device.free_memory(memory, None);
                    }
                    return Err(e);
                }
            }
        } else {
            (None, 0)
        };

        Ok(Heap {
            memory,
            size,
            mapped,
            buffer,
            base_address,
            class,
        })
    }

    /// The size in bytes, already rounded.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The class this heap was made for.
    pub fn class(&self) -> Class {
        self.class
    }

    /// The CPU address of offset zero, for a host-visible heap.
    ///
    /// This is the pointer a title's `VirtualAlloc` returns to it.
    pub fn pointer(&self) -> Option<NonNull<u8>> {
        self.mapped
    }

    /// The GPU address of offset zero, for a heap that carries one.
    pub fn address(&self) -> Option<u64> {
        self.buffer.map(|_| self.base_address)
    }

    /// The `VkDeviceMemory` behind the heap.
    pub fn memory(&self) -> vk::DeviceMemory {
        self.memory
    }

    /// The buffer whose address covers the heap, if it has one.
    pub fn buffer(&self) -> Option<vk::Buffer> {
        self.buffer
    }

    /// Reveals the bytes at `offset..offset + len` to `closure`.
    ///
    /// # Safety
    ///
    /// The caller must not concurrently read or write the same range, and
    /// `offset + len` must be inside the heap. This is the only way the
    /// contents are reachable, so the safety of every read and write goes
    /// through here rather than being assumed at each call site.
    pub unsafe fn with<T>(&self, offset: u64, len: u64, f: impl FnOnce(&mut [u8]) -> T) -> Option<T> {
        let base = self.mapped?;
        if offset.checked_add(len)? > self.size {
            return None;
        }
        // SAFETY: checked to be inside the mapping above, and the caller
        // promises no aliasing access.
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(base.as_ptr().add(offset as usize), len as usize)
        };
        Some(f(bytes))
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // The device is not stored here (a heap outliving its device is a
        // use-after-free the type system cannot see), so freeing is the
        // owner's job through [`Heap::destroy`].
        let _ = self.memory;
    }
}

impl Heap {
    /// Frees the heap's memory and buffer.
    ///
    /// # Safety
    ///
    /// `device` must be the device the heap was allocated on, nothing may
    /// be using the memory (no recording command buffer, no mapping taken
    /// through [`Heap::with`]), and this must be called at most once.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        tracing::debug!(size = self.size, class = ?self.class, "heap destroyed");
        if let Some(buffer) = self.buffer.take() {
            // SAFETY: created by this heap and not destroyed before.
            unsafe { device.destroy_buffer(buffer, None) };
        }
        if self.mapped.take().is_some() {
            // SAFETY: mapped by this heap and not unmapped before.
            unsafe { device.unmap_memory(self.memory) };
        }
        // SAFETY: allocated by this heap and not freed before.
        unsafe { device.free_memory(self.memory, None) };
    }
}

/// A buffer spanning `size` bytes, and the device address it reports.
fn create_address_buffer(device: &ash::Device, size: u64) -> Result<(vk::Buffer, u64), String> {
    let info = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: `info` describes one buffer and outlives the call.
    let buffer = unsafe { device.create_buffer(&info, None) }
        .map_err(|e| format!("vkCreateBuffer({size} bytes) failed: {e}"))?;
    let address_info = vk::BufferDeviceAddressInfo::default().buffer(buffer);
    // SAFETY: `buffer` was just created on this device.
    let address = unsafe { device.get_buffer_device_address(&address_info) };
    Ok((buffer, address))
}
