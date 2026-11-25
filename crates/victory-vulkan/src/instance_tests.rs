//! The Vulkan query tests. They skip cleanly on a machine with no driver.

use crate::instance::*;

/// One Vulkan query at a time: some ICDs are not safe to create and destroy
/// instances with from several threads at once, and these tests all drive
/// the same loader.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn availability_matches_probing() {
    let _serial = serial();
    if !available() {
        // No driver here: probing must say why, not panic.
        assert!(probe().is_err());
        return;
    }
    let probed = probe().expect("a loader with no adapters still probes");
    assert!(probed.len() < 64, "an implausible adapter count");
}

#[test]
fn a_probed_adapter_describes_itself() {
    let _serial = serial();
    let Ok(adapters) = probe() else {
        return; // no Vulkan here
    };
    for a in adapters {
        assert!(!a.name.is_empty(), "{a:?}");
        assert!(a.api_version.0 >= 1, "{a:?}");
        assert_ne!(a.device_type, "unknown", "{a:?}");
    }
}

#[test]
fn the_loader_reports_a_vulkan_version() {
    let _serial = serial();
    if !available() {
        return;
    }
    let version = loader_version().expect("a loader that is open reports its version");
    assert!(version.0 >= 1, "{version:?}");
}

#[test]
fn instance_extensions_include_surface_when_a_driver_is_present() {
    let _serial = serial();
    let Ok(extensions) = instance_extensions() else {
        return;
    };
    assert!(
        extensions.is_empty() || extensions.iter().any(|e| e.starts_with("VK_KHR_surface")),
        "{extensions:?}"
    );
}

#[test]
fn a_version_splits_into_its_parts() {
    assert_eq!(split_api_version(ash::vk::make_api_version(0, 1, 3, 7)), (1, 3, 7));
    assert_eq!(split_api_version(0), (0, 0, 0));
}
