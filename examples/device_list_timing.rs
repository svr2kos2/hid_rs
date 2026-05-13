use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    hid_rs::logger::init();
    log::info!("Starting device list timing test");

    // Initialize HID subsystem
    hid_rs::init().await?;
    log::info!("HID Initialized");

    // Test 1: read device list immediately after init
    println!("=== Test 1: device_list immediately after init ===");
    match hid_rs::device_list() {
        Ok(devices) => {
            println!("Immediately: found {} devices", devices.len());
            for d in &devices {
                let vid = d.vid().ok();
                let pid = d.pid().ok();
                let name = d.get_product_name().ok().flatten();
                println!("  id={} vid={:?} pid={:?} name={:?}", d.id, vid, pid, name);
            }
        }
        Err(e) => {
            eprintln!("Immediately: get_device_list failed: {:?}", e);
        }
    }

    // Test 2: wait 500ms then read device list
    println!("=== Test 2: device_list after 500ms ===");
    sleep(Duration::from_millis(500)).await;
    match hid_rs::device_list() {
        Ok(devices) => {
            println!("After 500ms: found {} devices", devices.len());
            for d in &devices {
                let vid = d.vid().ok();
                let pid = d.pid().ok();
                let name = d.get_product_name().ok().flatten();
                println!("  id={} vid={:?} pid={:?} name={:?}", d.id, vid, pid, name);
            }
        }
        Err(e) => {
            eprintln!("After 500ms: get_device_list failed: {:?}", e);
        }
    }

    Ok(())
}
