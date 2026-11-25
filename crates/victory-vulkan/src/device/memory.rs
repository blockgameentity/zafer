//! Allocating a heap on the device, with the type the class needs.
//!
//! The caller names a [`Class`], not a memory-type index: which index has
//! the right properties is a property of the machine, and a title has no
//! idea. [`crate::resource::memory_type`] does the lookup, and this checks
//! the answer before touching the driver so a missing capability is a clear
//! error rather than a `vkAllocateMemory` failure.

use super::Device;
use crate::heap::Heap;
use crate::resource::{Class, memory_type};

impl Device {
    /// Creates a heap of `class` memory holding at least `bytes`.
    ///
    /// # Safety
    ///
    /// The returned heap borrows nothing, but its memory belongs to `self`,
    /// so it must be [destroyed](Heap::destroy) before this device is
    /// dropped. Nothing else may have been handed the address of the memory
    /// yet.
    pub unsafe fn heap(&self, class: Class, bytes: u64) -> Result<Heap, String> {
        // SAFETY: the device is live, which `&self` guarantees.
        let properties = unsafe {
            self.instance()
                .get_physical_device_memory_properties(self.physical())
        };
        let want = class.properties();
        let Some(index) = memory_type(u32::MAX, &properties.memory_types, want) else {
            return Err(format!(
                "no memory type has {want:?}, which {class:?} needs \
                 (the adapter reports {} types)",
                properties.memory_types.len()
            ));
        };
        // SAFETY: the index came from the adapter's own table, and the
        // caller promises to free the heap before the device goes.
        unsafe {
            Heap::new(
                self.device(),
                index,
                class,
                bytes,
                self.extensions.buffer_device_address,
            )
        }
    }
}
