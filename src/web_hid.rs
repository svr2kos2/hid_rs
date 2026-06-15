use crate::{
    hid_error::HidError,
    hid_report_descriptor::{HidReportDescriptor, HidReportInfo},
    ConnectionCallback, DeviceId, ProgressCallback, ReportCallback, SubscriptionId,
};
use std::{cell::RefCell, collections::HashMap, sync::Arc};

use js_sys::{wasm_bindgen, Function, Promise, Uint8Array};
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HidConnectionEvent, HidDevice, HidDeviceFilter, HidDeviceRequestOptions,
    HidInputReportEvent,
};

////////////////////////////////////////
// Constants
////////////////////////////////////////
const SUPPORTED_REPORT_IDS: [u8; 3] = [0x02, 0x21, 0x22];
const DEVICE_OPEN_DELAY_MS: u32 = 500;

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

pub(crate) fn get_serial_number(uuid: u128) -> Result<Option<String>, HidError> {
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

/// WebHID has no background poller, so shutdown is a no-op.
pub(crate) fn shutdown() -> Result<(), HidError> {
    Ok(())
}

pub(crate) async fn init() -> Result<(), HidError> {
    if !is_supported() {
        log::debug!("HID is not supported");
        return Err(HidError::NotSupported);
    }

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

    // Prevent closures from being dropped
    connect.forget();
    disconnect.forget();

    Ok(())
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
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

    firmware.push(check_sum);
    firmware.push(encrypt);
    firmware.push(write_data_cmd);
    firmware.push(size_addr);
    firmware.push(big_endian);
    firmware.push(err_for_size);

    let promise = match send_fun.call2(
        &JsValue::NULL,
        &device,
        &Uint8Array::from(firmware.as_slice()),
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
                Ok(firmware.len())
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
        listener(DeviceId(uuid), connected);
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
        listener(DeviceId(uuid), shared.clone());
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
        match find_device(&device) {
            Some(uuid) => remove_device(uuid).await,
            None => (),
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
    let uuid = match find_device(&device) {
        Some(u) => u,
        None => return,
    };

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

async fn add_device(device: HidDevice) -> Result<Option<u128>, HidError> {
    match find_device(&device) {
        Some(_) => {
            log::debug!("device already exist");
            return Ok(None);
        }
        None => (),
    };

    future_delay(DEVICE_OPEN_DELAY_MS).await;
    let collections = get_collections_by_device(device.clone());

    let has_report_id = collections
        .output_reports
        .iter()
        .any(|r| SUPPORTED_REPORT_IDS.contains(&r.report_id));

    if !has_report_id {
        return Ok(None);
    }

    match JsFuture::from(device.open()).await {
        Ok(_) => log::debug!("open device done"),
        Err(err) => log::debug!("open device failed {:?}", err),
    }

    // Dispatch inputreport synchronously from the browser-fired event
    // (no spawn_local microtask hop) to minimize per-report latency.
    let on_report = Closure::wrap(Box::new(|event: JsValue| {
        handle_input_report_event(event);
    }) as Box<dyn Fn(JsValue)>);

    device.set_oninputreport(Some(on_report.as_ref().unchecked_ref()));
    on_report.forget();

    // Mint outside the reserved dummy-device range.
    let uuid = crate::get_uuid();
    let report_info = collections
        .input_reports
        .iter()
        .map(|r| (r.report_id, r.clone()))
        .collect();
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
    notify_connection_changed(uuid, true);
    Ok(Some(uuid))
}

async fn remove_device(uuid: u128) {
    notify_connection_changed(uuid, false);
    DEVICE_LIST.with(|list| {
        list.borrow_mut().remove(&uuid);
    });
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        listeners.borrow_mut().remove(&uuid);
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
