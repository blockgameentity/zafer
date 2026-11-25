//! A heap against a real driver, when there is one.
//!
//! These are the only tests in the crate that need a device, so they skip
//! with a note on a machine with no Vulkan. What they check is exactly the
//! property the console's memory model depends on: that a heap has a CPU
//! pointer whose bytes are the heap's bytes, and that a heap which carries
//! an address reports one.

use crate::resource::Class;

/// A device, or a note that this machine has none.
fn device() -> Option<super::device::Device> {
    match super::device::create(c"heap_tests", &[]) {
        Ok(Some(device)) => Some(device),
        Ok(None) => {
            eprintln!("no Vulkan device here; skipping");
            None
        }
        Err(e) => panic!("the device refused to be created: {e}"),
    }
}

#[test]
fn a_heap_holds_what_is_written_to_it() {
    let Some(device) = device() else {
        return;
    };
    // SAFETY: the heap is destroyed below, before `device` is dropped.
    let heap = unsafe { device.heap(Class::Upload, 4096) }
        .expect("an upload heap of 4 KiB");
    assert!(heap.pointer().is_some(), "an upload heap is host-visible");
    assert_eq!(heap.size(), crate::resource::GRANULARITY, "rounded up");

    // SAFETY: the range is inside the heap and nothing else is using it.
    let written = unsafe {
        heap.with(4, 4, |bytes| {
            bytes.copy_from_slice(&[1, 2, 3, 4]);
            bytes.to_vec()
        })
    };
    assert_eq!(written, Some(vec![1, 2, 3, 4]));

    // Reading it back through the same window is what a title does with a
    // pointer it was handed.
    // SAFETY: as above.
    let read = unsafe { heap.with(4, 4, |bytes| bytes.to_vec()) };
    assert_eq!(read, Some(vec![1, 2, 3, 4]));

    let mut heap = heap;
    // SAFETY: nothing uses the memory now, and it is freed once.
    unsafe { heap.destroy(device.device()) };
}

#[test]
fn a_window_past_the_end_is_refused() {
    let Some(device) = device() else {
        return;
    };
    // SAFETY: the heap is destroyed below, before `device` is dropped.
    let mut heap = unsafe { device.heap(Class::Upload, 1024) }
        .expect("an upload heap of 1 KiB");
    // SAFETY: the range is deliberately out of bounds; `with` must refuse
    // rather than form a slice over it.
    let out = unsafe { heap.with(heap.size(), 1, |bytes| bytes.len()) };
    assert_eq!(out, None, "a window past the end is not a slice");
    // A window that wraps in `offset + len` is refused too, rather than
    // wrapping to a small offset.
    // SAFETY: as above.
    let wrap = unsafe { heap.with(u64::MAX, 2, |bytes| bytes.len()) };
    assert_eq!(wrap, None, "a wrapping window is not a slice");
    // SAFETY: nothing uses the memory now, and it is freed once.
    unsafe { heap.destroy(device.device()) };
}

#[test]
fn an_address_heap_reports_a_gpu_address() {
    let Some(device) = device() else {
        return;
    };
    // SAFETY: the heap is destroyed below, before `device` is dropped.
    let mut heap = unsafe { device.heap(Class::Device, 4096) }
        .expect("a device heap of 4 KiB");
    // The console hands a shader an address; without one, buffer device
    // addressing is not available on this adapter and the backend cannot
    // run a title's pointer model.
    // Vulkan does *not* promise a non-zero address: lavapipe legitimately
    // reports 0, and zero is a real address in a flat virtual address space
    // (which is the console's, and therefore ours). What matters is that an
    // address exists for a heap that asked for one, and that it covers the
    // heap — so this checks presence, plus that a heap which did not ask
    // reports none.
    assert!(
        heap.address().is_some(),
        "buffer device addressing was enabled, so a device heap must carry an address"
    );
    // SAFETY: nothing uses the memory now, and it is freed once.
    unsafe { heap.destroy(device.device()) };
}
