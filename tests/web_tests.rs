//! WebHID / WASM tests — compiled and executed only under `wasm32`.
//!
//! Run with `wasm-pack test`:
//!
//!   # Headless Chrome — pure-logic tests, no device required:
//!   wasm-pack test --headless --chrome
//!
//!   # Browser with UI — WebHID tests, device must have been previously
//!   # granted by the browser (via requestDevice in a prior session):
//!   wasm-pack test --chrome
//!
//! NOTE: WebHID is not available in headless Chrome.  Tests that rely on it
//! will be skipped automatically when `hid_rs::is_supported()` returns false.

// This entire file is compiled only for wasm32 targets.
// A plain `cargo test` (native) will see an empty module and skip it.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

// Requests wasm-bindgen-test to run tests inside a real browser tab.
// Combine with `wasm-pack test --chrome` (with or without `--headless`).
wasm_bindgen_test_configure!(run_in_browser);

use hid_rs::{hid_error::HidError, DeviceId};

// ── Pure logic (run headless too) ─────────────────────────────────────────────

#[wasm_bindgen_test]
fn device_id_roundtrip_wasm() {
    let raw: u128 = 0xDEAD_BEEF_CAFE_1234;
    let id = DeviceId::new(raw);
    assert_eq!(id.as_u128(), raw);
}

#[wasm_bindgen_test]
fn device_id_equality_wasm() {
    assert_eq!(DeviceId::new(1), DeviceId::new(1));
    assert_ne!(DeviceId::new(1), DeviceId::new(2));
}

#[wasm_bindgen_test]
fn device_id_display_wasm() {
    let s = format!("{}", DeviceId::new(0xABCD));
    assert_eq!(s.len(), 32);
    assert!(s.ends_with("abcd"));
}

#[wasm_bindgen_test]
fn hid_error_display_wasm() {
    let e = HidError::new("wasm error");
    assert!(format!("{}", e).contains("wasm error"));
}

#[wasm_bindgen_test]
fn hid_error_device_not_found_wasm() {
    let e = HidError::DeviceNotFound(0x42);
    assert!(!format!("{}", e).is_empty());
}

// ── WebHID integration tests ──────────────────────────────────────────────────
//
// These tests call into the live WebHID backend.  They pass if:
//   • The browser supports WebHID (Chrome 89+), AND
//   • At least one HID device has been previously granted to this origin.
//
// In headless Chrome, WebHID is unavailable; is_supported() returns false
// and the tests are skipped gracefully.

/// Verify `is_supported()` is callable from WASM (value depends on browser).
#[wasm_bindgen_test]
fn is_supported_callable_in_browser() {
    let supported = hid_rs::is_supported();
    web_sys::console::log_1(&format!("WebHID is_supported: {}", supported).into());
    // No assertion — value depends on browser / OS.
}

/// After `init()`, `device_list()` must succeed.
/// If no devices were previously granted, the list will simply be empty.
#[wasm_bindgen_test]
async fn device_list_after_init_wasm() {
    if !hid_rs::is_supported() {
        web_sys::console::log_1(&"SKIP: WebHID not supported in this context".into());
        return;
    }

    hid_rs::init().await.expect("init() failed");

    let devices = hid_rs::device_list().expect("device_list() failed");
    web_sys::console::log_1(&format!("WebHID device_list: {} device(s)", devices.len()).into());
}

/// Verify `init()` is idempotent in a browser context.
#[wasm_bindgen_test]
async fn init_is_idempotent_wasm() {
    if !hid_rs::is_supported() {
        return;
    }
    hid_rs::init().await.expect("first init failed");
    hid_rs::init()
        .await
        .expect("second init (idempotent) failed");
}

/// `on_connection_changed` must return a Subscription without error.
#[wasm_bindgen_test]
async fn connection_subscription_wasm() {
    if !hid_rs::is_supported() {
        return;
    }
    hid_rs::init().await.expect("init failed");

    let sub =
        hid_rs::on_connection_changed(|_id, _connected| {}).expect("on_connection_changed failed");
    drop(sub); // unsubscribe — must not panic
}

/// For each device already accessible (previously granted), basic property
/// reads must succeed.
#[wasm_bindgen_test]
async fn device_properties_accessible_wasm() {
    if !hid_rs::is_supported() {
        return;
    }
    hid_rs::init().await.expect("init failed");

    let devices = hid_rs::device_list().expect("device_list failed");
    if devices.is_empty() {
        web_sys::console::log_1(
            &"SKIP: no previously-granted HID devices in this browser profile".into(),
        );
        return;
    }

    for device in devices {
        let vid = device.vid().expect("vid() failed");
        let pid = device.pid().expect("pid() failed");
        web_sys::console::log_1(
            &format!("  VID={:#06x}  PID={:#06x}  ID={}", vid, pid, device.id).into(),
        );
    }
}
