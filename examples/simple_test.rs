use hid_rs::{DeviceId, HidDevice, Subscription};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (cross-platform)
    hid_rs::logger::init();
    log::info!("Starting HID Example");

    // Initialize HID subsystem
    hid_rs::init().await?;
    log::info!("HID Initialized");

    // Keep one report-subscription per device so they live until disconnect.
    let report_subs: &'static Mutex<Vec<(DeviceId, Subscription)>> =
        Box::leak(Box::new(Mutex::new(Vec::new())));

    let _connection_sub = hid_rs::on_connection_changed(move |id, connected| {
        println!("Device connection changed: id={id} connected={connected}");
        let device = HidDevice::from(id);
        if connected {
            match device.on_report(|id, data| {
                println!("Report from {id}: {:02X?}", &data[..]);
            }) {
                Ok(sub) => report_subs.lock().unwrap().push((id, sub)),
                Err(err) => eprintln!("Failed to add report listener for {id}: {err:?}"),
            }
        } else {
            report_subs.lock().unwrap().retain(|(d, _)| *d != id);
        }
    })?
    .detach(); // keep the connection listener alive for the lifetime of the program

    // wait 100 seconds to receive events
    sleep(Duration::from_secs(100)).await;
    Ok(())
}
