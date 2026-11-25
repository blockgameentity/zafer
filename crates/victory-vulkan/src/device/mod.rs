//! Devices: the logical device the backend submits everything through.
//!
//! An instance says which adapters exist; this module turns one of them into
//! something that can be used. The backend runs on a single queue, so the
//! choice is made once: the first adapter with a queue family that can do
//! graphics and compute, and one queue from that family at priority 1.0.
//!
//! Nothing here assumes a driver exists. [`create`] answers `Ok(None)` when
//! the loader is missing or no adapter qualifies, so every test skips
//! cleanly on a machine with no Vulkan at all.

mod create;
mod memory;
mod queue;
mod shader;

pub use shader::ShaderModule;

pub use create::{Device, create};
pub use queue::{PRIORITY, family, wanted};

#[cfg(test)]
mod tests;
