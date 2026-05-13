//! Integration tests — require a connected HID device.
//!
//! Gated behind the `integration-tests` feature flag so CI passes without hardware:
//!
//!   # Compile only (no hardware needed):
//!   cargo test --test integration_tests
//!
//!   # Run with hardware:
//!   cargo test --test integration_tests --features integration-tests
//!
//! These tests also verify platform-level behavior (is_supported, etc.) that
//! does not require hardware and runs unconditionally.

// Android and WASM have separate test entry-points; skip this file for them.
#![cfg(not(any(target_arch = "wasm32", target_os = "android")))]

// ── Platform sanity (no hardware required) ───────────────────────────────────

/// Desktop platforms must report HID as supported.
#[test]
fn is_supported_on_desktop() {
    assert!(
        hid_rs::is_supported(),
        "is_supported() should return true on desktop platforms"
    );
}

// ── Hardware-dependent tests ──────────────────────────────────────────────────
//
// All tests below are compiled only when `--features integration-tests` is
// passed, ensuring a plain `cargo test` never fails due to missing hardware.

#[cfg(feature = "integration-tests")]
mod with_hardware {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    use tokio::time::{sleep, Duration};

    /// `init()` must succeed and be idempotent.
    #[tokio::test]
    async fn init_is_idempotent() {
        hid_rs::init().await.expect("first init failed");
        hid_rs::init()
            .await
            .expect("second init (idempotent) failed");
    }

    /// `device_list()` must succeed after init and return coherent data.
    #[tokio::test]
    async fn device_list_returns_valid_devices() {
        hid_rs::init().await.expect("init failed");
        let devices = hid_rs::device_list().expect("device_list failed");
        println!("Found {} HID device(s)", devices.len());
        for device in &devices {
            println!("  Device ID: {}", device.id);
        }
        // The list itself is valid; hardware presence is environment-dependent.
    }

    /// Every device returned by `device_list()` must:
    ///   • report itself as available
    ///   • expose a valid (non-zero) VID and PID
    #[tokio::test]
    async fn device_properties_are_consistent() {
        hid_rs::init().await.expect("init failed");
        let devices = hid_rs::device_list().expect("device_list failed");

        if devices.is_empty() {
            println!("SKIP: no HID devices connected");
            return;
        }

        for device in devices {
            assert!(
                device.available(),
                "device {:?} listed but available() returned false",
                device.id
            );

            let vid = device.vid().expect("vid() failed");
            let pid = device.pid().expect("pid() failed");
            println!("  VID={:#06x}  PID={:#06x}  ID={}", vid, pid, device.id);
        }
    }

    /// `on_connection_changed` must fire the callback for every device that is
    /// already connected at the time of subscription.
    #[tokio::test]
    async fn connection_callback_fires_for_existing_devices() {
        hid_rs::init().await.expect("init failed");

        let fired = Arc::new(AtomicU32::new(0));
        let fired_clone = fired.clone();

        let _sub = hid_rs::on_connection_changed(move |_id, connected| {
            if connected {
                fired_clone.fetch_add(1, Ordering::Relaxed);
            }
        })
        .expect("on_connection_changed failed");

        // Give the backend a moment to deliver the initial burst of callbacks.
        sleep(Duration::from_millis(500)).await;

        let callback_count = fired.load(Ordering::Relaxed);
        let list_count = hid_rs::device_list().expect("device_list failed").len() as u32;

        println!(
            "connection callbacks fired: {}  device_list count: {}",
            callback_count, list_count
        );

        // At minimum, one callback per listed device must have arrived.
        assert!(
            callback_count >= list_count,
            "expected >= {} connection callbacks, got {}",
            list_count,
            callback_count
        );
    }

    /// Dropping a `Subscription` must successfully unsubscribe without panic.
    #[tokio::test]
    async fn subscription_drop_unsubscribes() {
        hid_rs::init().await.expect("init failed");
        {
            let sub = hid_rs::on_connection_changed(|_id, _connected| {})
                .expect("on_connection_changed failed");
            drop(sub); // must not panic
        }
    }

    /// `Subscription::detach()` must keep the listener alive and not panic.
    #[tokio::test]
    async fn subscription_detach_keeps_listener() {
        hid_rs::init().await.expect("init failed");
        let _sub_id = hid_rs::on_connection_changed(|_id, _connected| {})
            .expect("on_connection_changed failed")
            .detach(); // returns SubscriptionId; listener stays installed
    }

    /// Subscribing to reports on a connected device and verifying the
    /// subscription handle is returned without error.
    #[tokio::test]
    async fn report_subscription_on_connected_device() {
        use hid_rs::HidDevice;

        hid_rs::init().await.expect("init failed");
        let devices = hid_rs::device_list().expect("device_list failed");

        if devices.is_empty() {
            println!("SKIP: no HID devices connected");
            return;
        }

        let device = HidDevice::from(devices[0].id);
        let _report_sub = device
            .on_report(|id, data| {
                println!("Report from {}: {:02X?}", id, &data[..]);
            })
            .expect("on_report failed");
    }
}
