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
use hid_report_descriptor::HidReportDescriptor;
use once_cell::sync::Lazy;
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

/// Snapshot the currently-connected HID devices.
pub fn device_list() -> Result<Vec<HidDevice>, HidError> {
    let devices = platform_hid::get_device_list().map_err(|e| {
        log::error!("device_list failed: {:?}", e);
        e
    })?;
    log::debug!("device_list success: {} devices", devices.len());
    Ok(devices.into_iter().map(HidDevice::from).collect())
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
    platform_hid::register_connection_listener(id, cb)?;
    Ok(Subscription::new(id, |id| {
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
        platform_hid::available(self.id.as_u128())
    }

    pub fn vid(&self) -> Result<u16, HidError> {
        platform_hid::vid(self.id.as_u128())
    }

    pub fn pid(&self) -> Result<u16, HidError> {
        platform_hid::pid(self.id.as_u128())
    }

    pub fn get_serial_number(&self) -> Result<Option<String>, HidError> {
        platform_hid::get_serial_number(self.id.as_u128())
    }

    pub fn get_product_name(&self) -> Result<Option<String>, HidError> {
        platform_hid::get_product_name(self.id.as_u128())
    }

    pub fn get_collections(&self) -> Result<HidReportDescriptor, HidError> {
        platform_hid::get_collections(self.id.as_u128())
    }

    pub async fn send_report(&self, data: Vec<u8>) -> Result<(), HidError> {
        platform_hid::send_report(self.id.as_u128(), data)
            .await
            .map(|_| ())
    }

    pub async fn send_report_slice(&self, data: &[u8]) -> Result<(), HidError> {
        platform_hid::send_report(self.id.as_u128(), data.to_vec())
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
        platform_hid::register_report_listener(uuid, id, cb)?;
        Ok(Subscription::new(id, move |id| {
            if let Err(e) = platform_hid::unregister_report_listener(uuid, id) {
                log::debug!("unregister_report_listener failed: {:?}", e);
            }
        }))
    }

    pub fn has_report_id(&self, report_id: u8) -> Result<bool, HidError> {
        platform_hid::has_report_id(self.id.as_u128(), report_id)
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
