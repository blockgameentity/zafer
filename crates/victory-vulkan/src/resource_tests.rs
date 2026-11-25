//! The memory policy. Pure arithmetic and classification, so no driver.

use ash::vk;

use crate::resource::*;

fn ty(flags: vk::MemoryPropertyFlags) -> vk::MemoryType {
    vk::MemoryType {
        property_flags: flags,
        heap_index: 0,
    }
}

#[test]
fn a_reservation_rounds_up_to_the_granularity() {
    assert_eq!(reservation_size(1), GRANULARITY);
    assert_eq!(reservation_size(GRANULARITY), GRANULARITY);
    assert_eq!(reservation_size(GRANULARITY + 1), 2 * GRANULARITY);
    assert_eq!(reservation_size(3 * GRANULARITY), 3 * GRANULARITY);
}

#[test]
fn a_reservation_is_never_zero() {
    assert_eq!(reservation_size(0), GRANULARITY);
}

#[test]
fn an_absurd_reservation_saturates() {
    let huge = reservation_size(u64::MAX);
    assert!(huge >= GRANULARITY);
    assert_eq!(huge % GRANULARITY, 0);
}

#[test]
fn alignment_rounds_up_and_leaves_aligned_offsets_alone() {
    assert_eq!(align(0, 256), Some(0));
    assert_eq!(align(256, 256), Some(256));
    assert_eq!(align(1, 256), Some(256));
    assert_eq!(align(257, 256), Some(512));
}

#[test]
fn a_zero_alignment_is_treated_as_one() {
    assert_eq!(align(7, 0), Some(7));
}

#[test]
fn alignment_that_would_overflow_is_none() {
    assert_eq!(align(u64::MAX, 4096), None);
}

#[test]
fn place_honours_the_alignment_and_the_capacity() {
    assert_eq!(place(0, 1024, 512, 256), Some(0));
    assert_eq!(place(100, 1024, 512, 256), Some(256));
    assert_eq!(place(0, 1024, 2048, 256), None);
    assert_eq!(place(1000, 1024, 32, 256), None);
}

#[test]
fn place_refuses_to_wrap() {
    assert_eq!(place(u64::MAX - 8, u64::MAX, 64, 8), None);
}

#[test]
fn inside_checks_the_end_and_the_wrap() {
    assert!(inside(0, 1024, 1024));
    assert!(inside(512, 512, 1024));
    assert!(!inside(512, 513, 1024));
    assert!(!inside(0, 1025, 1024));
    assert!(!inside(u64::MAX, 2, u64::MAX));
}

#[test]
fn the_gpu_classes_ask_for_device_local() {
    for class in [Class::Upload, Class::Device] {
        assert!(
            class
                .properties()
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL),
            "{class:?}"
        );
    }
}

#[test]
fn readback_asks_for_cached_host_memory_instead() {
    let properties = Class::Readback.properties();
    assert!(properties.contains(vk::MemoryPropertyFlags::HOST_VISIBLE));
    assert!(properties.contains(vk::MemoryPropertyFlags::HOST_CACHED));
    assert!(!properties.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL));
}

#[test]
fn only_the_host_classes_are_host_visible() {
    assert!(Class::Upload.properties().contains(vk::MemoryPropertyFlags::HOST_VISIBLE));
    assert!(Class::Readback.properties().contains(vk::MemoryPropertyFlags::HOST_VISIBLE));
    assert!(!Class::Device.properties().contains(vk::MemoryPropertyFlags::HOST_VISIBLE));
}

#[test]
fn only_the_host_classes_are_mapped() {
    assert!(Class::Upload.is_mapped());
    assert!(Class::Readback.is_mapped());
    assert!(!Class::Device.is_mapped());
}

#[test]
fn device_memory_and_uploads_carry_addresses() {
    assert!(Class::Device.needs_address());
    assert!(Class::Upload.needs_address());
    assert!(!Class::Readback.needs_address());
}

#[test]
fn the_first_matching_type_wins_and_respects_the_mask() {
    let properties = [
        ty(vk::MemoryPropertyFlags::DEVICE_LOCAL),
        ty(vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT),
        ty(vk::MemoryPropertyFlags::DEVICE_LOCAL
            | vk::MemoryPropertyFlags::HOST_VISIBLE
            | vk::MemoryPropertyFlags::HOST_COHERENT),
    ];
    let want = Class::Upload.properties();
    assert_eq!(memory_type(u32::MAX, &properties, want), Some(2));
    // Type 0 is excluded by the mask even though it is the only one left.
    assert_eq!(memory_type(0b100, &properties, want), Some(2));
    assert_eq!(memory_type(0b001, &properties, want), None);
}

#[test]
fn a_missing_class_is_named() {
    let properties = [ty(vk::MemoryPropertyFlags::DEVICE_LOCAL)];
    let absent = missing(&properties);
    assert!(absent.contains(&Class::Upload));
    assert!(absent.contains(&Class::Readback));
    assert!(!absent.contains(&Class::Device));
}

#[test]
fn nothing_is_missing_when_one_type_covers_all() {
    let everything = vk::MemoryPropertyFlags::DEVICE_LOCAL
        | vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::HOST_CACHED;
    assert!(missing(&[ty(everything)]).is_empty());
}
