#[cfg(target_arch = "wasm32")]
#[path = "web_hid.rs"]
mod platform_hid;

#[cfg(all(not(target_arch = "wasm32"), target_os = "android"))]
#[path = "android_hid.rs"]
mod platform_hid;

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
#[path = "os_hid.rs"]
mod platform_hid;

pub mod hid_error;
pub mod hid_report_descriptor;
pub mod logger;

use hid_error::HidError;
use hid_report_descriptor::{HidReportDescriptor, HidReportInfo};
use once_cell::sync::Lazy;
use sayo_dummy_device::DummyDevice;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

static HID_INITIALIZED: AtomicBool = AtomicBool::new(false);

////////////////////////////////////////////////////////////////////////////////
// Device filter (cross-platform policy hook)
////////////////////////////////////////////////////////////////////////////////

/// Predicate used to decide whether a discovered HID interface should be
/// tracked. Receives the parsed report descriptor of the candidate.
pub type DeviceFilter = Arc<dyn Fn(&HidReportDescriptor) -> bool + Send + Sync + 'static>;

static DEVICE_FILTER: Lazy<RwLock<Option<DeviceFilter>>> = Lazy::new(|| RwLock::new(None));

/// Install a global device-filter predicate. The platform backend will only
/// track HID interfaces for which the predicate returns `true`. Replaces any
/// previously installed filter. When no filter is set, every HID interface
/// matching the VID/PID set passed to [`request_device`] is tracked.
pub fn set_device_filter<F>(filter: F)
where
    F: Fn(&HidReportDescriptor) -> bool + Send + Sync + 'static,
{
    if let Ok(mut slot) = DEVICE_FILTER.write() {
        *slot = Some(Arc::new(filter));
    }
}

/// Remove any installed device filter (revert to tracking all VID/PID-matched
/// HID interfaces).
pub fn clear_device_filter() {
    if let Ok(mut slot) = DEVICE_FILTER.write() {
        *slot = None;
    }
}

/// Internal: evaluate the installed filter on a descriptor. Returns `true`
/// when no filter is set so backends accept everything by default.
#[allow(dead_code)] // currently only consulted by the desktop backend
pub(crate) fn evaluate_device_filter(desc: &HidReportDescriptor) -> bool {
    match DEVICE_FILTER.read() {
        Ok(slot) => slot.as_ref().is_none_or(|f| f(desc)),
        Err(_) => true,
    }
}

////////////////////////////////////////////////////////////////////////////////
// Internal queue-depth bookkeeping helpers
////////////////////////////////////////////////////////////////////////////////

/// Increment a queue-depth counter before publishing work to another thread.
///
/// Callers should roll the increment back with [`decrement_queue_depth`] if the
/// enqueue operation fails.
pub(crate) fn increment_queue_depth(depth: &AtomicUsize) -> usize {
    depth.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

/// Decrement a queue-depth counter without allowing `usize` underflow.
/// Returns the new depth after the decrement (or `0` if it was already zero).
pub(crate) fn decrement_queue_depth(depth: &AtomicUsize) -> usize {
    let mut current = depth.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return 0;
        }

        match depth.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return current - 1,
            Err(actual) => current = actual,
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// DeviceId
////////////////////////////////////////////////////////////////////////////////

/// Opaque, platform-stable identifier for a HID device.
///
/// Backed by a `u128` that is unique for the lifetime of a connected device.
/// The same physical device may receive a different `DeviceId` after a
/// disconnect/reconnect cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct DeviceId(u128);

impl DeviceId {
    #[inline]
    pub const fn new(raw: u128) -> Self {
        DeviceId(raw)
    }
    #[inline]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl From<u128> for DeviceId {
    #[inline]
    fn from(v: u128) -> Self {
        DeviceId(v)
    }
}
impl From<DeviceId> for u128 {
    #[inline]
    fn from(v: DeviceId) -> u128 {
        v.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

////////////////////////////////////////////////////////////////////////////////
// Callback / subscription types
////////////////////////////////////////////////////////////////////////////////

/// Callback fired when a HID device connects (`true`) or disconnects (`false`).
pub type ConnectionCallback = Arc<dyn Fn(DeviceId, bool) + Send + Sync + 'static>;

/// Callback fired when an input report arrives. The payload is shared via
/// [`Arc`] so multiple listeners can observe the same buffer without copying.
pub type ReportCallback = Arc<dyn Fn(DeviceId, Arc<[u8]>) + Send + Sync + 'static>;

/// One-shot progress callback used by [`HidDevice::send_firmware`].
pub type ProgressCallback = Arc<dyn Fn(f64) + Send + Sync + 'static>;

/// Opaque identifier used by platform backends to look up a registered
/// listener for removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(u64);

impl SubscriptionId {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Handle returned by every `on_*` subscription. Dropping the handle (or
/// calling [`Subscription::unsubscribe`]) cancels the subscription. Call
/// [`Subscription::detach`] to keep the listener alive for the lifetime of
/// the process.
#[must_use = "dropping the Subscription cancels the listener; call .detach() to keep it"]
pub struct Subscription {
    id: SubscriptionId,
    cancel: Option<Box<dyn FnOnce(SubscriptionId) + Send + 'static>>,
}

impl Subscription {
    fn new<F>(id: SubscriptionId, cancel: F) -> Self
    where
        F: FnOnce(SubscriptionId) + Send + 'static,
    {
        Self {
            id,
            cancel: Some(Box::new(cancel)),
        }
    }

    pub fn id(&self) -> SubscriptionId {
        self.id
    }

    /// Explicitly cancel the subscription. Equivalent to dropping the handle.
    pub fn unsubscribe(mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel(self.id);
        }
    }

    /// Leak the subscription handle so the listener stays installed for the
    /// remainder of the program.
    pub fn detach(mut self) -> SubscriptionId {
        self.cancel = None;
        self.id
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel(self.id);
        }
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("id", &self.id)
            .finish()
    }
}

////////////////////////////////////////////////////////////////////////////////
// Top-level HID API (free functions)
////////////////////////////////////////////////////////////////////////////////

/// Initialize the HID backend. Idempotent.
pub async fn init() -> Result<(), HidError> {
    if HID_INITIALIZED.load(Ordering::Acquire) {
        log::debug!("hid_api already initialized, skipping");
        return Ok(());
    }

    logger::init();
    match platform_hid::init().await {
        Ok(_) => {
            HID_INITIALIZED.store(true, Ordering::Release);
            log::debug!("hid_api init success");
            Ok(())
        }
        Err(e) => {
            log::debug!("hid_api init failed: {:?}", e);
            Err(e)
        }
    }
}

/// Whether HID is supported on the current platform / runtime.
pub fn is_supported() -> bool {
    platform_hid::is_supported()
}

/// Stop the platform backend's background polling/event loop and release any
/// associated thread(s). Idempotent. After this returns, [`init`] must be
/// called again before live device events resume.
pub fn shutdown() -> Result<(), HidError> {
    let was_initialized = HID_INITIALIZED.swap(false, Ordering::AcqRel);
    if !was_initialized {
        return Ok(());
    }
    platform_hid::shutdown()
}

/// Prompt the user (where applicable) to grant access to devices matching
/// any of the given `(vid, Option<pid>)` filters. Returns the device IDs
/// that were granted.
pub async fn request_device(vpid: Vec<(u16, Option<u16>)>) -> Result<Vec<u128>, HidError> {
    match platform_hid::request_device(vpid).await {
        Ok(ids) => {
            log::debug!("request_device success: {:?}", ids);
            Ok(ids)
        }
        Err(e) => {
            log::error!("request_device failed: {:?}", e);
            Err(e)
        }
    }
}

/// Snapshot the currently-connected HID devices, including any registered
/// dummy/simulated devices so they surface through the normal enumeration.
pub fn device_list() -> Result<Vec<HidDevice>, HidError> {
    let devices = platform_hid::get_device_list().map_err(|e| {
        log::error!("device_list failed: {:?}", e);
        e
    })?;
    log::debug!("device_list success: {} devices", devices.len());
    let mut list: Vec<HidDevice> = devices.into_iter().map(HidDevice::from).collect();
    if let Ok(dummies) = DUMMY_DEVICES.read() {
        for &uuid in dummies.keys() {
            list.push(HidDevice::from(uuid));
        }
    }
    Ok(list)
}

/// Subscribe to device connection / disconnection events. The callback is
/// invoked immediately for every currently-connected device with
/// `connected = true` so callers don't need a separate enumeration step.
///
/// Drop the returned [`Subscription`] to unsubscribe.
pub fn on_connection_changed<F>(callback: F) -> Result<Subscription, HidError>
where
    F: Fn(DeviceId, bool) + Send + Sync + 'static,
{
    let id = SubscriptionId::next();
    let cb: ConnectionCallback = Arc::new(callback);
    platform_hid::register_connection_listener(id, cb.clone())?;
    // Mirror platform behavior for dummies: remember the listener (so future
    // register/unregister_dummy fire it) and immediately report every
    // already-registered dummy as connected.
    if let Ok(mut map) = DUMMY_CONN_LISTENERS.write() {
        map.insert(id, cb.clone());
    }
    if let Ok(dummies) = DUMMY_DEVICES.read() {
        for &uuid in dummies.keys() {
            cb(DeviceId(uuid), true);
        }
    }
    Ok(Subscription::new(id, move |id| {
        if let Ok(mut map) = DUMMY_CONN_LISTENERS.write() {
            map.remove(&id);
        }
        if let Err(e) = platform_hid::unregister_connection_listener(id) {
            log::debug!("unregister_connection_listener failed: {:?}", e);
        }
    }))
}

////////////////////////////////////////////////////////////////////////////////
// HidDevice
////////////////////////////////////////////////////////////////////////////////

pub struct HidDevice {
    pub id: DeviceId,
}

impl PartialEq for HidDevice {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl From<DeviceId> for HidDevice {
    fn from(id: DeviceId) -> Self {
        HidDevice { id }
    }
}

impl From<u128> for HidDevice {
    fn from(raw: u128) -> Self {
        HidDevice {
            id: DeviceId::new(raw),
        }
    }
}

impl From<HidDevice> for DeviceId {
    fn from(device: HidDevice) -> DeviceId {
        device.id
    }
}

impl From<HidDevice> for u128 {
    fn from(device: HidDevice) -> u128 {
        device.id.as_u128()
    }
}

impl HidDevice {
    pub fn new(id: impl Into<DeviceId>) -> Self {
        HidDevice { id: id.into() }
    }

    pub fn available(&self) -> bool {
        let uuid = self.id.as_u128();
        if is_dummy(uuid) {
            return true;
        }
        platform_hid::available(uuid)
    }

    pub fn vid(&self) -> Result<u16, HidError> {
        let uuid = self.id.as_u128();
        if let Some(v) = dummy_read(uuid, |d| d.vid()) {
            return Ok(v);
        }
        platform_hid::vid(uuid)
    }

    pub fn pid(&self) -> Result<u16, HidError> {
        let uuid = self.id.as_u128();
        if let Some(v) = dummy_read(uuid, |d| d.pid()) {
            return Ok(v);
        }
        platform_hid::pid(uuid)
    }

    pub fn get_serial_number(&self) -> Result<Option<String>, HidError> {
        let uuid = self.id.as_u128();
        if let Some(s) = dummy_read(uuid, |d| d.serial_number()) {
            return Ok(s);
        }
        platform_hid::get_serial_number(uuid)
    }

    pub fn get_product_name(&self) -> Result<Option<String>, HidError> {
        let uuid = self.id.as_u128();
        if let Some(s) = dummy_read(uuid, |d| d.product_name()) {
            return Ok(s);
        }
        platform_hid::get_product_name(uuid)
    }

    pub fn get_collections(&self) -> Result<HidReportDescriptor, HidError> {
        let uuid = self.id.as_u128();
        if let Some(desc) = dummy_read(uuid, |d| dummy_descriptor(d.meta())) {
            return Ok(desc);
        }
        platform_hid::get_collections(uuid)
    }

    pub async fn send_report(&self, data: Vec<u8>) -> Result<(), HidError> {
        let uuid = self.id.as_u128();
        if is_dummy(uuid) {
            dummy_handle_report(uuid, &data);
            return Ok(());
        }
        platform_hid::send_report(uuid, data).await.map(|_| ())
    }

    pub async fn send_report_slice(&self, data: &[u8]) -> Result<(), HidError> {
        let uuid = self.id.as_u128();
        if is_dummy(uuid) {
            dummy_handle_report(uuid, data);
            return Ok(());
        }
        platform_hid::send_report(uuid, data.to_vec())
            .await
            .map(|_| ())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_firmware<F>(
        &self,
        firmware: Vec<u8>,
        write_data_cmd: u8,
        size_addr: u8,
        big_endian: u8,
        err_for_size: u8,
        encrypt: u8,
        check_sum: u8,
        on_progress: F,
    ) -> Result<usize, HidError>
    where
        F: Fn(f64) + Send + Sync + 'static,
    {
        let uuid = self.id.as_u128();
        if is_dummy(uuid) {
            // The simulated device has no firmware path; report full progress.
            on_progress(1.0);
            return Ok(firmware.len());
        }
        let mut buffer = firmware;
        let cb: ProgressCallback = Arc::new(on_progress);
        platform_hid::send_firmware(
            self.id.as_u128(),
            &mut buffer,
            write_data_cmd,
            size_addr,
            big_endian,
            err_for_size,
            encrypt,
            check_sum,
            cb,
        )
        .await
    }

    /// Subscribe to input reports from this device. The callback receives the
    /// shared [`Arc<[u8]>`] payload so multiple listeners can observe the
    /// same buffer without copying.
    ///
    /// Drop the returned [`Subscription`] to unsubscribe.
    pub fn on_report<F>(&self, callback: F) -> Result<Subscription, HidError>
    where
        F: Fn(DeviceId, Arc<[u8]>) + Send + Sync + 'static,
    {
        let id = SubscriptionId::next();
        let uuid = self.id.as_u128();
        let cb: ReportCallback = Arc::new(callback);
        platform_hid::register_report_listener(uuid, id, cb.clone())?;
        // Always mirror the listener into the dummy table so a device that
        // becomes a dummy AFTER this subscription is installed (open-then-enable)
        // still receives the simulated responses. Harmless for real devices.
        if let Ok(mut map) = DUMMY_REPORT_LISTENERS.write() {
            map.entry(uuid).or_default().insert(id, cb);
        }
        Ok(Subscription::new(id, move |id| {
            if let Ok(mut map) = DUMMY_REPORT_LISTENERS.write() {
                if let Some(set) = map.get_mut(&uuid) {
                    set.remove(&id);
                    if set.is_empty() {
                        map.remove(&uuid);
                    }
                }
            }
            if let Err(e) = platform_hid::unregister_report_listener(uuid, id) {
                log::debug!("unregister_report_listener failed: {:?}", e);
            }
        }))
    }

    pub fn has_report_id(&self, report_id: u8) -> Result<bool, HidError> {
        let uuid = self.id.as_u128();
        if let Some(b) = dummy_read(uuid, |d| d.has_report_id(report_id)) {
            return Ok(b);
        }
        platform_hid::has_report_id(uuid, report_id)
    }
}

////////////////////////////////////////////////////////////////////////////////
// Dummy / simulated devices
////////////////////////////////////////////////////////////////////////////////
//
// A dummy device is simulated entirely in this cross-platform layer: each public
// seam above checks the dummy registry first and, on a hit, serves the request
// from a `sayo_dummy_device::DummyDevice` instead of delegating to
// `platform_hid`. This keeps the simulation identical on every backend
// (node/web/ipc) and lets the device flow through the normal stack
// (device_list / openDeviceByUuid / connection events / initialize) with no
// special-casing above hid_rs.

/// UUIDs `< RESERVED_UUID_MAX` are reserved for dummy devices (id == model_code,
/// which is a u16). Real device uuids are minted via [`get_uuid`] strictly
/// outside this range, so [`is_dummy`] can never misclassify a real device.
pub const RESERVED_UUID_MAX: u128 = 0x1_0000;

static DUMMY_DEVICES: Lazy<RwLock<HashMap<u128, DummyDevice>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
/// Report listeners for dummy uuids. Populated by EVERY `on_report` (not only
/// dummies) so a device that becomes a dummy AFTER its report subscription is
/// installed still receives responses — see `on_report`.
static DUMMY_REPORT_LISTENERS: Lazy<RwLock<HashMap<u128, HashMap<SubscriptionId, ReportCallback>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static DUMMY_CONN_LISTENERS: Lazy<RwLock<HashMap<SubscriptionId, ConnectionCallback>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Mint a uuid for a real device, excluding the reserved dummy range. Platform
/// backends call this instead of `Uuid::new_v4()` directly.
pub(crate) fn get_uuid() -> u128 {
    loop {
        let id = uuid::Uuid::new_v4().as_u128();
        if id >= RESERVED_UUID_MAX {
            return id;
        }
    }
}

fn is_dummy(uuid: u128) -> bool {
    DUMMY_DEVICES
        .read()
        .map(|m| m.contains_key(&uuid))
        .unwrap_or(false)
}

/// Read-only access to a registered dummy, applying `f` while the registry lock
/// is held. Returns `None` if no dummy is registered for `uuid`.
fn dummy_read<T>(uuid: u128, f: impl FnOnce(&DummyDevice) -> T) -> Option<T> {
    DUMMY_DEVICES.read().ok().and_then(|m| m.get(&uuid).map(f))
}

fn dummy_descriptor(meta: &sayo_dummy_device::DeviceMeta) -> HidReportDescriptor {
    fn map(v: &[sayo_dummy_device::ReportInfoMeta]) -> Vec<HidReportInfo> {
        v.iter()
            .map(|r| HidReportInfo {
                report_id: r.report_id,
                size: r.size,
                usages: r.usages.clone(),
            })
            .collect()
    }
    HidReportDescriptor {
        input_reports: map(&meta.input_reports),
        output_reports: map(&meta.output_reports),
        feature_reports: map(&meta.feature_reports),
    }
}

/// Feed an outgoing request to the dummy engine and deliver its response reports
/// back through the dummy report listeners (the same path real input reports
/// take). The registry write lock is released before listeners fire.
fn dummy_handle_report(uuid: u128, data: &[u8]) {
    let responses = {
        let Ok(mut map) = DUMMY_DEVICES.write() else {
            return;
        };
        let Some(dev) = map.get_mut(&uuid) else {
            return;
        };
        dev.handle_report(data)
    };
    for r in responses {
        notify_dummy_report(uuid, r);
    }
}

fn notify_dummy_report(uuid: u128, report: Vec<u8>) {
    let listeners: Vec<ReportCallback> = match DUMMY_REPORT_LISTENERS.read() {
        Ok(map) => match map.get(&uuid) {
            Some(set) => set.values().cloned().collect(),
            None => return,
        },
        Err(_) => return,
    };
    let shared: Arc<[u8]> = Arc::from(report);
    for cb in listeners {
        cb(DeviceId(uuid), shared.clone());
    }
}

fn notify_dummy_connection(uuid: u128, connected: bool) {
    let listeners: Vec<ConnectionCallback> = match DUMMY_CONN_LISTENERS.read() {
        Ok(map) => map.values().cloned().collect(),
        Err(_) => return,
    };
    for cb in listeners {
        cb(DeviceId(uuid), connected);
    }
}

/// Register a simulated device from a fixture JSON. Returns its uuid
/// (== `model_code`) and fires connection listeners so it surfaces like a
/// freshly-plugged device. Safe to call from any backend.
pub fn register_dummy(fixture_json: &str) -> Result<u128, HidError> {
    let dummy = DummyDevice::from_fixture_json(fixture_json).map_err(HidError::Other)?;
    let uuid = dummy.uuid();
    {
        let mut map = DUMMY_DEVICES
            .write()
            .map_err(|_| HidError::lock_poisoned("DUMMY_DEVICES"))?;
        map.insert(uuid, dummy);
    }
    notify_dummy_connection(uuid, true);
    Ok(uuid)
}

/// Remove a previously-registered dummy and fire `connected = false`.
pub fn unregister_dummy(uuid: u128) {
    let removed = DUMMY_DEVICES
        .write()
        .ok()
        .map(|mut m| m.remove(&uuid).is_some())
        .unwrap_or(false);
    if removed {
        notify_dummy_connection(uuid, false);
    }
}

#[cfg(test)]
mod tests {
    use super::{decrement_queue_depth, increment_queue_depth};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn queue_depth_increment_and_decrement_round_trip() {
        let depth = AtomicUsize::new(0);

        assert_eq!(increment_queue_depth(&depth), 1);
        assert_eq!(depth.load(Ordering::Relaxed), 1);

        assert_eq!(decrement_queue_depth(&depth), 0);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_depth_decrement_at_zero_does_not_underflow() {
        let depth = AtomicUsize::new(0);

        assert_eq!(decrement_queue_depth(&depth), 0);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_depth_rollback_restores_previous_value() {
        let depth = AtomicUsize::new(0);

        assert_eq!(increment_queue_depth(&depth), 1);
        assert_eq!(decrement_queue_depth(&depth), 0);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
    }
}
