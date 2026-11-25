//! Which adapter `victory-vulkan` picks, and what the machine offers.
//!
//! Run: `cargo run -p victory-vulkan --example adapters`.

fn main() {
    match victory_vulkan::instance::probe() {
        Ok(list) => {
            for info in &list {
                println!(
                    "offered: {:<14} {} (vulkan {}.{}.{})",
                    info.device_type, info.name, info.api_version.0, info.api_version.1,
                    info.api_version.2
                );
            }
        }
        Err(e) => println!("probe failed: {e}"),
    }
    match victory_vulkan::device::create(c"adapters", &[]) {
        Ok(Some(device)) => println!("CHOSEN: {}", device.name()),
        Ok(None) => println!("CHOSEN: none"),
        Err(e) => println!("create failed: {e}"),
    }
}
