//! WebView-bridge backend tests — wasm32 only, driven by the in-memory
//! `js/fake_sayo_hid_bridge.js`. No device or browser WebHID is needed:
//!
//!   wasm-pack test --node -- --test bridge_tests
//!
//! hid_rs keeps global state, so every test starts from `fresh_bridge()`
//! and ends with `teardown()`.

#![cfg(target_arch = "wasm32")]

use futures::channel::mpsc;
use futures::StreamExt;
use hid_rs::{hid_error::HidError, DeviceId, HidDevice};
use js_sys::{Function, Object, Reflect, Uint8Array};
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

#[wasm_bindgen(module = "/js/fake_sayo_hid_bridge.js")]
extern "C" {
    type FakeBridge;

    #[wasm_bindgen(js_name = createFakeSayoHidBridge)]
    fn create_fake_bridge() -> FakeBridge;

    #[wasm_bindgen(method)]
    fn install(this: &FakeBridge);

    #[wasm_bindgen(method)]
    fn uninstall(this: &FakeBridge);

    #[wasm_bindgen(method)]
    fn connect(this: &FakeBridge, device: &JsValue);

    #[wasm_bindgen(method)]
    fn disconnect(this: &FakeBridge, uuid: &str);

    #[wasm_bindgen(method, js_name = emitInput)]
    fn emit_input(this: &FakeBridge, uuid: &str, data: &Uint8Array) -> bool;

    #[wasm_bindgen(method, js_name = isStreaming)]
    fn is_streaming(this: &FakeBridge, uuid: &str) -> bool;

    #[wasm_bindgen(method, getter)]
    fn calls(this: &FakeBridge) -> js_sys::Array;
}

const SAYO_VID: u16 = 0x8089;
const DEV_A: &str = "00000000-0000-4000-8000-00000000000a";
const DEV_B: &str = "00000000-0000-4000-8000-00000000000b";

/// Vendor-defined collection: input 0x02 (64 B), output 0x21 (32 B),
/// feature 0x22 (16 B). Sizes below include the Report ID byte.
const DESCRIPTOR: [u8; 38] = [
    0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, //
    0x85, 0x02, 0x75, 0x08, 0x95, 0x40, 0x09, 0x01, 0x81, 0x02, //
    0x85, 0x21, 0x75, 0x08, 0x95, 0x20, 0x09, 0x02, 0x91, 0x02, //
    0x85, 0x22, 0x75, 0x08, 0x95, 0x10, 0x09, 0x03, 0xb1, 0x02, //
    0xc0,
];
const OUTPUT_REPORT_SIZE: usize = 33;

fn set(target: &Object, key: &str, value: impl Into<JsValue>) {
    Reflect::set(target, &JsValue::from_str(key), &value.into()).unwrap();
}

/// A fake device. With `echo`, every output report is answered with input
/// report 0x02 carrying the first three payload bytes.
fn device(uuid: &str, granted: bool, echo: bool) -> JsValue {
    let dev = Object::new();
    set(&dev, "uuid", uuid);
    set(&dev, "vendorId", SAYO_VID);
    set(&dev, "productId", 0x0001u16);
    set(&dev, "productName", "Fake Sayo");
    set(&dev, "serialNumber", "SN-1");
    set(&dev, "granted", granted);
    set(&dev, "descriptor", Uint8Array::from(DESCRIPTOR.as_slice()));
    if echo {
        let handler = Function::new_with_args(
            "data, emit",
            "emit(Uint8Array.of(0x02, ...data.subarray(1, 4)))",
        );
        set(&dev, "onOutputReport", handler);
    }
    dev.into()
}

fn call_names(fake: &FakeBridge) -> Vec<String> {
    fake.calls()
        .iter()
        .map(|call| js_sys::Array::from(&call).get(0).as_string().unwrap())
        .collect()
}

fn fresh_bridge() -> FakeBridge {
    let _ = hid_rs::shutdown();
    let fake = create_fake_bridge();
    fake.install();
    fake
}

fn teardown(fake: FakeBridge) {
    hid_rs::shutdown().unwrap();
    fake.uninstall();
}

fn only_device() -> HidDevice {
    let mut devices = hid_rs::device_list().unwrap();
    assert_eq!(devices.len(), 1, "expected exactly one device");
    devices.remove(0)
}

#[wasm_bindgen_test]
async fn bridge_backend_is_selected_when_installed() {
    let fake = fresh_bridge();
    assert!(hid_rs::is_supported());
    hid_rs::init().await.unwrap();
    assert_eq!(call_names(&fake)[..2], ["init", "listDevices"]);
    teardown(fake);
}

#[wasm_bindgen_test]
async fn init_adopts_granted_devices_and_caches_metadata() {
    let fake = fresh_bridge();
    fake.connect(&device(DEV_A, true, false));
    fake.connect(&device(DEV_B, false, false));
    hid_rs::init().await.unwrap();

    let dev = only_device();
    assert!(dev.available());
    assert_eq!(dev.vid().unwrap(), SAYO_VID);
    assert_eq!(dev.pid().unwrap(), 0x0001);
    assert_eq!(dev.get_product_name().unwrap().as_deref(), Some("Fake Sayo"));
    assert_eq!(dev.get_serial_number().unwrap().as_deref(), Some("SN-1"));
    assert!(dev.has_report_id(0x02).unwrap());
    assert!(dev.has_report_id(0x21).unwrap());
    assert!(!dev.has_report_id(0x99).unwrap());

    let desc = dev.get_collections().unwrap();
    assert_eq!(desc.input_reports[0].size, 65);
    assert_eq!(desc.output_reports[0].size, OUTPUT_REPORT_SIZE);
    assert_eq!(desc.feature_reports[0].size, 17);
    teardown(fake);
}

#[wasm_bindgen_test]
async fn request_device_grants_and_adopts() {
    let fake = fresh_bridge();
    fake.connect(&device(DEV_A, false, false));
    hid_rs::init().await.unwrap();
    assert!(hid_rs::device_list().unwrap().is_empty());

    let ids = hid_rs::request_device(vec![(SAYO_VID, None)]).await.unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(only_device().id.as_u128(), ids[0]);

    // Asking again returns the already-adopted device instead of a new id.
    let again = hid_rs::request_device(vec![(SAYO_VID, None)]).await.unwrap();
    assert_eq!(again, ids);
    teardown(fake);
}

#[wasm_bindgen_test]
async fn request_device_returns_id_when_hot_plug_event_races_it() {
    let fake = fresh_bridge();
    let dev = device(DEV_A, false, false);
    set(dev.unchecked_ref(), "announceOnGrant", true);
    fake.connect(&dev);
    hid_rs::init().await.unwrap();

    let ids = hid_rs::request_device(vec![(SAYO_VID, None)]).await.unwrap();
    assert_eq!(ids.len(), 1, "the granted device must be returned");
    assert_eq!(only_device().id.as_u128(), ids[0]);
    teardown(fake);
}

#[wasm_bindgen_test]
async fn devices_outside_the_vid_filter_are_ignored() {
    let fake = fresh_bridge();
    hid_rs::set_device_filters(vec![(0x1234, None)]);
    fake.connect(&device(DEV_A, true, false));
    hid_rs::init().await.unwrap();
    assert!(hid_rs::device_list().unwrap().is_empty());
    hid_rs::set_device_filters(vec![(SAYO_VID, None)]);
    teardown(fake);
}

#[wasm_bindgen_test]
async fn send_report_pads_and_input_round_trips() {
    let fake = fresh_bridge();
    fake.connect(&device(DEV_A, true, true));
    hid_rs::init().await.unwrap();
    let dev = only_device();

    let (tx, mut rx) = mpsc::unbounded::<Vec<u8>>();
    let tx = Arc::new(Mutex::new(tx));
    let sub = dev
        .on_report(move |_, data| {
            let _ = tx.lock().unwrap().unbounded_send(data.to_vec());
        })
        .unwrap();
    assert!(fake.is_streaming(DEV_A));

    dev.send_report(vec![0x21, 0xaa, 0xbb, 0xcc]).await.unwrap();
    let sent = js_sys::Array::from(&fake.calls().iter().last().unwrap());
    assert_eq!(sent.get(0).as_string().unwrap(), "sendReport");
    let payload: Vec<u8> = js_sys::Array::from(&sent.get(2))
        .iter()
        .map(|b| b.as_f64().unwrap() as u8)
        .collect();
    assert_eq!(payload.len(), OUTPUT_REPORT_SIZE);
    assert_eq!(&payload[..4], &[0x21, 0xaa, 0xbb, 0xcc]);
    assert!(payload[4..].iter().all(|&b| b == 0));

    assert_eq!(rx.next().await.unwrap(), vec![0x02, 0xaa, 0xbb, 0xcc]);

    drop(sub);
    assert!(!fake.is_streaming(DEV_A));
    teardown(fake);
}

#[wasm_bindgen_test]
async fn input_stream_stops_only_after_last_listener() {
    let fake = fresh_bridge();
    fake.connect(&device(DEV_A, true, false));
    hid_rs::init().await.unwrap();
    let dev = only_device();

    let first = dev.on_report(|_, _| {}).unwrap();
    let second = dev.on_report(|_, _| {}).unwrap();
    let starts = call_names(&fake).iter().filter(|c| *c == "startInput").count();
    assert_eq!(starts, 1);

    drop(first);
    assert!(fake.is_streaming(DEV_A));
    drop(second);
    assert!(!fake.is_streaming(DEV_A));
    teardown(fake);
}

#[wasm_bindgen_test]
async fn send_report_validates_against_descriptor() {
    let fake = fresh_bridge();
    fake.connect(&device(DEV_A, true, false));
    hid_rs::init().await.unwrap();
    let dev = only_device();

    assert!(matches!(
        dev.send_report(Vec::new()).await,
        Err(HidError::EmptyData)
    ));
    assert!(matches!(
        dev.send_report(vec![0x99, 0x00]).await,
        Err(HidError::ReportIdMissing { report_id: 0x99, .. })
    ));
    assert!(matches!(
        dev.send_report(vec![0x21; OUTPUT_REPORT_SIZE + 1]).await,
        Err(HidError::DataTooLarge { .. })
    ));
    teardown(fake);
}

#[wasm_bindgen_test]
async fn hot_plug_events_reach_connection_listeners() {
    let fake = fresh_bridge();
    hid_rs::init().await.unwrap();

    let (tx, mut rx) = mpsc::unbounded::<(DeviceId, bool)>();
    let tx = Arc::new(Mutex::new(tx));
    let _sub = hid_rs::on_connection_changed(move |id, connected| {
        let _ = tx.lock().unwrap().unbounded_send((id, connected));
    })
    .unwrap();

    fake.connect(&device(DEV_A, true, false));
    let (id, connected) = rx.next().await.unwrap();
    assert!(connected);
    assert_eq!(only_device().id, id);

    fake.disconnect(DEV_A);
    assert_eq!(rx.next().await.unwrap(), (id, false));
    assert!(hid_rs::device_list().unwrap().is_empty());
    assert!(matches!(
        HidDevice::from(id).vid(),
        Err(HidError::DeviceNotFound(_))
    ));
    teardown(fake);
}
