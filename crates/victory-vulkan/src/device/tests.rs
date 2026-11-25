//! The device tests. They skip cleanly on a machine with no driver.

use ash::vk;

use super::create;

/// One Vulkan device at a time: some ICDs are not safe to create and destroy
/// instance-level objects with from several threads at once, and these tests
/// all drive the same loader.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}


/// The preference order: a discrete GPU wins, an integrated one is second,
/// and a CPU adapter is a last resort rather than a failure.
///
/// This is the ordering a desktop with an iGPU depends on: enumerating in
/// order and taking the first usable adapter takes the iGPU, which is the
/// wrong GPU for a console title.
#[test]
fn the_preference_order_puts_a_discrete_gpu_first() {
    use ash::vk::PhysicalDeviceType as Kind;
    let order = [
        Kind::DISCRETE_GPU,
        Kind::INTEGRATED_GPU,
        Kind::VIRTUAL_GPU,
        Kind::CPU,
        Kind::OTHER,
    ];
    let ranks: Vec<u8> = order.iter().copied().map(super::create::rank_of).collect();
    assert_eq!(ranks, vec![0, 1, 2, 3, 4], "the rank order");
    // Strictly increasing, so no two kinds are tied and the choice is total.
    assert!(ranks.windows(2).all(|w| w[0] < w[1]), "no ties");
}
#[test]
fn no_adapter_is_not_an_error() {
    let _serial = serial();
    if crate::loader::entry().is_ok() {
        return; // a driver here: this test is about the machine with none
    }
    match create(c"victory test", &[]) {
        Ok(None) => {}
        Ok(Some(_)) => panic!("a device appeared with no loader installed"),
        Err(e) => panic!("no loader must be an answer, not an error: {e}"),
    }
}

#[test]
fn a_device_is_created_and_dropped() {
    let _serial = serial();
    let Ok(Some(device)) = create(c"victory test", &[]) else {
        return; // no loader or no adapter here
    };
    assert_ne!(device.physical(), vk::PhysicalDevice::null());
    assert_ne!(device.queue(), vk::Queue::null());
    assert!(device.queue_family() < 64, "{}", device.queue_family());
    // Dropping closes the device and the instance behind it; a second
    // creation proves the instance is reusable afterwards.
    drop(device);
    match create(c"victory test", &[]) {
        Ok(_) => {}
        Err(e) => panic!("a second device failed after a clean drop: {e}"),
    }
}
