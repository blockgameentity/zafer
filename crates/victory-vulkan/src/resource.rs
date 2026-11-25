//! The console's GPU memory, answered from the Vulkan side.
//!
//! The console's model is Vulkan's: memory is unified, a GPU address is a
//! CPU address, and the title may hand a raw address to a shader expecting
//! the same bytes it can read itself. That makes the memory layer of this
//! backend unusual — it is a heap with a stable CPU pointer, not an
//! allocator whose resources are opaque handles.
//!
//! This module holds the *policy* of that layer as pure functions, so it is
//! testable without a driver. The allocations themselves come from
//! [`crate::device`] and live in the command layer.

/// The alignment every heap base satisfies. Vulkan requires a
/// `VkDeviceMemory` to be allocated with an alignment that makes its
/// offsets legal for every binding type, and 64 KiB is the largest a
/// driver may report for `VkPhysicalDeviceLimits::minMemoryMapAlignment`
/// in practice; the console's pages are the same size.
pub const HEAP_ALIGNMENT: u64 = 64 * 1024;

/// The console's allocation granularity: reservations round up to it.
pub const GRANULARITY: u64 = 64 * 1024;

/// A memory class: how a heap will be used decides which
/// `vk::MemoryPropertyFlags` it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Written by the CPU and read by the GPU (constant buffers, staging).
    /// Unified memory means this is also the fast path on the console.
    Upload,
    /// Written by the GPU and read by the CPU (readback, statistics).
    Readback,
    /// Only the GPU touches it (render targets, textures, the command
    /// stream a title builds in its own memory).
    Device,
}

impl Class {
    /// The memory properties this class needs.
    ///
    /// The console has no separate device-local pool, so a class that the
    /// GPU alone reads asks for `DEVICE_LOCAL` too: on a discrete host that
    /// is exactly the VRAM a title's heaps belong in, and on integrated
    /// hardware it is already host memory.
    ///
    /// [`Class::Readback`] is the exception and deliberately so. The CPU
    /// reads it back, and a read of uncached device-local memory is
    /// pathologically slow, so it asks for cached host memory instead. On
    /// hardware where that type does not exist the requirement fails
    /// visibly through [`missing`] rather than silently turning every
    /// readback into a stall.
    pub fn properties(self) -> ash::vk::MemoryPropertyFlags {
        use ash::vk::MemoryPropertyFlags as F;
        match self {
            Class::Upload => F::HOST_VISIBLE | F::HOST_COHERENT | F::DEVICE_LOCAL,
            Class::Readback => F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
            Class::Device => F::DEVICE_LOCAL,
        }
    }

    /// Whether the CPU may keep a mapped pointer to this class.
    ///
    /// The console's model says a GPU address is dereferenceable, so the
    /// classes that a title writes through a pointer stay mapped for life.
    pub fn is_mapped(self) -> bool {
        matches!(self, Class::Upload | Class::Readback)
    }

    /// Whether the GPU can read an address into this memory as a buffer
    /// device address, which is how a title's pointers reach a shader.
    pub fn needs_address(self) -> bool {
        matches!(self, Class::Device | Class::Upload)
    }
}

/// The size to allocate for a reservation of `bytes`.
///
/// Rounds up to [`GRANULARITY`], never returns zero (Vulkan rejects a
/// zero-size allocation), and saturates rather than wrapping for a size
/// near `u64::MAX`, which a title can ask for with an absurd
/// `VirtualAlloc`.
pub fn reservation_size(bytes: u64) -> u64 {
    let rounded = bytes
        .checked_add(GRANULARITY - 1)
        .map(|n| n / GRANULARITY * GRANULARITY)
        .unwrap_or(u64::MAX - (u64::MAX % GRANULARITY));
    rounded.max(GRANULARITY)
}

/// `offset` rounded up to the alignment of the binding it will have.
///
/// `alignment` comes from `vkGetBufferMemoryRequirements` or
/// `vkGetImageMemoryRequirements`; a value of zero (no requirement) is
/// treated as one, so callers may pass the driver's answer through.
pub fn align(offset: u64, alignment: u64) -> Option<u64> {
    let alignment = alignment.max(1);
    let remainder = offset % alignment;
    if remainder == 0 {
        return Some(offset);
    }
    offset.checked_add(alignment - remainder)
}

/// Where a suballocation of `size` fits in a heap of `capacity` starting at
/// `cursor`, aligned as asked: the offset, or `None` if it does not fit.
///
/// This is the whole allocator of the memory layer. The console's titles
/// reserve once and suballocate themselves, so what a port needs is the
/// arithmetic that decides where a range may start, not a general-purpose
/// allocator that would fragment what the title already planned.
pub fn place(cursor: u64, capacity: u64, size: u64, alignment: u64) -> Option<u64> {
    let start = align(cursor, alignment)?;
    let end = start.checked_add(size)?;
    (end <= capacity).then_some(start)
}

/// Whether `range` of `len` bytes lies inside a heap of `capacity`.
pub fn inside(offset: u64, len: u64, capacity: u64) -> bool {
    match offset.checked_add(len) {
        Some(end) => end <= capacity,
        None => false,
    }
}

/// The memory type the driver offered that has every property of `want`,
/// preferring the first, which Vulkan orders best-first.
///
/// `types` is the `memory_type_bits` mask from a memory-requirements query
/// and `properties` is the adapter's list, so the choice can be made without
/// an allocation having happened.
pub fn memory_type(
    types: u32,
    properties: &[ash::vk::MemoryType],
    want: ash::vk::MemoryPropertyFlags,
) -> Option<u32> {
    properties
        .iter()
        .enumerate()
        .find(|(index, ty)| {
            types & (1 << index) != 0 && ty.property_flags.contains(want)
        })
        .map(|(index, _)| index as u32)
}

/// Whether every type Vulkan offered fails the classes the backend needs.
///
/// Reported per class so a run can say *which* capability is missing rather
/// than that memory allocation failed.
pub fn missing(properties: &[ash::vk::MemoryType]) -> Vec<Class> {
    [Class::Upload, Class::Readback, Class::Device]
        .into_iter()
        .filter(|class| {
            memory_type(u32::MAX, properties, class.properties()).is_none()
        })
        .collect()
}
