//! The present and acquire result classification. Pure, so no driver.

use ash::vk;

use crate::present::*;

#[test]
fn a_plain_success_is_presented() {
    assert_eq!(Outcome::of(vk::Result::SUCCESS), Ok(Outcome::Presented));
}

#[test]
fn a_stale_swapchain_is_suboptimal_not_an_error() {
    for result in [vk::Result::SUBOPTIMAL_KHR, vk::Result::ERROR_OUT_OF_DATE_KHR] {
        assert_eq!(Outcome::of(result), Ok(Outcome::Suboptimal));
        assert!(Outcome::of(result).unwrap().recreate());
        assert!(!Outcome::of(result).unwrap().retry());
    }
}

#[test]
fn a_full_queue_is_not_ready_and_retries() {
    assert_eq!(Outcome::of(vk::Result::NOT_READY), Ok(Outcome::NotReady));
    assert!(Outcome::of(vk::Result::NOT_READY).unwrap().retry());
    assert!(!Outcome::of(vk::Result::NOT_READY).unwrap().recreate());
}

#[test]
fn presented_neither_recreates_nor_retries() {
    let outcome = Outcome::of(vk::Result::SUCCESS).unwrap();
    assert!(!outcome.recreate());
    assert!(!outcome.retry());
}

#[test]
fn a_real_failure_says_what_it_was() {
    let error = Outcome::of(vk::Result::ERROR_DEVICE_LOST).unwrap_err();
    assert!(error.contains("DEVICE_LOST"), "{error}");
    assert!(Outcome::of(vk::Result::ERROR_SURFACE_LOST_KHR).is_err());
}

#[test]
fn acquiring_treats_the_harmless_codes_as_try_again() {
    for result in [
        vk::Result::SUCCESS,
        vk::Result::NOT_READY,
        vk::Result::TIMEOUT,
        vk::Result::SUBOPTIMAL_KHR,
        vk::Result::ERROR_OUT_OF_DATE_KHR,
    ] {
        assert_eq!(acquired(result), Ok(None), "{result:?}");
    }
}

#[test]
fn acquiring_reports_a_real_failure() {
    let error = acquired(vk::Result::ERROR_DEVICE_LOST).unwrap_err();
    assert!(error.contains("DEVICE_LOST"), "{error}");
}
