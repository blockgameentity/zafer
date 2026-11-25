//!
//! Creating a swapchain needs four decisions the API refuses to make —
//! format, present mode, image count, composite alpha and extent — and all
//! four are pure functions of what the surface reports. They live here,
//! separate from the `VkSwapchainKHR` itself, because a test can then check
//! the policy on a machine with no driver at all, and because the window
//! layer re-runs them on every resize.

use ash::vk;

/// The surface format to use, in order of preference: sRGB on BGRA (what
/// the console's render targets are, and what a Linux compositor hands out
/// most often), then sRGB on RGBA, then whatever the surface listed first.
///
/// A surface reporting a single `FORMAT_UNDEFINED` entry means "any
/// format is fine", so the preferred one is returned directly.
pub fn choose_format(available: &[vk::SurfaceFormatKHR]) -> vk::SurfaceFormatKHR {
    let wanted = [
        (
            vk::Format::B8G8R8A8_SRGB,
            vk::ColorSpaceKHR::SRGB_NONLINEAR,
        ),
        (
            vk::Format::R8G8B8A8_SRGB,
            vk::ColorSpaceKHR::SRGB_NONLINEAR,
        ),
    ];
    if available.is_empty() || available.iter().any(|f| f.format == vk::Format::UNDEFINED) {
        return vk::SurfaceFormatKHR {
            format: wanted[0].0,
            color_space: wanted[0].1,
        };
    }
    for &(format, color_space) in &wanted {
        if let Some(found) = available
            .iter()
            .find(|f| f.format == format && f.color_space == color_space)
        {
            return *found;
        }
    }
    available[0]
}

/// `FIFO` always exists and is the only one that cannot tear, so it wins.
/// `MAILBOX` is next (no tearing, no queue), then `IMMEDIATE`. The result
/// is `FIFO` even when the surface listed nothing, because the spec
/// guarantees it.
pub fn choose_present_mode(available: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    for mode in [
        vk::PresentModeKHR::FIFO,
        vk::PresentModeKHR::MAILBOX,
        vk::PresentModeKHR::IMMEDIATE,
    ] {
        if available.contains(&mode) {
            return mode;
        }
    }
    vk::PresentModeKHR::FIFO
}

/// One image more than the driver's minimum, so the compositor can hold one
/// while the title renders the next, bounded by the driver's maximum and
/// never below two (a swapchain of one image cannot present without
/// stalling).
pub fn image_count(min: u32, max: u32) -> u32 {
    let wanted = min.saturating_add(1).max(2);
    match max {
        0 => wanted,
        max => wanted.min(max).max(2.min(max)),
    }
}

/// `OPAQUE` unless the surface refuses it, in which case the first mode it
/// does support. A single bit is required: Vulkan rejects a mask with more
/// than one bit set.
pub fn composite_alpha(available: vk::CompositeAlphaFlagsKHR) -> vk::CompositeAlphaFlagsKHR {
    if available.contains(vk::CompositeAlphaFlagsKHR::OPAQUE) {
        return vk::CompositeAlphaFlagsKHR::OPAQUE;
    }
    let bits = available.as_raw();
    for candidate in [
        vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
        vk::CompositeAlphaFlagsKHR::INHERIT,
    ] {
        if bits & candidate.as_raw() != 0 {
            return candidate;
        }
    }
    vk::CompositeAlphaFlagsKHR::OPAQUE
}

/// The swapchain extent for a window of `width` x `height`.
///
/// A minimised window reports zero, and Vulkan rejects an extent of zero,
/// so that case keeps the surface's minimum instead.
pub fn enclosing_extent(
    width: u32,
    height: u32,
    min: vk::Extent2D,
    max: vk::Extent2D,
) -> vk::Extent2D {
    if width == 0 || height == 0 {
        return min;
    }
    let clamp = |value: u32, low: u32, high: u32| {
        let high = if high == 0 { u32::MAX } else { high };
        value.max(low).min(high)
    };
    vk::Extent2D {
        width: clamp(width, min.width, max.width),
        height: clamp(height, min.height, max.height),
    }
}
