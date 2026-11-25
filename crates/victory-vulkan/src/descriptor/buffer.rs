//! A descriptor heap backed by a buffer a shader reads by address.
//!
//! `VK_EXT_descriptor_buffer` is what makes the console's model work at all:
//! a title computes an address for a descriptor and the shader reads through
//! it, so the heap has to *be* memory with a device address, not a set of
//! driver-owned `VkDescriptorSet`s. The layout of an entry is
//! [`super::STRIDE`] bytes and its meaning is [`super::format_word`]; this
//! only owns the allocation and the address.
//!
//! Where the extension is absent the caller gets `Err` and can fall back to
//! descriptor sets; nothing here silently degrades, because a heap whose
//! addresses do not resolve is a black screen rather than an error.

use ash::vk;

/// A descriptor buffer: the heap's bytes, plus its device address.
pub struct Buffer {
    device: ash::Device,
    memory: vk::DeviceMemory,
    buffer: vk::Buffer,
    address: u64,
    size: u64,
    /// The bytes, when the allocation is host-visible. Held so the CPU side
    /// of the heap writes into the same memory the shader reads.
    mapped: *mut u8,
}

impl Buffer {
    /// Allocates a descriptor buffer of `bytes`.
    ///
    /// Requires `VK_EXT_descriptor_buffer` on the device; the caller checks
    /// for it before calling, because only it knows whether it has a
    /// fallback.
    pub fn new(
        device: &ash::Device,
        bytes: u64,
        memory_type: u32,
        host_visible: bool,
    ) -> Result<Buffer, String> {
        let create = vk::BufferCreateInfo::default()
            .size(bytes)
            .usage(
                vk::BufferUsageFlags::RESOURCE_DESCRIPTOR_BUFFER_EXT
                    | vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: a plain buffer creation on a valid device.
        let buffer = unsafe { device.create_buffer(&create, None) }
            .map_err(|e| format!("vkCreateBuffer(descriptor): {e}"))?;

        // SAFETY: the buffer was just created.
        let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
        let allocate = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        // SAFETY: the memory type index came from the caller, who resolved
        // it against this adapter's memory types.
        let memory = match unsafe { device.allocate_memory(&allocate, None) } {
            Ok(memory) => memory,
            Err(e) => {
                // SAFETY: the buffer exists and this function owns it.
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(format!("vkAllocateMemory(descriptor): {e}"));
            }
        };
        // SAFETY: `memory` is ours, `buffer` is its buffer, and the offset is
        // zero over a freshly allocated block.
        if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
            // SAFETY: both were created above and are owned here.
            unsafe {
                device.free_memory(memory, None);
                device.destroy_buffer(buffer, None);
            }
            return Err(format!("vkBindBufferMemory(descriptor): {e}"));
        }

        let address_info = vk::BufferDeviceAddressInfo::default().buffer(buffer);
        // SAFETY: the buffer is bound, and `SHADER_DEVICE_ADDRESS` was asked
        // for in its creation flags.
        let address = unsafe { device.get_buffer_device_address(&address_info) };

        let mut mapped = std::ptr::null_mut();
        if host_visible {
            // SAFETY: the memory is host-visible and not mapped yet.
            match unsafe { device.map_memory(memory, 0, bytes, vk::MemoryMapFlags::empty()) } {
                Ok(pointer) => mapped = pointer.cast(),
                Err(e) => {
                    // SAFETY: as above, in the reverse order of creation.
                    unsafe {
                        device.free_memory(memory, None);
                        device.destroy_buffer(buffer, None);
                    }
                    return Err(format!("vkMapMemory(descriptor): {e}"));
                }
            }
        }

        Ok(Buffer {
            device: device.clone(),
            memory,
            buffer,
            address,
            size: bytes,
            mapped,
        })
    }

    /// The device address a shader reads the heap through.
    pub fn address(&self) -> u64 {
        self.address
    }

    /// The heap's size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The heap's bytes, when the allocation is host-visible.
    pub fn mapped(&self) -> Option<*mut u8> {
        (!self.mapped.is_null()).then_some(self.mapped)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.mapped.is_null() {
            // SAFETY: mapped by this object, over this memory.
            unsafe { self.device.unmap_memory(self.memory) };
        }
        // SAFETY: both were created by this object, in this order.
        unsafe {
            self.device.free_memory(self.memory, None);
            self.device.destroy_buffer(self.buffer, None);
        }
    }
}
