use hid_rs::{Hid, SafeCallback2};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (cross-platform)
    hid_rs::logger::init();
    log::info!("Starting HID Example");

    // Initialize HID subsystem
    Hid::init_hid().await?;
    log::info!("HID Initialized");

    // Define filters (empty for all devices, or specific VID/PID)
    // Example: SayoDevice VID=0x8089
    let filters = vec![
        (0x8089, None), // SayoDevice
    ];

    log::info!("Setting device filters...");
    // On Desktop/Android, this sets the filter for the background scanner.
    // On Web, this would prompt the user to select a device.
    let _ = Hid::request_device(filters).await?;

    log::info!("Waiting for device scan...");
    // Give the background thread time to scan (Desktop implementation polls every 1s)
    sleep(Duration::from_secs(2)).await;

    log::info!("Getting device list...");
    let devices = hid_rs::Hid::get_device_list()?;

    if devices.is_empty() {
        log::warn!("No devices found!");
        return Ok(());
    }

    for device in devices {
        let vid = device.vid()?;
        let pid = device.pid()?;
        let name = device.get_product_name()?.unwrap_or_default();
        
        log::info!("Found Device: {} (VID: {:04X}, PID: {:04X})", name, vid, pid);

        // Example: Listen for reports
        let callback = SafeCallback2::new(|_uuid, data| Box::pin(async move {
            log::info!("Received report: {:02X?}", data);
        }));

        log::info!("Subscribing to report events...");
        device.add_report_listener(&callback).await?;

        // Keep alive for a bit to receive events
        sleep(Duration::from_secs(5)).await;

        log::info!("Unsubscribing...");
        device.remove_report_listener(&callback).await?;
    }

    Ok(())
}
