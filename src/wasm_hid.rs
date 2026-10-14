//! wasm32 backend dispatcher.
//!
//! A single wasm build serves two hosts:
//! - a browser with WebHID (`web_hid`), and
//! - an Android WebView whose host injects `globalThis.__sayoHidBridge`
//!   (`bridge_hid`), since WebView has no WebHID.
//!
//! The backend is detected at `init()` and pinned until `shutdown()`. Before
//! `init()`, calls are routed to whichever backend is detectable right now.

use crate::{
    bridge_hid, hid_error::HidError, hid_report_descriptor::HidReportDescriptor, web_hid,
    ConnectionCallback, ProgressCallback, ReportCallback, SubscriptionId,
};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Web,
    Bridge,
}

thread_local! {
    static SELECTED: Cell<Option<Backend>> = const { Cell::new(None) };
}

fn detect() -> Backend {
    if bridge_hid::is_supported() {
        Backend::Bridge
    } else {
        Backend::Web
    }
}

fn backend() -> Backend {
    SELECTED.with(Cell::get).unwrap_or_else(detect)
}

macro_rules! dispatch {
    ($name:ident($($arg:expr),*)) => {
        match backend() {
            Backend::Web => web_hid::$name($($arg),*),
            Backend::Bridge => bridge_hid::$name($($arg),*),
        }
    };
    (async $name:ident($($arg:expr),*)) => {
        match backend() {
            Backend::Web => web_hid::$name($($arg),*).await,
            Backend::Bridge => bridge_hid::$name($($arg),*).await,
        }
    };
}

pub(crate) fn is_supported() -> bool {
    bridge_hid::is_supported() || web_hid::is_supported()
}

pub(crate) async fn init() -> Result<(), HidError> {
    let selected = detect();
    log::debug!("wasm HID backend: {selected:?}");
    SELECTED.with(|slot| slot.set(Some(selected)));
    let result = dispatch!(async init());
    if result.is_err() {
        SELECTED.with(|slot| slot.set(None));
    }
    result
}

pub(crate) fn shutdown() -> Result<(), HidError> {
    let result = dispatch!(shutdown());
    SELECTED.with(|slot| slot.set(None));
    result
}

/// Filters are stored by both backends so the value survives regardless of
/// whether it is set before or after the backend is pinned.
pub(crate) fn set_device_filters(vendor_ids: Vec<(u16, Option<u16>)>) {
    web_hid::set_device_filters(vendor_ids.clone());
    bridge_hid::set_device_filters(vendor_ids);
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
    dispatch!(async request_device(vendor_ids))
}

pub(crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    dispatch!(get_device_list())
}

pub(crate) fn register_connection_listener(
    id: SubscriptionId,
    callback: ConnectionCallback,
) -> Result<(), HidError> {
    dispatch!(register_connection_listener(id, callback))
}

pub(crate) fn unregister_connection_listener(id: SubscriptionId) -> Result<(), HidError> {
    dispatch!(unregister_connection_listener(id))
}

pub(crate) fn available(uuid: u128) -> bool {
    dispatch!(available(uuid))
}

pub(crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    dispatch!(vid(uuid))
}

pub(crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    dispatch!(pid(uuid))
}

pub(crate) fn get_serial_number(uuid: u128) -> Result<Option<String>, HidError> {
    dispatch!(get_serial_number(uuid))
}

pub(crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    dispatch!(get_product_name(uuid))
}

pub(crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    dispatch!(get_collections(uuid))
}

pub(crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    dispatch!(has_report_id(uuid, report_id))
}

pub(crate) async fn send_report(uuid: u128, data: Vec<u8>) -> Result<usize, HidError> {
    dispatch!(async send_report(uuid, data))
}

#[allow(clippy::too_many_arguments)]
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
    match backend() {
        Backend::Web => {
            web_hid::send_firmware(
                uuid,
                firmware,
                write_data_cmd,
                size_addr,
                big_endian,
                err_for_size,
                encrypt,
                check_sum,
                on_progress,
            )
            .await
        }
        Backend::Bridge => Err(HidError::NotImplemented),
    }
}

pub(crate) fn register_report_listener(
    uuid: u128,
    id: SubscriptionId,
    callback: ReportCallback,
) -> Result<(), HidError> {
    dispatch!(register_report_listener(uuid, id, callback))
}

pub(crate) fn unregister_report_listener(uuid: u128, id: SubscriptionId) -> Result<(), HidError> {
    dispatch!(unregister_report_listener(uuid, id))
}
