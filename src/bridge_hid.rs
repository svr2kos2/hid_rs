//! HID backend for hosts without WebHID (Android WebView).
//!
//! The host injects `globalThis.__sayoHidBridge` (see
//! `js/sayo_hid_bridge_android.js`), which forwards to the native USB stack.
//! Device metadata and the parsed report descriptor are cached when a device
//! is added, so every synchronous query is answered locally; only
//! enumeration, descriptor fetch, report I/O and input-stream control cross
//! the bridge.

use crate::{
    hid_error::HidError,
    hid_report_descriptor::{HidReportDescriptor, HidReportInfo},
    ConnectionCallback, DeviceId, ReportCallback, SubscriptionId,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    sync::Arc,
};

use futures::future::{FutureExt, LocalBoxFuture, Shared};
use js_sys::{Array, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::JsFuture;

////////////////////////////////////////
// Constants
////////////////////////////////////////
const BRIDGE_GLOBAL: &str = "__sayoHidBridge";
const DEFAULT_VENDOR_ID: u16 = 0x8089;

////////////////////////////////////////
// Host bridge contract
////////////////////////////////////////
#[wasm_bindgen]
extern "C" {
    type JsBridge;

    #[wasm_bindgen(method, catch)]
    fn init(this: &JsBridge) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch, js_name = listDevices)]
    fn list_devices(this: &JsBridge) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch, js_name = requestDevices)]
    fn request_devices(this: &JsBridge, filters: &Array) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch, js_name = getReportDescriptor)]
    fn get_report_descriptor(this: &JsBridge, id: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch, js_name = sendReport)]
    fn send_report(this: &JsBridge, id: &str, data: &Uint8Array) -> Result<Promise, JsValue>;

    #[wasm_bindgen(method, catch, js_name = startInput)]
    fn start_input(this: &JsBridge, id: &str) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = stopInput)]
    fn stop_input(this: &JsBridge, id: &str) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch)]
    fn shutdown(this: &JsBridge) -> Result<(), JsValue>;

    #[wasm_bindgen(method, catch, js_name = setEventHandler)]
    fn set_event_handler(this: &JsBridge, handler: &JsValue) -> Result<(), JsValue>;
}

fn bridge() -> Option<JsBridge> {
    let value = Reflect::get(&js_sys::global(), &JsValue::from_str(BRIDGE_GLOBAL)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(value.unchecked_into())
}

fn require_bridge() -> Result<JsBridge, HidError> {
    bridge().ok_or(HidError::NotSupported)
}

fn bridge_error(op: &str, error: JsValue) -> HidError {
    HidError::Io(format!("bridge {op} failed: {error:?}"))
}

async fn await_bridge(op: &str, promise: Result<Promise, JsValue>) -> Result<JsValue, HidError> {
    let promise = promise.map_err(|e| bridge_error(op, e))?;
    JsFuture::from(promise)
        .await
        .map_err(|e| bridge_error(op, e))
}

////////////////////////////////////////
// Interfaces
////////////////////////////////////////

pub(crate) fn is_supported() -> bool {
    bridge().is_some()
}

pub(crate) fn available(uuid: u128) -> bool {
    DEVICE_LIST.with(|list| list.borrow().contains_key(&uuid))
}

pub(crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    with_device(uuid, |dev| dev.vendor_id)
}

pub(crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    with_device(uuid, |dev| dev.product_id)
}

pub(crate) fn get_serial_number(uuid: u128) -> Result<Option<String>, HidError> {
    with_device(uuid, |dev| dev.serial_number.clone())
}

pub(crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    with_device(uuid, |dev| dev.product_name.clone())
}

pub(crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    with_device(uuid, |dev| dev.descriptor.clone())
}

pub(crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    with_device(uuid, |dev| dev.report_info.contains_key(&report_id))
}

/// Replace the VID/PID allow-list used when adopting already-authorized
/// devices. An empty list intentionally disables automatic device adoption.
pub(crate) fn set_device_filters(vendor_ids: Vec<(u16, Option<u16>)>) {
    ACCEPTED_FILTERS.with(|filters| {
        *filters.borrow_mut() = vendor_ids;
    });
    FILTERS_CONFIGURED.with(|configured| configured.set(true));
}

pub(crate) async fn init() -> Result<(), HidError> {
    let bridge = require_bridge()?;

    ACCEPTED_FILTERS.with(|filters| {
        let mut filters = filters.borrow_mut();
        if !FILTERS_CONFIGURED.with(Cell::get) && filters.is_empty() {
            filters.push((DEFAULT_VENDOR_ID, None));
        }
    });

    // Install the handler first so no hot-plug event is missed between the
    // initial enumeration and the subscription.
    install_event_handler(&bridge)?;
    await_bridge("init", bridge.init()).await?;

    let devices = await_bridge("listDevices", bridge.list_devices()).await?;
    for info in parse_device_infos(&devices)? {
        let bridge_id = info.bridge_id.clone();
        if let Err(e) = add_device(info).await {
            log::warn!("bridge: failed to add device {bridge_id}: {e}");
        }
    }
    Ok(())
}

pub(crate) fn shutdown() -> Result<(), HidError> {
    if let Some(bridge) = bridge() {
        // Detach the handler before its closures are dropped below.
        let _ = bridge.set_event_handler(&JsValue::NULL);
        let streaming: Vec<String> = STREAMING.with(|set| {
            set.borrow_mut()
                .drain()
                .filter_map(|uuid| bridge_id_of(uuid))
                .collect()
        });
        for id in streaming {
            let _ = bridge.stop_input(&id);
        }
        let _ = bridge.shutdown();
    }
    EVENT_HANDLER.with(|handler| handler.borrow_mut().take());
    DEVICE_LIST.with(|list| list.borrow_mut().clear());
    BRIDGE_IDS.with(|ids| ids.borrow_mut().clear());
    OPENING_DEVICES.with(|set| set.borrow_mut().clear());
    CANCELLED_OPENINGS.with(|set| set.borrow_mut().clear());
    STREAMING.with(|set| set.borrow_mut().clear());
    DEVICE_REPORT_LISTENERS.with(|listeners| listeners.borrow_mut().clear());
    Ok(())
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
    let bridge = require_bridge()?;
    set_device_filters(vendor_ids.clone());

    let filters = Array::new();
    for (vendor_id, product_id) in &vendor_ids {
        let filter = Object::new();
        let _ = Reflect::set(&filter, &"vendorId".into(), &JsValue::from(*vendor_id));
        if let Some(product_id) = product_id {
            let _ = Reflect::set(&filter, &"productId".into(), &JsValue::from(*product_id));
        }
        filters.push(&filter);
    }

    let devices = await_bridge("requestDevices", bridge.request_devices(&filters)).await?;
    let mut uuids = Vec::new();
    for info in parse_device_infos(&devices)? {
        if let Some(uuid) = add_device(info).await? {
            uuids.push(uuid);
        }
    }
    Ok(uuids)
}

pub(crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    Ok(DEVICE_LIST.with(|list| list.borrow().keys().copied().collect()))
}

pub(crate) fn register_connection_listener(
    id: SubscriptionId,
    callback: ConnectionCallback,
) -> Result<(), HidError> {
    let uuids: Vec<u128> = DEVICE_LIST.with(|list| list.borrow().keys().copied().collect());
    DEVICE_CONNECTION_LISTENERS.with(|listeners| {
        listeners.borrow_mut().insert(id, callback.clone());
    });
    for uuid in uuids {
        callback(DeviceId(uuid), true);
    }
    Ok(())
}

pub(crate) fn unregister_connection_listener(id: SubscriptionId) -> Result<(), HidError> {
    DEVICE_CONNECTION_LISTENERS.with(|listeners| {
        listeners.borrow_mut().remove(&id);
    });
    Ok(())
}

pub(crate) async fn send_report(uuid: u128, data: Vec<u8>) -> Result<usize, HidError> {
    if data.is_empty() {
        return Err(HidError::EmptyData);
    }
    let report_id = data[0];
    let (bridge_id, size) = DEVICE_LIST.with(|list| {
        let list = list.borrow();
        let dev = list.get(&uuid).ok_or(HidError::DeviceNotFound(uuid))?;
        let size = dev
            .report_info
            .get(&report_id)
            .map(|r| r.size)
            .ok_or(HidError::ReportIdMissing { uuid, report_id })?;
        Ok::<_, HidError>((dev.bridge_id.clone(), size))
    })?;

    if data.len() > size {
        return Err(HidError::DataTooLarge {
            max: size,
            got: data.len(),
        });
    }
    let mut send_data = data;
    send_data.resize(size, 0);

    let bridge = require_bridge()?;
    let payload = Uint8Array::from(send_data.as_slice());
    let written = await_bridge("sendReport", bridge.send_report(&bridge_id, &payload)).await?;
    match written.as_f64() {
        Some(n) if n >= 0.0 => Ok(n as usize),
        _ => Err(HidError::Io(format!(
            "bridge sendReport failed for device {uuid:032x}: {written:?}"
        ))),
    }
}

pub(crate) fn register_report_listener(
    uuid: u128,
    id: SubscriptionId,
    callback: ReportCallback,
) -> Result<(), HidError> {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        listeners
            .borrow_mut()
            .entry(uuid)
            .or_default()
            .insert(id, callback);
    });

    let start = STREAMING.with(|set| set.borrow_mut().insert(uuid));
    if start {
        if let (Some(bridge), Some(bridge_id)) = (bridge(), bridge_id_of(uuid)) {
            if let Err(e) = bridge.start_input(&bridge_id) {
                STREAMING.with(|set| set.borrow_mut().remove(&uuid));
                return Err(bridge_error("startInput", e));
            }
        } else {
            STREAMING.with(|set| set.borrow_mut().remove(&uuid));
        }
    }
    Ok(())
}

pub(crate) fn unregister_report_listener(uuid: u128, id: SubscriptionId) -> Result<(), HidError> {
    let now_empty = DEVICE_REPORT_LISTENERS.with(|listeners| {
        let mut binding = listeners.borrow_mut();
        match binding.get_mut(&uuid) {
            Some(map) => {
                map.remove(&id);
                if map.is_empty() {
                    binding.remove(&uuid);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    });
    if now_empty && STREAMING.with(|set| set.borrow_mut().remove(&uuid)) {
        if let (Some(bridge), Some(bridge_id)) = (bridge(), bridge_id_of(uuid)) {
            bridge
                .stop_input(&bridge_id)
                .map_err(|e| bridge_error("stopInput", e))?;
        }
    }
    Ok(())
}

////////////////////////////////////////
// Global variables
////////////////////////////////////////
thread_local! {
    static DEVICE_LIST: RefCell<HashMap<u128, BridgeDevice>> = RefCell::new(HashMap::new());
    static BRIDGE_IDS: RefCell<HashMap<String, u128>> = RefCell::new(HashMap::new());
    static DEVICE_CONNECTION_LISTENERS: RefCell<HashMap<SubscriptionId, ConnectionCallback>> = RefCell::new(HashMap::new());
    static DEVICE_REPORT_LISTENERS: RefCell<HashMap<u128, HashMap<SubscriptionId, ReportCallback>>> = RefCell::new(HashMap::new());
    static STREAMING: RefCell<HashSet<u128>> = RefCell::new(HashSet::new());
    static ACCEPTED_FILTERS: RefCell<Vec<(u16, Option<u16>)>> = const { RefCell::new(Vec::new()) };
    static FILTERS_CONFIGURED: Cell<bool> = const { Cell::new(false) };
    static OPENING_DEVICES: RefCell<HashMap<String, Adoption>> = RefCell::new(HashMap::new());
    static CANCELLED_OPENINGS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static EVENT_HANDLER: RefCell<Option<EventHandler>> = const { RefCell::new(None) };
}

////////////////////////////////////////
// Internal structures
////////////////////////////////////////
struct BridgeDevice {
    bridge_id: String,
    vendor_id: u16,
    product_id: u16,
    product_name: Option<String>,
    serial_number: Option<String>,
    report_info: HashMap<u8, HidReportInfo>,
    descriptor: HidReportDescriptor,
}

/// An in-flight `add_device_inner`, shared so that a hot-plug event and a
/// `request_device` racing for the same device both observe its outcome.
type Adoption = Shared<LocalBoxFuture<'static, Result<Option<u128>, String>>>;

/// `DeviceInfo` as delivered by the host bridge.
struct DeviceInfo {
    bridge_id: String,
    vendor_id: u16,
    product_id: u16,
    product_name: Option<String>,
    serial_number: Option<String>,
}

/// Keeps the handler object and its closures alive while installed.
struct EventHandler {
    _object: Object,
    _on_input_report: Closure<dyn Fn(JsValue, JsValue)>,
    _on_device_added: Closure<dyn Fn(JsValue)>,
    _on_device_removed: Closure<dyn Fn(JsValue)>,
}

fn parse_device_infos(value: &JsValue) -> Result<Vec<DeviceInfo>, HidError> {
    let array = value
        .dyn_ref::<Array>()
        .ok_or_else(|| HidError::Other("bridge returned a non-array device list".to_string()))?;
    array.iter().map(|item| parse_device_info(&item)).collect()
}

fn parse_device_info(value: &JsValue) -> Result<DeviceInfo, HidError> {
    let get = |key: &str| Reflect::get(value, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED);
    let number = |key: &str| {
        get(key)
            .as_f64()
            .filter(|n| (0.0..=f64::from(u16::MAX)).contains(n))
            .map(|n| n as u16)
            .ok_or_else(|| HidError::Other(format!("bridge DeviceInfo has an invalid `{key}`")))
    };
    let text = |key: &str| get(key).as_string().filter(|s| !s.is_empty());

    Ok(DeviceInfo {
        bridge_id: text("uuid")
            .ok_or_else(|| HidError::Other("bridge DeviceInfo is missing `uuid`".to_string()))?,
        vendor_id: number("vendorId")?,
        product_id: number("productId")?,
        product_name: text("productName"),
        serial_number: text("serialNumber"),
    })
}

fn with_device<T>(uuid: u128, f: impl FnOnce(&BridgeDevice) -> T) -> Result<T, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(f(dev)),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

fn uuid_of(bridge_id: &str) -> Option<u128> {
    BRIDGE_IDS.with(|ids| ids.borrow().get(bridge_id).copied())
}

fn bridge_id_of(uuid: u128) -> Option<String> {
    DEVICE_LIST.with(|list| list.borrow().get(&uuid).map(|dev| dev.bridge_id.clone()))
}

////////////////////////////////////////
// Event notification
////////////////////////////////////////
fn notify_connection_changed(uuid: u128, connected: bool) {
    let listeners: Vec<ConnectionCallback> = DEVICE_CONNECTION_LISTENERS
        .with(|listeners| listeners.borrow().values().cloned().collect());
    for listener in listeners {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(DeviceId(uuid), connected);
        }));
        if result.is_err() {
            log::error!("bridge connection callback panicked for device {uuid:032x}");
        }
    }
}

fn notify_report_arrive(uuid: u128, report: Vec<u8>) {
    let listeners: Vec<ReportCallback> =
        DEVICE_REPORT_LISTENERS.with(|listeners| match listeners.borrow().get(&uuid) {
            Some(map) => map.values().cloned().collect(),
            None => Vec::new(),
        });
    if listeners.is_empty() {
        return;
    }
    let shared: Arc<[u8]> = Arc::from(report.into_boxed_slice());
    for listener in listeners {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(DeviceId(uuid), shared.clone());
        }));
        if result.is_err() {
            log::error!("bridge report callback panicked for device {uuid:032x}");
        }
    }
}

////////////////////////////////////////
// Bridge events
////////////////////////////////////////
fn install_event_handler(bridge: &JsBridge) -> Result<(), HidError> {
    // Dispatched synchronously: the host already delivers one message per
    // report, so a microtask hop would only add latency.
    let on_input_report = Closure::wrap(Box::new(|id: JsValue, data: JsValue| {
        let Some(id) = id.as_string() else {
            return;
        };
        let Some(uuid) = uuid_of(&id) else {
            log::warn!("bridge: dropping input report for unknown device {id}");
            return;
        };
        let Some(data) = data.dyn_ref::<Uint8Array>() else {
            log::warn!("bridge: input report payload is not a Uint8Array");
            return;
        };
        notify_report_arrive(uuid, data.to_vec());
    }) as Box<dyn Fn(JsValue, JsValue)>);

    let on_device_added = Closure::wrap(Box::new(|info: JsValue| {
        let info = match parse_device_info(&info) {
            Ok(info) => info,
            Err(e) => {
                log::warn!("bridge: ignoring malformed deviceAdded event: {e}");
                return;
            }
        };
        wasm_bindgen_futures::spawn_local(async move {
            let bridge_id = info.bridge_id.clone();
            if let Err(e) = add_device(info).await {
                log::warn!("bridge: failed to add device {bridge_id}: {e}");
            }
        });
    }) as Box<dyn Fn(JsValue)>);

    let on_device_removed = Closure::wrap(Box::new(|id: JsValue| {
        if let Some(id) = id.as_string() {
            handle_device_removed(&id);
        }
    }) as Box<dyn Fn(JsValue)>);

    let object = Object::new();
    let _ = Reflect::set(&object, &"onInputReport".into(), on_input_report.as_ref());
    let _ = Reflect::set(&object, &"onDeviceAdded".into(), on_device_added.as_ref());
    let _ = Reflect::set(&object, &"onDeviceRemoved".into(), on_device_removed.as_ref());
    bridge
        .set_event_handler(&object)
        .map_err(|e| bridge_error("setEventHandler", e))?;

    EVENT_HANDLER.with(|handler| {
        *handler.borrow_mut() = Some(EventHandler {
            _object: object,
            _on_input_report: on_input_report,
            _on_device_added: on_device_added,
            _on_device_removed: on_device_removed,
        });
    });
    Ok(())
}

fn handle_device_removed(bridge_id: &str) {
    match BRIDGE_IDS.with(|ids| ids.borrow_mut().remove(bridge_id)) {
        Some(uuid) => remove_device(uuid),
        None => {
            if OPENING_DEVICES.with(|map| map.borrow().contains_key(bridge_id)) {
                CANCELLED_OPENINGS.with(|set| set.borrow_mut().insert(bridge_id.to_string()));
            }
        }
    }
}

////////////////////////////////////////
// Internal functions
////////////////////////////////////////
/// Adopt a device announced by the bridge. Returns its id, or `None` when
/// it was filtered out. Concurrent calls for the same device share one
/// adoption and return the same id.
async fn add_device(info: DeviceInfo) -> Result<Option<u128>, HidError> {
    let bridge_id = info.bridge_id.clone();
    if let Some(uuid) = uuid_of(&bridge_id) {
        return Ok(Some(uuid));
    }
    if let Some(adoption) = OPENING_DEVICES.with(|map| map.borrow().get(&bridge_id).cloned()) {
        return adoption.await.map_err(HidError::Other);
    }

    CANCELLED_OPENINGS.with(|set| set.borrow_mut().remove(&bridge_id));
    let adoption: Adoption = add_device_inner(info)
        .map(|result| result.map_err(|e| e.to_string()))
        .boxed_local()
        .shared();
    OPENING_DEVICES.with(|map| map.borrow_mut().insert(bridge_id.clone(), adoption.clone()));
    let result = adoption.await;
    OPENING_DEVICES.with(|map| map.borrow_mut().remove(&bridge_id));
    result.map_err(HidError::Other)
}

async fn add_device_inner(info: DeviceInfo) -> Result<Option<u128>, HidError> {
    let accepted = ACCEPTED_FILTERS.with(|filters| {
        filters.borrow().iter().any(|(vendor_id, product_id)| {
            info.vendor_id == *vendor_id
                && product_id.is_none_or(|product_id| info.product_id == product_id)
        })
    });
    if !accepted {
        return Ok(None);
    }

    let bridge = require_bridge()?;
    let raw = await_bridge(
        "getReportDescriptor",
        bridge.get_report_descriptor(&info.bridge_id),
    )
    .await?;
    let raw = raw
        .dyn_ref::<Uint8Array>()
        .ok_or_else(|| HidError::Other("bridge returned a non-Uint8Array descriptor".to_string()))?
        .to_vec();
    let descriptor = HidReportDescriptor::from_raw(&raw)
        .ok_or_else(|| HidError::Other("failed to parse report descriptor".to_string()))?;

    if !crate::evaluate_device_filter(&descriptor) {
        return Ok(None);
    }

    let cancelled = CANCELLED_OPENINGS.with(|set| set.borrow_mut().remove(&info.bridge_id));
    if cancelled {
        return Err(HidError::Io("device disconnected while opening".to_string()));
    }

    let mut report_info = descriptor
        .input_reports
        .iter()
        .map(|r| (r.report_id, r.clone()))
        .collect::<HashMap<_, _>>();
    for report in &descriptor.output_reports {
        report_info.insert(report.report_id, report.clone());
    }

    let uuid = crate::get_uuid();
    BRIDGE_IDS.with(|ids| ids.borrow_mut().insert(info.bridge_id.clone(), uuid));
    DEVICE_LIST.with(|list| {
        list.borrow_mut().insert(
            uuid,
            BridgeDevice {
                bridge_id: info.bridge_id,
                vendor_id: info.vendor_id,
                product_id: info.product_id,
                product_name: info.product_name,
                serial_number: info.serial_number,
                report_info,
                descriptor,
            },
        );
    });
    notify_connection_changed(uuid, true);
    Ok(Some(uuid))
}

fn remove_device(uuid: u128) {
    notify_connection_changed(uuid, false);
    if let Some(dev) = DEVICE_LIST.with(|list| list.borrow_mut().remove(&uuid)) {
        BRIDGE_IDS.with(|ids| ids.borrow_mut().remove(&dev.bridge_id));
    }
    STREAMING.with(|set| set.borrow_mut().remove(&uuid));
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        listeners.borrow_mut().remove(&uuid);
    });
}
