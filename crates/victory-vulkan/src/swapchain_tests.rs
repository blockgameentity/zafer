//! The swapchain policy. No driver needed: every function here is pure.

use ash::vk;

use crate::swapchain::*;

fn format(f: vk::Format) -> vk::SurfaceFormatKHR {
    vk::SurfaceFormatKHR {
        format: f,
        color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
    }
}

#[test]
fn srgb_bgra_wins_when_offered() {
    let available = [
        format(vk::Format::R8G8B8A8_UNORM),
        format(vk::Format::B8G8R8A8_SRGB),
    ];
    assert_eq!(choose_format(&available).format, vk::Format::B8G8R8A8_SRGB);
}

#[test]
fn srgb_rgba_is_the_fallback() {
    let available = [
        format(vk::Format::B8G8R8A8_UNORM),
        format(vk::Format::R8G8B8A8_SRGB),
    ];
    assert_eq!(choose_format(&available).format, vk::Format::R8G8B8A8_SRGB);
}

#[test]
fn undefined_means_any_and_gets_the_preferred() {
    let available = [vk::SurfaceFormatKHR {
        format: vk::Format::UNDEFINED,
        color_space: vk::ColorSpaceKHR::SRGB_NONLINEAR,
    }];
    assert_eq!(choose_format(&available).format, vk::Format::B8G8R8A8_SRGB);
}

#[test]
fn an_empty_list_still_answers() {
    assert_eq!(choose_format(&[]).format, vk::Format::B8G8R8A8_SRGB);
}

#[test]
fn an_unknown_format_list_falls_back_to_the_first_entry() {
    let available = [format(vk::Format::A2B10G10R10_UNORM_PACK32)];
    assert_eq!(
        choose_format(&available).format,
        vk::Format::A2B10G10R10_UNORM_PACK32
    );
}

#[test]
fn fifo_is_preferred_over_mailbox() {
    let available = [vk::PresentModeKHR::MAILBOX, vk::PresentModeKHR::FIFO];
    assert_eq!(choose_present_mode(&available), vk::PresentModeKHR::FIFO);
}

#[test]
fn mailbox_is_next_then_immediate() {
    assert_eq!(
        choose_present_mode(&[vk::PresentModeKHR::IMMEDIATE, vk::PresentModeKHR::MAILBOX]),
        vk::PresentModeKHR::MAILBOX
    );
    assert_eq!(
        choose_present_mode(&[vk::PresentModeKHR::IMMEDIATE]),
        vk::PresentModeKHR::IMMEDIATE
    );
}

#[test]
fn an_empty_mode_list_is_fifo() {
    assert_eq!(choose_present_mode(&[]), vk::PresentModeKHR::FIFO);
}

#[test]
fn image_count_is_min_plus_one_and_at_least_two() {
    assert_eq!(image_count(2, 8), 3);
    assert_eq!(image_count(1, 8), 2);
    assert_eq!(image_count(0, 8), 2);
}

#[test]
fn image_count_respects_the_maximum() {
    assert_eq!(image_count(3, 3), 3);
    assert_eq!(image_count(3, 0), 4);
}

#[test]
fn opaque_alpha_wins() {
    let both = vk::CompositeAlphaFlagsKHR::OPAQUE | vk::CompositeAlphaFlagsKHR::INHERIT;
    assert_eq!(composite_alpha(both), vk::CompositeAlphaFlagsKHR::OPAQUE);
}

#[test]
fn alpha_falls_back_to_a_single_supported_bit() {
    let available = vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED
        | vk::CompositeAlphaFlagsKHR::INHERIT;
    assert_eq!(
        composite_alpha(available),
        vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED
    );
}

#[test]
fn a_minimised_window_keeps_the_minimum_extent() {
    let min = vk::Extent2D {
        width: 16,
        height: 16,
    };
    assert_eq!(enclosing_extent(0, 0, min, vk::Extent2D::default()), min);
}

#[test]
fn the_extent_clamps_to_the_surface_maximum() {
    let min = vk::Extent2D {
        width: 1,
        height: 1,
    };
    let max = vk::Extent2D {
        width: 1920,
        height: 1080,
    };
    assert_eq!(
        enclosing_extent(4000, 3000, min, max),
        vk::Extent2D {
            width: 1920,
            height: 1080
        }
    );
    assert_eq!(
        enclosing_extent(800, 600, min, max),
        vk::Extent2D {
            width: 800,
            height: 600
        }
    );
}

#[test]
fn an_unbounded_surface_accepts_the_window_size() {
    let min = vk::Extent2D {
        width: 1,
        height: 1,
    };
    assert_eq!(
        enclosing_extent(2560, 1440, min, vk::Extent2D::default()),
        vk::Extent2D {
            width: 2560,
            height: 1440
        }
    );
}
