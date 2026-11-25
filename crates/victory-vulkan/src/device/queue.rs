//! Queue families: which one the device asks for, and the priority its one
//! queue is created at.
//!
//! Graphics and compute together are the requirement, not two families: the
//! backend sends rendering and simulation down the same queue, and every
//! family that can do both can also transfer.

use ash::vk;

/// The flags the chosen family must have.
pub fn wanted() -> vk::QueueFlags {
    vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE
}

/// The first family that can do graphics and compute, if the adapter has
/// one. Vulkan makes no promise about the order, so this is simply the
/// lowest index that qualifies, which keeps the choice stable per machine.
pub fn family(families: &[vk::QueueFamilyProperties]) -> Option<u32> {
    families
        .iter()
        .position(|f| f.queue_flags.contains(wanted()))
        .map(|index| index as u32)
}

/// The priority of the only queue created: with one queue it may as well
/// have the whole device.
pub const PRIORITY: f32 = 1.0;

/// The create info for that one queue.
///
/// `priority` must outlive the `vkCreateDevice` call that reads it, because
/// the create info only borrows it.
pub fn create_info(queue_family: u32, priority: &[f32]) -> vk::DeviceQueueCreateInfo<'_> {
    vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(priority)
}
