//! The swapchain: how to choose its properties, and the object itself.
//!
//! The two halves are split on purpose. [`policy`] is pure and needs no
//! driver, so the decisions Vulkan leaves to the application are tested on a
//! machine with no Vulkan at all. [`object`] carries those decisions out.

pub mod object;
mod policy;

pub use object::{Status, Surface, Swapchain};
pub use policy::{
    choose_format, choose_present_mode, composite_alpha, enclosing_extent, image_count,
};
