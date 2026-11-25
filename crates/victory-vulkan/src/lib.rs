//! The Vulkan backend: the host graphics API we are moving to.
//!
//! The console's memory model is Vulkan's (a GPU address is a CPU address,
//! memory is unified), and D3D12-on-Linux already means vkd3d-proton
//! translating a full API on top of Vulkan. This module is additive: the
//! D3D12 host stays the working path while the backend is built, and the
//! design and work order are in `docs/vulkan-backend.md`.
//!
//! What exists today is the part under everything else: opening the loader
//! at run time and enumerating adapters. That alone tells a run whether
//! the machine can take the backend, and the tests skip cleanly when there
//! is no driver.

pub mod descriptor;
pub mod device;
pub mod heap;
pub mod instance;
pub mod loader;
pub mod present;
pub mod presenter;
pub mod query;
pub mod resource;
pub mod swapchain;

#[cfg(test)]
mod descriptor_tests;
#[cfg(test)]
mod heap_tests;
#[cfg(test)]
mod present_tests;
#[cfg(test)]
mod query_tests;
#[cfg(test)]
mod resource_tests;
#[cfg(test)]
mod instance_tests;
#[cfg(test)]
mod swapchain_tests;

pub use presenter::Presenter;
pub use instance::{
    Info, SURFACE_EXTENSION, available, instance_extensions, loader_version, probe,
};
