use hid_rs::{Hid, HidDevice, SafeCallback2};
use std::{future::Future, pin::Pin, sync::LazyLock, time::Duration};
use tokio::time::sleep;

static HID_DEVICE_EVENT_HANDLER: LazyLock<SafeCallback2<u128, Vec<u8>, ()>> =
    LazyLock::new(|| SafeCallback2::<u128, Vec<u8>, ()>::new(move |uuid, data| {
        Box::pin(async move {
            println!(
                "Received report from device {:?}, Data: {:?}",
                uuid,
                data
            );
        }) as Pin<Box<dyn Future<Output = ()> + Send + 'static>>
    }));

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (cross-platform)
    hid_rs::logger::init();
    log::info!("Starting HID Example");

    // Initialize HID subsystem
    Hid::init_hid().await?;
    log::info!("HID Initialized");

    hid_rs::Hid::sub_connection_changed(SafeCallback2::<u128, bool, ()>::new(|uuid, event_type| {
        Box::pin(async move {
            println!(
                "Device connection changed: {:?}, Type: {:?}",
                uuid,
                event_type
            );
            let device = HidDevice::from(uuid);
            match event_type {
                true => {
                    if let Err(err) = device.add_report_listener(&HID_DEVICE_EVENT_HANDLER).await {
                        eprintln!("Failed to add report listener for {uuid:?}: {err:?}");
                        return;
                    }
                }
                false => {
                    if let Err(err) = device.remove_report_listener(&HID_DEVICE_EVENT_HANDLER).await {
                        eprintln!("Failed to remove report listener for {uuid:?}: {err:?}");
                    }
                }
            }
        }) as Pin<Box<dyn Future<Output = ()> + Send + 'static>>
    })).await?;

    // wait 100 seconds to receive events
    sleep(Duration::from_secs(100)).await;
    Ok(())
}
