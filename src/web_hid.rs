use std::{
    cell::RefCell, 
    collections::HashMap
};
use crate::{
    hid_error::HidError, 
    hid_report_descriptor::{
        HidReportDescriptor, 
        HidReportInfo
    },
    SafeCallback,
    SafeCallback2
};

use js_sys::{
    wasm_bindgen, Function, Promise, Uint8Array
};
use serde::{
    Deserialize, Serialize
};
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    HidConnectionEvent, HidDevice, HidDeviceRequestOptions, HidInputReportEvent
};

////////////////////////////////////////
// Constants
////////////////////////////////////////
const SUPPORTED_REPORT_IDS: [u8; 3] = [0x02, 0x21, 0x22];
const DEVICE_OPEN_DELAY_MS: u32 = 500;

////////////////////////////////////////
// Interfaces
////////////////////////////////////////

pub(in crate) fn available(uuid: u128) -> bool {
    DEVICE_LIST.with(|list| list.borrow().contains_key(&uuid))
}

pub(in crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    DEVICE_LIST.with(|list| {
        match list.borrow().get(&uuid) {
            Some(dev) => Ok(dev.device.vendor_id()),
            None => Err(HidError::new("FAILED to find device")),
        }
    })
}

pub(in crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    DEVICE_LIST.with(|list| {
        match list.borrow().get(&uuid) {
            Some(dev) => Ok(dev.device.product_id()),
            None => Err(HidError::new("FAILED to find device")),
        }
    })
}

pub(in crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    DEVICE_LIST.with(|list| {
        match list.borrow().get(&uuid) {
            Some(dev) => Ok(Some(dev.device.product_name())),
            None => Err(HidError::new("FAILED to find device")),
        }
    })
}

pub(in crate) fn is_supported() -> bool {
    match web_sys::window() {
        Some(window) => {
            js_sys::Reflect::has(&window.navigator(), &JsValue::from_str("hid"))
                .unwrap_or(false)
        },
        None => false,
    }
}

pub(in crate) async fn init() -> Result<(), HidError> {
    if !is_supported() {
        log::debug!("HID is not supported");
        return Err(HidError::new("HID is not supported"));
    }

    let api = get_api()?;
    let promise = api.get_devices();
    let result = JsFuture::from(promise).await;
    let devices = match result {
        Ok(d) => d,
        Err(e) => {
            return Err(HidError::new(&format!("FAILED to get HID devices: {:?}", e)));
        }
    };
    let devs_array = match devices.dyn_ref::<js_sys::Array>() {
        Some(a) => a,
        None => {
            return Err(HidError::new("FAILED to cast HID devices to array"));
        }
    };
    for device_value in devs_array.iter() {
        let device = device_value.dyn_into::<HidDevice>()
            .map_err(|_| HidError::new("Failed to cast to HidDevice"))?;
        match add_device(device).await {
            Ok(_) => (),
            Err(e) => {
                return Err(HidError::new(&format!("FAILED to add device: {:?}", e)));
            }
        };
    }

    // Create event handlers using Closure instead of Function with string code
    let connect = Closure::wrap(Box::new(|event: JsValue| {
        wasm_bindgen_futures::spawn_local(async move {
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

pub(in crate) async fn request_device(vendor_ids: Vec<(u16, Option<u16>)>) -> Result<Vec<u128>, HidError> {
    let filters: Vec<WebHidFilter> = vendor_ids.iter().map(|(vendor_id, pid)| {
        WebHidFilter {
            vendor_id : Some(*vendor_id),
            product_id : *pid,
            usage_page : None,  
            usage : None,
        }
    }).collect();

    let hid_filter = match serde_wasm_bindgen::to_value(&filters) {
        Ok(f) => f,
        Err(e) => {
            return Err(HidError::new(&format!("FAILED to serialize HID filters: {:?}", e)));
        }
    };
    let options = HidDeviceRequestOptions::new(&hid_filter);
    let promise = get_api()?.request_device(&options);
    let result = JsFuture::from(promise).await;
    let devices = match result {
        Ok(d) => d,
        Err(e) => {
            return Err(HidError::new(&format!("FAILED to request HID devices: {:?}", e)));
        }
    };
    let devs_array = match devices.dyn_ref::<js_sys::Array>() {
        Some(a) => a,
        None => {
            return Err(HidError::new("FAILED to cast HID devices to array"));
        }
    };
    if devs_array.length() < 1 {
        return Ok(Vec::new());
    }

    let mut uuids = Vec::new();
    for device_value in devs_array.iter() {
        let device = device_value.dyn_into::<HidDevice>()
            .map_err(|_| HidError::new("Failed to cast to HidDevice"))?;
        log::debug!("request device {:?}", device.product_name());
        match find_device(&device) {
            Some(uuid) => {
                log::debug!("device already exist");
                uuids.push(uuid);
                continue;
            },
            None => (),
        }
        match add_device(device).await {
            Ok(uuid) => match uuid {
                Some(u) => uuids.push(u),
                None => (),
            },
            Err(e) => {
                return Err(HidError::new(&format!("FAILED to add device: {:?}", e)));
            }
        };
    }
    Ok(uuids)
}

pub(in crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let list = DEVICE_LIST.with(|list| {
        list.borrow().keys().copied().collect()
    });
    Ok(list)
}

pub(in crate) async fn sub_connection_changed(callback: SafeCallback2<u128, bool, ()>) -> Result<(), HidError> {
    log::debug!("sub_connection_changed");
    let uuids: Vec<u128> = DEVICE_LIST.with(|list| {
        list.borrow().iter().map(|(uuid, _)| *uuid).collect()
    });
    for uuid in uuids {
        let _ = callback.call(uuid, true).await;
    }
    DEVICE_CONNECTION_LISTENERS.with(|listeners| {
        listeners.borrow_mut().push(callback);
    });
    Ok(())
}

pub(in crate) async fn unsub_connection_changed(callback: SafeCallback2<u128, bool, ()>) -> Result<(), HidError> {
    let uuids: Vec<u128> = DEVICE_LIST.with(|list| {
        list.borrow().keys().map(|k| *k).collect()
    });
    for uuid in uuids {
        let _ = callback.call(uuid, false).await;
    }
    DEVICE_CONNECTION_LISTENERS.with(|listeners| {
        listeners.borrow_mut().retain(|l| !l.ptr_eq(&callback));
    });
    Ok(())
}

pub(in crate) fn get_collections(uuid : u128) -> Result<HidReportDescriptor, HidError> {
    DEVICE_LIST.with(|list| {
        match list.borrow().get(&uuid) {
            Some(dev) => Ok(dev.descriptor.clone()),
            None => Err(HidError::new("FAILED to find device")),
        }
    })
}

pub(in crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    Ok(pack.report_info.contains_key(&report_id))
}

pub(in crate) async fn send_report(uuid: u128, data: &[u8]) -> Result<usize, HidError> {
    if data.is_empty() {
        return Err(HidError::new("FAILED to send report: data is empty"));
    }
    let report_id = data[0];
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    let size = match pack.report_info.get(&report_id) {
        Some(r) => r.size,
        None => return Err(HidError::new("FAILED to send report: report id not found")),
    };

    if data.len() > size {
        return Err(HidError::new("FAILED to send report: data size is too large"));
    }

    let device = pack.device;
    ensure_device_open(&device).await?;

    let mut send_data = data.to_vec();
    send_data.resize(size, 0);
    let _ = device.send_report_with_u8_slice(report_id, &mut send_data[1..]);
    Ok(send_data.len())
}

// Helper function to ensure device is open
async fn ensure_device_open(device: &HidDevice) -> Result<(), HidError> {
    if !device.opened() {
        let promise = device.open();
        match JsFuture::from(promise).await {
            Ok(_) => log::debug!("open device done"),
            Err(err) => return Err(HidError::new(&format!("FAILED to open device: {:?}", err))),
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
    
    let listener = SEND_FIRMWARE_PROGRESS.with(|listeners| {
        listeners.borrow().first().cloned()
    });
    
    if let Some(listener) = listener {
        listener.call(progress).await;
    }
    Promise::resolve(&JsValue::NULL)
}

const JS_CODE: &str = include_str!("send_firmware.js");
pub(in crate) async fn send_firmware(uuid: u128, firmware: &mut Vec<u8>, 
    write_data_cmd:u8, size_addr: u8, big_endian: u8, err_for_size: u8, encrypt: u8, check_sum: u8,
    on_progress: SafeCallback<f64, ()>) -> Result<usize, HidError> {
    
    SEND_FIRMWARE_PROGRESS.with(|progress| {
        progress.borrow_mut().clear();
        progress.borrow_mut().push(on_progress);
    });

    log::debug!("send_firmware {:?}", firmware.len());
    let pack = match get_device(uuid) {
        Ok(d) => d,
        Err(e) => return Err(e),
    };
    let device = pack.device;
    ensure_device_open(&device).await?;

    let send_fun = Function::new_with_args("device, firmware", 
    JS_CODE);

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
                progress.borrow_mut().clear();
            });
            return Err(HidError::new(&format!("FAILED to send report: {:?}", err)));
        },
    };

    let res = match JsFuture::from(promise).await {
        Ok(success) => {
            if success.as_bool().unwrap_or(false) {
                Ok(firmware.len())
            } else {
                Err(HidError::new("FAILED to send report: failed to send firmware"))
            }
        },
        Err(err) => {
            log::debug!("FAILED to send report: {:?}", err);
            remove_device(uuid).await;
            Err(HidError::new(&format!("FAILED to send report: {:?}", err)))
        },
    };

    // Always clean up progress listener
    SEND_FIRMWARE_PROGRESS.with(|progress| {
        progress.borrow_mut().clear();
    });

    res
}

pub(in crate) async fn sub_report_arrive(uuid: u128, callback: SafeCallback2<u128, Vec<u8>, ()>) -> Result<(), HidError> {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        let mut binding = listeners.borrow_mut();
        match binding.get_mut(&uuid) {
            Some(list) => list.push(callback),
            None => {
                binding.insert(uuid, vec!(callback));
            }
        }
    });
    Ok(())
}

pub(in crate) async fn unsub_report_arrive(uuid: u128, callback: SafeCallback2<u128, Vec<u8>, ()>) -> Result<(), HidError> {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        match listeners.borrow_mut().get_mut(&uuid) {
            Some(list) => list.retain(|l| !l.ptr_eq(&callback)),
            None => (),
        }
    });
    Ok(())
}

////////////////////////////////////////
// Global variables
////////////////////////////////////////
thread_local! {
    static SEND_FIRMWARE_PROGRESS: RefCell<Vec<SafeCallback<f64, ()>>> = RefCell::new(Vec::new());
    static DEVICE_LIST: RefCell<HashMap<u128, HidDevicePackage>> = RefCell::new(HashMap::new());
    static DEVICE_CONNECTION_LISTENERS: RefCell<Vec<SafeCallback2<u128, bool, ()>>> = RefCell::new(Vec::new());
    static DEVICE_REPORT_LISTENERS: RefCell<HashMap<u128, Vec<SafeCallback2<u128, Vec<u8>, ()>>>> = RefCell::new(HashMap::new());
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
async fn notify_connection_changed(uuid: u128, connected: bool) {
    log::debug!("notify_connection_changed {:?}", connected);
    let listeners = DEVICE_CONNECTION_LISTENERS.with(|listeners| {
        log::debug!("listener count {:?}", listeners.borrow().len());
        listeners.borrow().clone()
    });
    for listener in listeners {
        log::debug!("notify listener");
        listener.call(uuid, connected).await;
        log::debug!("listener done");
    }
}

async fn notify_report_arrive(uuid: u128, report: Vec<u8>) {
    DEVICE_REPORT_LISTENERS.with(|listeners| {
        if let Some(list) = listeners.borrow().get(&uuid) {
            let listeners_clone = list.clone();
            // 在闭包外执行异步操作
            wasm_bindgen_futures::spawn_local(async move {
                for listener in listeners_clone {
                    listener.call(uuid, report.clone()).await;
                }
            });
        }
    });
}

////////////////////////////////////////
// JavaScript interfaces
////////////////////////////////////////
#[derive(Serialize, Deserialize, Debug, Default)]
pub(in crate) struct WebHidFilter {
    #[serde(rename = "vendorId")]
    pub vendor_id: Option<u16>,
    #[serde(rename = "productId")]
    pub product_id: Option<u16>,
    #[serde(rename = "usagePage")]
    pub usage_page: Option<u16>,
    pub usage: Option<u16>,
}

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
        let _ = add_device(device).await;
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
pub async fn on_device_report_arrived(event_js: JsValue) -> Promise {
    let event = match event_js.dyn_into::<HidInputReportEvent>() {
        Ok(e) => e,
        Err(_) => {
            log::debug!("FAILED to cast JsValue to HidInputReportEvent");
            return Promise::resolve(&JsValue::NULL);
        }
    };
    let device = event.device();
    let report_id = event.report_id();
    let data_view = event.data();
    let mut data: Vec<u8> = vec![0; data_view.byte_length() + 1];
    data[0] = report_id;
    
    for i in 0..data_view.byte_length() {
        data[i + 1] = data_view.get_uint8(i + data_view.byte_offset());
    }
    
    match find_device(&device) {
        Some(uuid) => notify_report_arrive(uuid, data).await,
        None => (),
    };
    Promise::resolve(&JsValue::NULL)
}

////////////////////////////////////////
// Internal functions
////////////////////////////////////////
pub(in crate) fn get_api() -> Result<web_sys::Hid, HidError> {
    let window = web_sys::window()
        .ok_or_else(|| HidError::new("Cannot get window"))?;
    Ok(window.navigator().hid())
}

fn get_device(uuid: u128) -> Result<HidDevicePackage, HidError> {
    DEVICE_LIST.with(|list| {
        match list.borrow().get(&uuid) {
            Some(dev) => Ok(dev.clone()),
            None => Err(HidError::new("FAILED to find device")),
        }
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
        },
        None => (),
    };

    future_delay(DEVICE_OPEN_DELAY_MS).await;
    let collections = get_collections_by_device(device.clone());

    let has_report_id = collections.output_reports.iter()
        .any(|r| SUPPORTED_REPORT_IDS.contains(&r.report_id));
    
    if !has_report_id {
        return Ok(None);
    }

    match JsFuture::from(device.open()).await {
        Ok(_) => log::debug!("open device done"),
        Err(err) => log::debug!("open device failed {:?}", err),
    }

    let on_report = Closure::wrap(Box::new(|event: JsValue| {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = on_device_report_arrived(event).await;
        });
    }) as Box<dyn Fn(JsValue)>);
    
    device.set_oninputreport(Some(on_report.as_ref().unchecked_ref()));
    on_report.forget();

    let uuid = Uuid::new_v4().as_u128();
    let report_info = collections.input_reports.iter()
        .map(|r| (r.report_id, r.clone()))
        .collect();
    DEVICE_LIST.with(|list| {
        list.borrow_mut().insert(uuid, HidDevicePackage {
            device: device.clone(),
            report_info,
            descriptor: collections,
        });
    });
    notify_connection_changed(uuid, true).await;
    Ok(Some(uuid))
}

async fn remove_device(uuid: u128) {
    notify_connection_changed(uuid, false).await;
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
        if let Some(info) = HidReportDescriptor::from_js_value(collection) {
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

use std::sync::Arc;
