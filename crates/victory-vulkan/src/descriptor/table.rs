//! `ash` layout description a real Vulkan descriptor set needs.
//!
//! A console descriptor heap is simply a block of memory the title owns. A
//! descriptor handle is an *address* into that block, so a game is free to
//! read or even write an entry directly — there is no API call in between
//! and no object the driver tracks. That rules out modelling the heap as a
//! `VkDescriptorSet`: engine code would have to go through Vulkan for what
//! was a store instruction, and a written-to entry would be invisible to
//! the driver's descriptor cache. So the authoritative copy here is a
//! CPU-side `Vec<Entry>`, and a one-off dirty region is re-uploaded when it
//! changes.
//!
//! `VK_EXT_descriptor_buffer` is the eventual home for that GPU-side copy:
//! it hands us a buffer of descriptors addressed by device address, which
//! is the console's model almost verbatim, so `binding` below exists only
//! to describe one entry's layout while the buffer path is built.
//!
//! // TODO: upload `Heap::entries` into a `VK_EXT_descriptor_buffer`
//! buffer once the extension is negotiated, and derive handles from that
//! buffer's device address instead of a fake `base`.
//!
//! Nothing here touches a `VkInstance` or `VkDevice`, so the whole module
//! builds and tests on a machine with no Vulkan driver.

use ash::vk;

/// The console's descriptor stride: handles step by 32 bytes.
pub const STRIDE: u64 = 32;

/// One 32-byte console descriptor entry, as eight opaque words.
///
/// The words are kept raw: the title may rewrite any of them directly, so
/// interpreting them is a per-use decision (`Kind::of`) rather than
/// something enforced at construction.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Entry(pub [u32; 8]);

/// Word 0 is the format word, laid out as
/// `bits 0..=7 kind tag, bits 8..=15 format selector, bits 16..=31 zero`.
///
/// The selector is kind-specific (a sampled image's pixel format, a
/// buffer's element index, and so on) and is not interpreted here; only
/// the tag decides the Vulkan descriptor type. Any reserved tag is
/// `Unknown`, which is what a title-written or uninitialised entry looks
/// like.
const KIND_SHIFT: u32 = 0;
const KIND_MASK: u32 = 0xff;

/// The format word of an entry — the only word `Kind::of` reads.
pub fn format_word(entry: &Entry) -> u32 {
    entry.0[0]
}

/// What an entry describes, as far as the GPU is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    SampledImage,
    StorageImage,
    UniformBuffer,
    StorageBuffer,
    InlineUniform,
    Unknown,
}

impl Kind {
    /// Classify an entry from its format word. A zeroed entry is `Unknown`.
    pub fn of(entry: &Entry) -> Kind {
        let tag = (format_word(entry) >> KIND_SHIFT) & KIND_MASK;
        match tag {
            1 => Kind::SampledImage,
            2 => Kind::StorageImage,
            3 => Kind::UniformBuffer,
            4 => Kind::StorageBuffer,
            5 => Kind::InlineUniform,
            _ => Kind::Unknown,
        }
    }

    /// The format word tag that `Kind::of` reports for this kind.
    pub fn tag(self) -> u32 {
        match self {
            Kind::SampledImage => 1,
            Kind::StorageImage => 2,
            Kind::UniformBuffer => 3,
            Kind::StorageBuffer => 4,
            Kind::InlineUniform => 5,
            // 0 is the canonical "nothing", and any other value would make
            // an entry that classifies as something else.
            Kind::Unknown => 0,
        }
    }

    /// The Vulkan descriptor type, or `None` for `Unknown`.
    pub fn descriptor_type(self) -> Option<vk::DescriptorType> {
        Some(match self {
            Kind::SampledImage => vk::DescriptorType::SAMPLED_IMAGE,
            Kind::StorageImage => vk::DescriptorType::STORAGE_IMAGE,
            Kind::UniformBuffer => vk::DescriptorType::UNIFORM_BUFFER,
            Kind::StorageBuffer => vk::DescriptorType::STORAGE_BUFFER,
            Kind::InlineUniform => vk::DescriptorType::INLINE_UNIFORM_BLOCK,
            Kind::Unknown => return None,
        })
    }
}

/// Describe one entry as a set-layout binding at `binding`.
///
/// Always one descriptor, always visible to every graphics stage we may run
/// a console shader on. `Unknown` has no descriptor type, so it has no
/// binding either.
pub fn binding(kind: Kind, binding: u32) -> Option<vk::DescriptorSetLayoutBinding<'static>> {
    let descriptor_type = kind.descriptor_type()?;
    Some(vk::DescriptorSetLayoutBinding {
        binding,
        descriptor_type,
        descriptor_count: 1,
        stage_flags: vk::ShaderStageFlags::FRAGMENT
            | vk::ShaderStageFlags::VERTEX
            | vk::ShaderStageFlags::COMPUTE,
        ..Default::default()
    })
}

/// A CPU-side descriptor heap: entries grow on demand up to `capacity`.
///
/// This mirrors the console's heap, where a handle's index is its byte
/// offset over [`STRIDE`]. Growth is lazy so a title that reserves a large
/// heap but uses ten entries costs ten entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Heap {
    entries: Vec<Entry>,
    capacity: usize,
}

impl Heap {
    /// An empty heap that will never exceed `capacity` entries.
    pub fn new(capacity: usize) -> Heap {
        Heap { entries: Vec::new(), capacity }
    }

    /// The entry at `index`, or `None` past the end (or past the capacity).
    pub fn get(&self, index: u32) -> Option<&Entry> {
        self.entries.get(usize::try_from(index).ok()?)
    }

    /// Write `entry` at `index`, growing the heap with zeroed entries as
    /// needed. `index` must be below the capacity.
    pub fn set(&mut self, index: u32, entry: Entry) -> Result<(), String> {
        let index = usize::try_from(index).map_err(|_| format!("descriptor index {index} is out of range"))?;
        if index >= self.capacity {
            return Err(format!(
                "descriptor index {index} is past the heap's capacity of {}",
                self.capacity
            ));
        }
        if index >= self.entries.len() {
            self.entries.resize(index + 1, Entry::default());
        }
        self.entries[index] = entry;
        Ok(())
    }

    /// The number of entries that currently exist, which is the used prefix
    /// of the heap, not its limit.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the heap has no entries yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The most entries this heap will ever hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// The handle a title would see for `index`: the heap's base plus the
/// console's 32-byte (see [`STRIDE`]) stride.
///
/// `base` must leave room for the whole heap; no entry index is refused
/// here, since a handle is just an address.
pub fn address_of(base: u64, index: u32) -> u64 {
    base + u64::from(index) * STRIDE
}

/// The index a handle names, or `None` if `address` is below `base` or not
/// on a [`STRIDE`] boundary — i.e. not the address of any entry.
pub fn index_of(base: u64, address: u64) -> Option<u32> {
    let offset = address.checked_sub(base)?;
    if offset % STRIDE != 0 {
        return None;
    }
    u32::try_from(offset / STRIDE).ok()
}

