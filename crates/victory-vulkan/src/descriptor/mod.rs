//! Descriptor heaps, and the buffer a shader reads them through.
//!
//! [`table`] is the heap's *layout*: what an entry means, how wide it is and
//! where it sits relative to the heap's address. It is plain data, so it is
//! tested with no driver. [`buffer`] owns the `VkBuffer` and the device
//! address that layout describes.

pub mod buffer;
mod table;

pub use buffer::Buffer;
pub use table::{address_of, binding, format_word, index_of, Entry, Heap, Kind, STRIDE};
