//! The descriptor heap tests. Pure data, so they need no Vulkan driver.

use crate::descriptor::*;
use ash::vk;

/// An entry whose format word carries `tag`, with a plausible format
/// selector in the bits above it.
fn tagged(tag: u32, selector: u32) -> Entry {
    let mut entry = Entry::default();
    entry.0[0] = tag | (selector << 8);
    entry
}

#[test]
fn a_zeroed_entry_is_unknown() {
    assert_eq!(Kind::of(&Entry::default()), Kind::Unknown);
    assert_eq!(Kind::of(&Entry([0; 8])), Kind::Unknown);
}

#[test]
fn each_tag_classifies_to_its_kind() {
    for kind in [
        Kind::SampledImage,
        Kind::StorageImage,
        Kind::UniformBuffer,
        Kind::StorageBuffer,
        Kind::InlineUniform,
    ] {
        assert_eq!(Kind::of(&tagged(kind.tag(), 0)), kind);
        assert_eq!(Kind::of(&tagged(kind.tag(), 0xff)), kind);
        assert_ne!(kind, Kind::Unknown);
    }
}

#[test]
fn reserved_tags_are_unknown() {
    for tag in [6u32, 7, 0x7f, 0xfe, 0xff] {
        assert_eq!(Kind::of(&tagged(tag, 0)), Kind::Unknown, "tag {tag}");
    }
}

#[test]
fn only_the_format_word_decides_the_kind() {
    let mut entry = tagged(Kind::StorageBuffer.tag(), 0);
    entry.0[1..].fill(u32::MAX);
    assert_eq!(Kind::of(&entry), Kind::StorageBuffer);
    assert_eq!(format_word(&entry), entry.0[0]);
}

#[test]
fn entries_are_comparable_and_copyable() {
    let a = tagged(3, 0);
    let b = a;
    assert_eq!(a, b);
    assert_eq!(Entry::default(), Entry([0; 8]));
    assert_ne!(tagged(3, 0), tagged(3, 1));
}

#[test]
fn every_known_kind_has_a_binding() {
    let every = vk::ShaderStageFlags::FRAGMENT
        | vk::ShaderStageFlags::VERTEX
        | vk::ShaderStageFlags::COMPUTE;
    for kind in [
        Kind::SampledImage,
        Kind::StorageImage,
        Kind::UniformBuffer,
        Kind::StorageBuffer,
        Kind::InlineUniform,
    ] {
        let b = binding(kind, 7).expect("a known kind has a binding");
        assert_eq!(b.binding, 7);
        assert_eq!(b.descriptor_count, 1);
        assert_eq!(b.descriptor_type, kind.descriptor_type().unwrap());
        assert_eq!(b.stage_flags, every);
    }
}

#[test]
fn the_binding_types_are_distinct_per_kind() {
    let mut types: Vec<vk::DescriptorType> = [
        Kind::SampledImage,
        Kind::StorageImage,
        Kind::UniformBuffer,
        Kind::StorageBuffer,
        Kind::InlineUniform,
    ]
    .into_iter()
    .map(|k| binding(k, 0).unwrap().descriptor_type)
    .collect();
    types.sort_by_key(|t| t.as_raw());
    types.dedup();
    assert_eq!(types.len(), 5);
}

#[test]
fn unknown_has_no_binding() {
    assert!(binding(Kind::Unknown, 0).is_none());
    assert!(Kind::Unknown.descriptor_type().is_none());
}

#[test]
fn a_new_heap_is_empty_and_keeps_its_limit() {
    let heap = Heap::new(8);
    assert_eq!(heap.len(), 0);
    assert!(heap.is_empty());
    assert_eq!(heap.capacity(), 8);
    assert!(heap.get(0).is_none());
    assert_eq!(Heap::default(), Heap::new(0));
}

#[test]
fn set_grows_the_heap_with_zeroed_entries() {
    let mut heap = Heap::new(4);
    heap.set(2, tagged(1, 0)).unwrap();
    assert_eq!(heap.len(), 3);
    assert_eq!(heap.get(0), Some(&Entry::default()));
    assert_eq!(heap.get(1), Some(&Entry::default()));
    assert_eq!(heap.get(2), Some(&tagged(1, 0)));
    assert!(heap.get(3).is_none());
    // Writing inside the used prefix does not grow the heap again.
    heap.set(0, tagged(2, 0)).unwrap();
    assert_eq!(heap.len(), 3);
    assert_eq!(Kind::of(heap.get(0).unwrap()), Kind::StorageImage);
}

#[test]
fn the_last_slot_at_capacity_is_writable() {
    let mut heap = Heap::new(1);
    heap.set(0, tagged(4, 0)).unwrap();
    assert_eq!(heap.len(), 1);
    assert_eq!(heap.capacity(), 1);
    assert!(heap.get(1).is_none());
    assert_eq!(binding(Kind::of(heap.get(0).unwrap()), 0).unwrap().binding, 0);
}

#[test]
fn writing_past_capacity_errors_and_changes_nothing() {
    let mut heap = Heap::new(2);
    heap.set(1, tagged(1, 0)).unwrap();
    let err = heap.set(2, tagged(5, 0)).unwrap_err();
    assert!(err.contains("capacity"), "{err}");
    assert_eq!(heap.len(), 2);
    assert!(heap.get(2).is_none());

    let mut none = Heap::new(0);
    assert!(none.set(0, Entry::default()).is_err());
    assert_eq!(none.len(), 0);
}

#[test]
fn addresses_step_by_the_console_stride() {
    let base = 0x1_0000;
    assert_eq!(STRIDE, 32);
    assert_eq!(address_of(base, 0), base);
    assert_eq!(address_of(base, 1), base + 32);
    assert_eq!(address_of(base, 3) - address_of(base, 2), STRIDE);
    assert_eq!(address_of(base, 100) - base, 100 * STRIDE);
}

#[test]
fn addresses_round_trip_through_their_index() {
    let base = 0xffff_0000;
    for index in [0u32, 1, 2, 255, 1 << 20] {
        assert_eq!(index_of(base, address_of(base, index)), Some(index));
    }
    assert_eq!(index_of(0, address_of(0, u32::MAX)), Some(u32::MAX));
}

#[test]
fn addresses_off_the_grid_have_no_index() {
    let base = 0x1000;
    assert_eq!(index_of(base, base - 1), None);
    assert_eq!(index_of(base, base - STRIDE), None);
    assert_eq!(index_of(base, base + 1), None);
    assert_eq!(index_of(base, base + STRIDE - 1), None);
    assert_eq!(index_of(base, base + STRIDE + 1), None);
    // Past the last index a u32 handle can name.
    assert_eq!(index_of(0, (u64::from(u32::MAX) + 1) * STRIDE), None);
}
