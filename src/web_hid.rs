use crate::{
    hid_error::HidError,
    hid_report_descriptor::{HidReportDescriptor, HidReportInfo},
    ConnectionCallback, DeviceId, ProgressCallback, ReportCallback, SubscriptionId,
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    sync::Arc,
};

use js_sys::{wasm_bindgen, Function, Promise, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HidConnectionEvent, HidDevice, HidDeviceFilter, HidDeviceRequestOptions, HidInputReportEvent,
};

////////////////////////////////////////
// Constants
////////////////////////////////////////
const SUPPORTED_REPORT_IDS: [u8; 3] = [0x02, 0x21, 0x22];
const DEVICE_OPEN_DELAY_MS: u32 = 500;
const DEVICE_SYNC_INTERVAL_MS: i32 = 500;
const DEFAULT_VENDOR_ID: u16 = 0x8089;

////////////////////////////////////////
// Interfaces
////////////////////////////////////////

pub(crate) fn available(uuid: u128) -> bool {
    DEVICE_LIST.with(|list| list.borrow().contains_key(&uuid))
}

pub(crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(dev.device.vendor_id()),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

pub(crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(dev.device.product_id()),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

pub(crate) fn get_serial_number(_uuid: u128) -> Result<Option<String>, HidError> {
    Ok(None)
}

pub(crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(Some(dev.device.product_name())),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

pub(crate) fn is_supported() -> bool {
    match web_sys::window() {
        Some(window) => {
            js_sys::Reflect::has(&window.navigator(), &JsValue::from_str("hid")).unwrap_or(false)
        }
        None => false,
    }
}

/// Replace the VID/PID allow-list used when adopting already-authorized HID
/// devices. An empty list intentionally disables automatic device adoption.
pub(crate) fn set_device_filters(vendor_ids: Vec<(u16, Option<u16>)>) {
    ACCEPTED_FILTERS.with(|filters| {
        *filters.borrow_mut() = vendor_ids;
    });
    FILTERS_CONFIGURED.with(|configured| configured.set(true));
}

pub(crate) fn shutdown() -> Result<(), HidError> {
    stop_device_sync_timer();
    if let Ok(api) = get_api() {
        api.set_onconnect(None);
        api.set_ondisconnect(None);
    }
    CONNECTION_HANDLERS.with(|handlers| handlers.borrow_mut().take());
    DEVICE_LIST.with(|list| {
        for package in list.borrow().values() {
            package.device.set_oninputreport(None);
        }
    });
    REPORT_HANDLERS.with(|handlers| handlers.borrow_mut().clear());
    DEVICE_LIST.with(|list| list.borrow_mut().clear());
    OPENING_DEVICES.with(|devices| devices.borrow_mut().clear());
    CANCELLED_OPENINGS.with(|devices| devices.borrow_mut().clear());
    DEVICE_REPORT_LISTENERS.with(|listeners| listeners.borrow_mut().clear());
    Ok(())
}

pub(crate) async fn init() -> Result<(), HidError> {
    if !is_supported() {
        log::debug!("HID is not supported");
        return Err(HidError::NotSupported);
    }

    ACCEPTED_FILTERS.with(|filters| {
        let mut filters = filters.borrow_mut();
        if !FILTERS_CONFIGURED.with(Cell::get) && filters.is_empty() {
            filters.push((DEFAULT_VENDOR_ID, None));
        }
    });

    let api = get_api()?;
    let promise = api.get_devices();
    let result = JsFuture::from(promise).await;
    let devices = match result {
        Ok(d) => d,
        Err(e) => {
            return Err(HidError::Io(format!("FAILED to get HID devices: {:?}", e)));
        }
    };
    let devs_array = match devices.dyn_ref::<js_sys::Array>() {
        Some(a) => a,
        None => {
            return Err(HidError::Other(
                "failed to cast HID devices to array".to_string(),
            ));
        }
    };
    for device_value in devs_array.iter() {
        let device = device_value
            .dyn_into::<HidDevice>()
            .map_err(|_| HidError::Other("failed to cast to HidDevice".to_string()))?;
        match add_device(device).await {
            Ok(_) => (),
            Err(e) => {
                return Err(HidError::Io(format!("FAILED to add device: {:?}", e)));
            }
        };
    }

    // Create event handlers using Closure instead of Function with string code
    let connect = Closure::wrap(Box::new(|event: JsValue| {
        wasm_bindgen_futures::spawn_local(async move {
            // returns JS Promise; nothing to log here
            let _ = on_connection_changed(event, true).await;
        });
    }) as Box<dyn Fn(JsValue)>);

    let disconnect = Closure::wrap(Box::new(|event: JsValue| {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = on_connection_changed(event, false).await;
        });
    }) as Box<dyn Fn(JsValue)>);

    let api = get_api()?;
    api.set_onconnect(Some(connect.as_ref().unchecked_ref()));
    api.set_ondisconnect(Some(disconnect.as_ref().unchecked_ref()));

    CONNECTION_HANDLERS.with(|handlers| {
        *handlers.borrow_mut() = Some((connect, disconnect));
    });

    start_device_sync_timer();

    Ok(())
}

fn start_device_sync_timer() {
    if DEVICE_SYNC_INTERVAL.with(Cell::get).is_some() {
        return;
    }
    let callback = Closure::wrap(Box::new(|| {
        if DEVICE_SYNC_RUNNING.replace(true) {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = reconcile_devices().await {
                log::debug!("periodic WebHID device sync failed: {error:?}");
            }
            DEVICE_SYNC_RUNNING.set(false);
        });
    }) as Box<dyn FnMut()>);
    let interval = web_sys::window().and_then(|window| {
        window
            .set_interval_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                DEVICE_SYNC_INTERVAL_MS,
            )
            .ok()
    });
    if let Some(interval) = interval {
        DEVICE_SYNC_INTERVAL.set(Some(interval));
        DEVICE_SYNC_HANDLER.with(|handler| *handler.borrow_mut() = Some(callback));
    }
}

fn stop_device_sync_timer() {
    if let (Some(interval), Some(window)) = (DEVICE_SYNC_INTERVAL.take(), web_sys::window()) {
        window.clear_interval_with_handle(interval);
    }
    DEVICE_SYNC_HANDLER.with(|handler| *handler.borrow_mut() = None);
    DEVICE_SYNC_RUNNING.set(false);
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
    set_device_filters(vendor_ids.clone());
    let filters: Vec<HidDeviceFilter> = vendor_ids
        .iter()
        .map(|(vendor_id, pid)| {
            let filter = HidDeviceFilter::new();
            filter.set_vendor_id(u32::from(*vendor_id));
            if let Some(product_id) = pid {
                filter.set_product_id(*product_id);
            }
            filter
        })
        .collect();

    let options = HidDeviceRequestOptions::new(&filters);
    let promise = get_api()?.request_device(&options);
    let result = JsFuture::from(promise).await;
    let devices = match result {
        Ok(d) => d,
        Err(e) => {
            return Err(HidError::Io(format!(
                "FAILED to request HID devices: {:?}",
                e
            )));
        }
    };
    let devs_array = match devices.dyn_ref::<js_sys::Array>() {
        Some(a) => a,
        None => {
            return Err(HidError::Other(
                "failed to cast HID devices to array".to_string(),
            ));
        }
    };
    if devs_array.length() < 1 {
        return Ok(Vec::new());
    }

    let mut uuids = Vec::new();
    for device_value in devs_array.iter() {
        let device = device_value
            .dyn_into::<HidDevice>()
            .map_err(|_| HidError::Other("failed to cast to HidDevice".to_string()))?;
        log::debug!("request device {:?}", device.product_name());
        match find_device(&device) {
            Some(uuid) => {
                log::debug!("device already exist");
                uuids.push(uuid);
                continue;
            }
            None => (),
        }
        match add_device(device).await {
            Ok(uuid) => match uuid {
                Some(u) => uuids.push(u),
                None => (),
            },
            Err(e) => {
                return Err(HidError::Io(format!("FAILED to add device: {:?}", e)));
            }
        };
    }
    Ok(uuids)
}

pub(crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let list = DEVICE_LIST.with(|list| list.borrow().keys().copied().collect());
    Ok(list)
}

/// Reconcile the WebHID cache with all currently-connected authorized devices.
pub(crate) async fn reconcile_devices() -> Result<Vec<u128>, HidError> {
    let promise = get_api()?.get_devices();
    let result = JsFuture::from(promise).await;
    let devices = match result {
        Ok(d) => d,
        Err(error) => {
            return Err(HidError::Io(format!(
                "FAILED to enumerate known HID devices: {error:?}"
            )));
        }
    };
    let devices = devices
        .dyn_ref::<js_sys::Array>()
        .ok_or_else(|| HidError::Other("failed to cast known HID devices to array".to_string()))?;

    let current: Vec<HidDevice> = devices
        .iter()
        .filter_map(|value| value.dyn_into::<HidDevice>().ok())
        .collect();

    for device in current {
        if find_device(&device).is_none() {
            let _ = add_device(device).await?;
        }
    }

    get_device_list()
}

pub(crate) fn register_connection_listener(
    id: SubscriptionId,
    callback: ConnectionCallback,
) -> Result<(), HidError> {
    log::debug!("register_connection_listener");
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

pub(crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(dev.descriptor.clone()),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

pub(crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    Ok(pack.report_info.contains_key(&report_id))
}

pub(crate) async fn send_report(uuid: u128, data: Vec<u8>) -> Result<usize, HidError> {
    if data.is_empty() {
        return Err(HidError::EmptyData);
    }
    let report_id = data[0];
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    let size = match pack.report_info.get(&report_id) {
        Some(r) => r.size,
        None => return Err(HidError::ReportIdMissing { uuid, report_id }),
    };

    if data.len() > size {
        return Err(HidError::DataTooLarge {
            max: size,
            got: data.len(),
        });
    }

    let device = pack.device;
    ensure_device_open(&device).await?;

    let mut send_data = data.to_vec();
    send_data.resize(size, 0);
    if let Err(e) = device.send_report_with_u8_slice(report_id, &mut send_data[1..]) {
        return Err(HidError::Io(format!("WebHID send_report failed: {e:?}")));
    }
    Ok(send_data.len())
}

// Helper function to ensure device is open
async fn ensure_device_open(device: &HidDevice) -> Result<(), HidError> {
    if !device.opened() {
        let promise = device.open();
        match JsFuture::from(promise).await {
            Ok(_) => log::debug!("open device done"),
            Err(err) => return Err(HidError::Io(format!("FAILED to open device: {:?}", err))),
        }
    }
    Ok(())
}

#[wasm_bindgen]
pub async fn send_firmware_progress(progress: JsValue) -> Promise {
    let progress = match progress.as_f64() {
        Some(p) => p,
        None => {
            log::debug!("Invalid progress value");
            return Promise::resolve(&JsValue::NULL);
        }
    };

    let listener = SEND_FIRMWARE_PROGRESS.with(|listeners| listeners.borrow().clone());

    if let Some(listener) = listener {
        listener(progress);
    }
    Promise::resolve(&JsValue::NULL)
}

const JS_CODE: &str = include_str!("../js/send_firmware.js");
pub(crate) async fn send_firmware(
    uuid: u128,
    firmware: &mut Vec<u8>,
    write_data_cmd: u8,
    size_addr: u8,
    big_endian: u8,
    err_for_size: u8,
    encrypt: u8,
    check_sum: u8,
    on_progress: ProgressCallback,
) -> Result<usize, HidError> {
    SEND_FIRMWARE_PROGRESS.with(|progress| {
        *progress.borrow_mut() = Some(on_progress);
    });

    log::debug!("send_firmware {:?}", firmware.len());
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    let device = pack.device;
    ensure_device_open(&device).await?;

    let send_fun = Function::new_with_args("device, firmware", JS_CODE);

    let mut payload = firmware.clone();
    payload.push(check_sum);
    payload.push(encrypt);
    payload.push(write_data_cmd);
    payload.push(size_addr);
    payload.push(big_endian);
    payload.push(err_for_size);

    let promise = match send_fun.call2(
        &JsValue::NULL,
        &device,
        &Uint8Array::from(payload.as_slice()),
    ) {
        Ok(p) => Promise::from(p),
        Err(err) => {
            log::debug!("FAILED to send report: {:?}", err);
            remove_device(uuid).await;
            // Clean up progress listener
            SEND_FIRMWARE_PROGRESS.with(|progress| {
                *progress.borrow_mut() = None;
            });
            return Err(HidError::Io(format!("FAILED to send report: {:?}", err)));
        }
    };

    let res = match JsFuture::from(promise).await {
        Ok(success) => {
            if success.as_bool().unwrap_or(false) {
                Ok(payload.len())
            } else {
                Err(HidError::Io("failed to send firmware".to_string()))
            }
        }
        Err(err) => {
            log::debug!("FAILED to send report: {:?}", err);
            remove_device(uuid).await;
            Err(HidError::Io(format!("FAILED to send report: {:?}", err)))
        }
    };

    // Always clean up progress listener
    SEND_FIRMWARE_PROGRESS.with(|progress| {
        *progress.borrow_mut() = None;
    });

    res
}

pub(crate) fn register_report_listener(
    uuid: u128,
    id: SubscriptionId,
    callback: ReportCallback,
) -> Result<(), HidError> {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        let mut binding = listeners.borrow_mut();
        binding.entry(uuid).or_default().insert(id, callback);
    });
    Ok(())
}

pub(crate) fn unregister_report_listener(uuid: u128, id: SubscriptionId) -> Result<(), HidError> {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        let mut binding = listeners.borrow_mut();
        if let Some(map) = binding.get_mut(&uuid) {
            map.remove(&id);
            if map.is_empty() {
                binding.remove(&uuid);
            }
        }
    });
    Ok(())
}

////////////////////////////////////////
// Global variables
////////////////////////////////////////
thread_local! {
    static SEND_FIRMWARE_PROGRESS: RefCell<Option<ProgressCallback>> = RefCell::new(None);
    static DEVICE_LIST: RefCell<HashMap<u128, HidDevicePackage>> = RefCell::new(HashMap::new());
    static DEVICE_CONNECTION_LISTENERS: RefCell<HashMap<SubscriptionId, ConnectionCallback>> = RefCell::new(HashMap::new());
    static DEVICE_REPORT_LISTENERS: RefCell<HashMap<u128, HashMap<SubscriptionId, ReportCallback>>> = RefCell::new(HashMap::new());
    static ACCEPTED_FILTERS: RefCell<Vec<(u16, Option<u16>)>> = const { RefCell::new(Vec::new()) };
    static FILTERS_CONFIGURED: Cell<bool> = const { Cell::new(false) };
    static CONNECTION_HANDLERS: RefCell<Option<(Closure<dyn Fn(JsValue)>, Closure<dyn Fn(JsValue)>)>> = RefCell::new(None);
    static REPORT_HANDLERS: RefCell<HashMap<u128, Closure<dyn Fn(JsValue)>>> = RefCell::new(HashMap::new());
    static OPENING_DEVICES: RefCell<Vec<HidDevice>> = const { RefCell::new(Vec::new()) };
    static CANCELLED_OPENINGS: RefCell<Vec<HidDevice>> = const { RefCell::new(Vec::new()) };
    static DEVICE_SYNC_INTERVAL: Cell<Option<i32>> = const { Cell::new(None) };
    static DEVICE_SYNC_HANDLER: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
    static DEVICE_SYNC_RUNNING: Cell<bool> = const { Cell::new(false) };
}

////////////////////////////////////////
// Internal structures
////////////////////////////////////////
#[derive(Clone)]
struct HidDevicePackage {
    device: HidDevice,
    report_info: HashMap<u8, HidReportInfo>,
    descriptor: HidReportDescriptor,
}

////////////////////////////////////////
// Event notification
////////////////////////////////////////
fn notify_connection_changed(uuid: u128, connected: bool) {
    log::debug!("notify_connection_changed {:?}", connected);
    let listeners: Vec<ConnectionCallback> = DEVICE_CONNECTION_LISTENERS
        .with(|listeners| listeners.borrow().values().cloned().collect());
    for listener in listeners {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(DeviceId(uuid), connected);
        }));
        if result.is_err() {
            log::error!("WebHID connection callback panicked for device {uuid:032x}");
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
            log::error!("WebHID report callback panicked for device {uuid:032x}");
        }
    }
}

////////////////////////////////////////
// JavaScript interfaces
////////////////////////////////////////
#[wasm_bindgen]
pub async fn on_connection_changed(event_js: JsValue, connected: bool) -> Promise {
    log::debug!("on_connection_changed {:?}", connected);
    let event = match event_js.dyn_into::<HidConnectionEvent>() {
        Ok(e) => e,
        Err(_) => {
            log::debug!("FAILED to cast JsValue to HidConnectionEvent");
            return Promise::resolve(&JsValue::NULL);
        }
    };
    let device = event.device();
    if connected {
        log::debug!("{}", device.product_name().as_str());
        if let Err(e) = add_device(device).await {
            log::warn!("add_device failed: {e}");
        }
    } else {
        match find_device_for_event(&device) {
            Some(uuid) => remove_device(uuid).await,
            None => cancel_opening(&device),
        };
        match JsFuture::from(device.close()).await {
            Ok(_) => log::debug!("close device done"),
            Err(err) => log::debug!("close device failed {:?}", err),
        };
    }
    Promise::resolve(&JsValue::NULL)
}

#[wasm_bindgen]
pub fn on_device_report_arrived(event_js: JsValue) -> Promise {
    handle_input_report_event(event_js);
    Promise::resolve(&JsValue::NULL)
}

/// Synchronous fast-path for an `inputreport` event.
///
/// Performance notes:
/// - Uses a single bulk `Uint8Array::copy_to` instead of per-byte
///   `DataView::get_uint8` calls (avoids one JS↔WASM boundary crossing
///   per byte of the report payload).
/// - Dispatches synchronously; the caller (the JS `inputreport` event
///   handler) intentionally does not spawn a microtask, so report
///   delivery happens in the same task the browser fires the event on.
fn handle_input_report_event(event_js: JsValue) {
    let event = match event_js.dyn_into::<HidInputReportEvent>() {
        Ok(e) => e,
        Err(_) => {
            log::debug!("FAILED to cast JsValue to HidInputReportEvent");
            return;
        }
    };
    let device = event.device();
    let uuid = match find_device_for_event(&device) {
        Some(u) => u,
        None => {
            log::warn!("dropping input report for an unrecognized WebHID device");
            return;
        }
    };

    dispatch_input_report(uuid, event);
}

fn dispatch_input_report(uuid: u128, event: HidInputReportEvent) {

    let report_id = event.report_id();
    let data_view = event.data();
    let byte_len = data_view.byte_length();

    let mut data: Vec<u8> = vec![0u8; byte_len + 1];
    data[0] = report_id;

    if byte_len > 0 {
        let array = Uint8Array::new_with_byte_offset_and_length(
            &data_view.buffer(),
            data_view.byte_offset() as u32,
            byte_len as u32,
        );
        array.copy_to(&mut data[1..]);
    }

    notify_report_arrive(uuid, data);
}

////////////////////////////////////////
// Internal functions
////////////////////////////////////////
pub(crate) fn get_api() -> Result<web_sys::Hid, HidError> {
    let window =
        web_sys::window().ok_or_else(|| HidError::Other("cannot get window".to_string()))?;
    Ok(window.navigator().hid())
}

fn get_device(uuid: u128) -> Result<HidDevicePackage, HidError> {
    DEVICE_LIST.with(|list| match list.borrow().get(&uuid) {
        Some(dev) => Ok(dev.clone()),
        None => Err(HidError::DeviceNotFound(uuid)),
    })
}

fn find_device(device: &HidDevice) -> Option<u128> {
    DEVICE_LIST.with(|list| {
        for (uuid, dev) in list.borrow().iter() {
            if dev.device.eq(device) {
                return Some(*uuid);
            }
        }
        None
    })
}

fn find_device_for_event(device: &HidDevice) -> Option<u128> {
    if let Some(uuid) = find_device(device) {
        return Some(uuid);
    }

    let matches: Vec<u128> = DEVICE_LIST.with(|list| {
        list.borrow()
            .iter()
            .filter_map(|(uuid, package)| {
                let candidate = &package.device;
                (candidate.vendor_id() == device.vendor_id()
                    && candidate.product_id() == device.product_id()
                    && candidate.product_name() == device.product_name())
                .then_some(*uuid)
            })
            .collect()
    });
    if matches.len() == 1 {
        Some(matches[0])
    } else if matches.is_empty() {
        DEVICE_LIST.with(|list| {
            let list = list.borrow();
            (list.len() == 1).then(|| list.keys().next().copied()).flatten()
        })
    } else {
        None
    }
}

async fn add_device(device: HidDevice) -> Result<Option<u128>, HidError> {
    CANCELLED_OPENINGS.with(|devices| {
        devices.borrow_mut().retain(|candidate| !candidate.eq(&device));
    });

    match find_device(&device) {
        Some(_) => {
            log::debug!("device already exist");
            return Ok(None);
        }
        None => (),
    };

    let already_opening = OPENING_DEVICES.with(|devices| {
        let mut devices = devices.borrow_mut();
        if devices.iter().any(|candidate| candidate.eq(&device)) {
            true
        } else {
            devices.push(device.clone());
            false
        }
    });
    if already_opening {
        log::debug!("device is already opening");
        return Ok(None);
    }

    let result = add_device_inner(device.clone()).await;
    OPENING_DEVICES.with(|devices| {
        devices
            .borrow_mut()
            .retain(|candidate| !candidate.eq(&device));
    });
    result
}

async fn add_device_inner(device: HidDevice) -> Result<Option<u128>, HidError> {
    match find_device(&device) {
        Some(_) => return Ok(None),
        None => (),
    }

    let accepted = ACCEPTED_FILTERS.with(|filters| {
        filters.borrow().iter().any(|(vendor_id, product_id)| {
            device.vendor_id() == *vendor_id
                && product_id.is_none_or(|product_id| device.product_id() == product_id)
        })
    });
    if !accepted {
        return Ok(None);
    }

    let collections = get_collections_by_device(device.clone());

    let has_report_id = collections
        .output_reports
        .iter()
        .any(|r| SUPPORTED_REPORT_IDS.contains(&r.report_id));

    if !has_report_id {
        return Ok(None);
    }

    future_delay(DEVICE_OPEN_DELAY_MS).await;

    let mut open_error = None;
    let mut opened = false;
    for attempt in 0..3 {
        match JsFuture::from(device.open()).await {
            Ok(_) => {
                opened = true;
                log::info!("open device done after attempt {}", attempt + 1);
                break;
            }
            Err(err) => {
                log::warn!("open device attempt {} failed: {:?}", attempt + 1, err);
                open_error = Some(err);
                if attempt < 2 {
                    future_delay(200).await;
                }
            }
        }
    }
    if !opened {
        return Err(HidError::Io(format!(
            "failed to open HID device after retries: {:?}",
            open_error
        )));
    }

    let cancelled = CANCELLED_OPENINGS.with(|devices| {
        devices.borrow().iter().any(|candidate| candidate.eq(&device))
    });
    if cancelled {
        let _ = JsFuture::from(device.close()).await;
        return Err(HidError::Io("device disconnected while opening".to_string()));
    }

    let opened_collections = get_collections_by_device(device.clone());
    let mut collections = collections;
    collections.input_reports.extend(opened_collections.input_reports);
    collections.output_reports.extend(opened_collections.output_reports);
    collections.feature_reports.extend(opened_collections.feature_reports);

    // Dispatch inputreport synchronously from the browser-fired event
    // (no spawn_local microtask hop) to minimize per-report latency.
    let uuid = crate::get_uuid();
    let on_report = Closure::wrap(Box::new(move |event: JsValue| {
        let Ok(event) = event.dyn_into::<HidInputReportEvent>() else {
            log::debug!("FAILED to cast JsValue to HidInputReportEvent");
            return;
        };
        dispatch_input_report(uuid, event);
    }) as Box<dyn Fn(JsValue)>);

    device.set_oninputreport(Some(on_report.as_ref().unchecked_ref()));

    let mut report_info = collections
        .input_reports
        .iter()
        .map(|r| (r.report_id, r.clone()))
        .collect::<HashMap<_, _>>();
    for report in &collections.output_reports {
        report_info.insert(report.report_id, report.clone());
    }
    DEVICE_LIST.with(|list| {
        list.borrow_mut().insert(
            uuid,
            HidDevicePackage {
                device: device.clone(),
                report_info,
                descriptor: collections,
            },
        );
    });
    REPORT_HANDLERS.with(|handlers| {
        handlers.borrow_mut().insert(uuid, on_report);
    });
    notify_connection_changed(uuid, true);
    Ok(Some(uuid))
}

async fn remove_device(uuid: u128) {
    notify_connection_changed(uuid, false);
    let device = DEVICE_LIST.with(|list| {
        list.borrow()
            .get(&uuid)
            .map(|package| package.device.clone())
    });
    if let Some(device) = device {
        device.set_oninputreport(None);
    }
    DEVICE_LIST.with(|list| {
        list.borrow_mut().remove(&uuid);
    });
    REPORT_HANDLERS.with(|handlers| {
        handlers.borrow_mut().remove(&uuid);
    });
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        listeners.borrow_mut().remove(&uuid);
    });
}

fn cancel_opening(device: &HidDevice) {
    OPENING_DEVICES.with(|devices| {
        devices
            .borrow_mut()
            .retain(|candidate| !candidate.eq(device));
    });
    CANCELLED_OPENINGS.with(|devices| {
        let mut devices = devices.borrow_mut();
        if !devices.iter().any(|candidate| candidate.eq(device)) {
            devices.push(device.clone());
        }
    });
}

fn get_collections_by_device(device: HidDevice) -> HidReportDescriptor {
    let collections = device.collections();
    let mut res = HidReportDescriptor::new();

    for collection in collections.iter() {
        if let Some(info) = HidReportDescriptor::from_js_value(collection.into()) {
            res.output_reports.extend(info.output_reports);
            res.input_reports.extend(info.input_reports);
            res.feature_reports.extend(info.feature_reports);
        }
    }
    res
}

async fn future_delay(ms: u32) {
    let promise = Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32)
            .unwrap();
    });
    JsFuture::from(promise).await.unwrap();
}
