use crate::hid_error::HidError;
use crate::hid_report_descriptor::{HidReportDescriptor, HidReportInfo};
use crate::{ConnectionCallback, DeviceId, ProgressCallback, ReportCallback, SubscriptionId};
use hidapi::{DeviceInfo, HidApi, HidDevice};
use hidreport::ReportDescriptor;
use once_cell::sync::Lazy;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{sync_channel, SyncSender, TrySendError},
        Arc, Mutex, RwLock,
    },
    thread::sleep,
};

////////////////////////////////////////
// Constants
////////////////////////////////////////

/// Per-device bounded report queue capacity (drop newest on overflow).
const REPORT_QUEUE_CAP: usize = 4096;
/// Edge-triggered warning threshold for backlog depth. Once queue depth
/// crosses this we log once; the warning re-arms after depth drops back
/// below the threshold.
const REPORT_QUEUE_WARN_THRESHOLD: usize = 256;
const DEVICE_POLL_INTERVAL_MS: u64 = 1000;
const READ_RETRY_DELAY_MS: u64 = 500;
const READ_BUFFER_SIZE: usize = 1024;
const REPORT_DESCRIPTOR_BUFFER_SIZE: usize = 1024;

type VidPidFilters = Vec<(u16, Option<u16>)>;

////////////////////////////////////////
// Interfaces
////////////////////////////////////////

pub(crate) fn is_supported() -> bool {
    true
}

pub(crate) fn available(uuid: u128) -> bool {
    match DEVICE_LIST.read() {
        Ok(binding) => binding.contains_key(&uuid),
        Err(err) => {
            log::debug!("Failed to acquire device list lock: {:?}", err);
            false
        }
    }
}

pub(crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    get_device_by_uuid(uuid, |device| {
        device
            .get_device_info()
            .map(|info| info.vendor_id())
            .map_err(|_| HidError::new("Failed to get vendor id"))
    })
}

pub(crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    get_device_by_uuid(uuid, |device| {
        device
            .get_device_info()
            .map(|info| info.product_id())
            .map_err(|_| HidError::new("Failed to get product id"))
    })
}

pub(crate) fn get_serial_number(uuid: u128) -> Result<Option<String>, HidError> {
    get_device_by_uuid(uuid, |device| {
        device.get_device_info()
            .map(|info| info.serial_number().map(|s| s.to_string()))
            .map_err(|_| HidError::new("Failed to get serial number"))
    })
}

pub(crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    get_device_by_uuid(uuid, |device| {
        device
            .get_product_string()
            .map_err(|_| HidError::new("Failed to get product name"))
    })
}

pub(crate) async fn init() -> Result<(), HidError> {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        log::debug!("os_hid::init called more than once; ignoring");
        return Ok(());
    }
    SHUTDOWN.store(false, Ordering::SeqCst);

    let api = create_api()?;
    HIDAPI
        .lock()
        .map_err(|_| HidError::lock_poisoned("HIDAPI"))?
        .replace(api);

    fn poll_devices() -> Result<(), HidError> {
        let vpids = VID_PID_LIST
            .lock()
            .map_err(|_| HidError::lock_poisoned("VID_PID_LIST"))?;
        let converted_vpids: Vec<(u16, u16)> = vpids
            .iter()
            .map(|(vid, pid)| (*vid, pid.unwrap_or(0)))
            .collect();
        drop(vpids);
        update_device_list(converted_vpids)
    }

    poll_devices()?;
    let handle = std::thread::spawn(move || {
        while !SHUTDOWN.load(Ordering::Relaxed) {
            if let Err(e) = poll_devices() {
                log::debug!("Device polling error: {:?}", e);
            }
            // Sleep in small slices so shutdown is responsive.
            let mut remaining = DEVICE_POLL_INTERVAL_MS;
            while remaining > 0 && !SHUTDOWN.load(Ordering::Relaxed) {
                let step = remaining.min(50);
                sleep(std::time::Duration::from_millis(step));
                remaining -= step;
            }
        }
        log::debug!("os_hid poll thread exiting");
    });
    if let Ok(mut slot) = POLL_THREAD.lock() {
        *slot = Some(handle);
    }
    Ok(())
}

/// Signal the device-polling thread to exit and wait for it to finish.
/// After calling this, [`init`] must be called again before any other
/// device-listing APIs will produce live results.
pub(crate) fn shutdown() -> Result<(), HidError> {
    SHUTDOWN.store(true, Ordering::SeqCst);
    let handle = POLL_THREAD
        .lock()
        .map_err(|_| HidError::lock_poisoned("POLL_THREAD"))?
        .take();
    if let Some(h) = handle {
        // Join can fail only if the thread panicked; either way we proceed.
        if let Err(e) = h.join() {
            log::warn!("poll thread join failed: {e:?}");
        }
    }
    INIT_DONE.store(false, Ordering::SeqCst);
    Ok(())
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
    let mut vid_pid_list = VID_PID_LIST
        .lock()
        .map_err(|_| HidError::lock_poisoned("VID_PID_LIST"))?;
    vid_pid_list.clear();
    vid_pid_list.extend(vendor_ids.iter().copied());
    let converted_vpids = normalize_vendor_ids(&vid_pid_list);
    drop(vid_pid_list);

    update_device_list(converted_vpids)?;
    get_device_list()
}

pub(crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let device_list_binding = DEVICE_LIST
        .read()
        .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
    let list: Vec<u128> = device_list_binding.keys().cloned().collect();
    Ok(list)
}

pub(crate) fn register_connection_listener(
    id: SubscriptionId,
    callback: ConnectionCallback,
) -> Result<(), HidError> {
    // Snapshot existing devices without holding any locks across the callback
    // invocation, otherwise the user closure could deadlock on the same locks.
    let existing: Vec<u128> = {
        let device_list_binding = DEVICE_LIST
            .read()
            .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
        device_list_binding.keys().copied().collect()
    };

    {
        let mut listeners = DEVICE_CONNECTION_LISTENERS
            .write()
            .map_err(|_| HidError::lock_poisoned("DEVICE_CONNECTION_LISTENERS"))?;
        listeners.insert(id, callback.clone());
    }

    for uuid in existing {
        callback(DeviceId(uuid), true);
    }
    Ok(())
}

pub(crate) fn unregister_connection_listener(id: SubscriptionId) -> Result<(), HidError> {
    let mut listeners = DEVICE_CONNECTION_LISTENERS
        .write()
        .map_err(|_| HidError::lock_poisoned("DEVICE_CONNECTION_LISTENERS"))?;
    listeners.remove(&id);
    Ok(())
}

pub(crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    let device_pack = snapshot_device(uuid)?;

    let descriptor = device_pack
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?
        .descriptors
        .clone();

    Ok(descriptor)
}

pub(crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let device_pack = snapshot_device(uuid)?;

    let pack = device_pack
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?;
    Ok(pack.report_info.contains_key(&report_id))
}

pub(crate) async fn send_report(uuid: u128, mut data: Vec<u8>) -> Result<usize, HidError> {
    if data.is_empty() {
        return Err(HidError::EmptyData);
    }

    let report_id = data[0];
    let device_pack = snapshot_device(uuid)?;

    let (device, size) = {
        let pack = device_pack
            .lock()
            .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?;

        let device = pack
            .devices
            .get(&report_id)
            .ok_or(HidError::ReportIdMissing { uuid, report_id })?
            .clone();

        let size = pack
            .report_info
            .get(&report_id)
            .ok_or(HidError::ReportIdMissing { uuid, report_id })?
            .size;

        (device, size)
    };

    if data.len() > size {
        return Err(HidError::DataTooLarge {
            max: size,
            got: data.len(),
        });
    }

    drop(device_pack);

    let device_guard = device
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevice"))?;

    // Pad data to required size
    data.resize(size, 0);

    // println!("Sending report: {:02X?}", data);
    let res = match device_guard.write(&data) {
        Ok(written_size) => Ok(written_size),
        Err(err) => {
            log::debug!("Failed to write report: {:?}", err);
            drop(device_guard);
            if let Err(e) = remove_device(uuid) {
                log::warn!("failed to remove device {uuid:032x} after write error: {e}");
            }
            Err(HidError::Io(format!("failed to write report: {err:?}")))
        }
    };
    res
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_firmware(
    _uuid: u128,
    _firmware: &mut Vec<u8>,
    _write_data_cmd: u8,
    _size_addr: u8,
    _big_endian: u8,
    _err_for_size: u8,
    _encrypt: u8,
    _check_sum: u8,
    _on_progress: ProgressCallback,
) -> Result<usize, HidError> {
    log::debug!("Send firmware intended for quick firmware transfer on web. This is not supposed to be called on desktop");
    Ok(0)
}

pub(crate) fn register_report_listener(
    uuid: u128,
    id: SubscriptionId,
    callback: ReportCallback,
) -> Result<(), HidError> {
    let mut listeners_binding = DEVICE_REPORT_LISTENERS
        .write()
        .map_err(|_| HidError::lock_poisoned("DEVICE_REPORT_LISTENERS"))?;
    listeners_binding
        .entry(uuid)
        .or_default()
        .insert(id, callback);
    Ok(())
}

pub(crate) fn unregister_report_listener(uuid: u128, id: SubscriptionId) -> Result<(), HidError> {
    let mut listeners_binding = DEVICE_REPORT_LISTENERS
        .write()
        .map_err(|_| HidError::lock_poisoned("DEVICE_REPORT_LISTENERS"))?;
    if let Some(map) = listeners_binding.get_mut(&uuid) {
        map.remove(&id);
        if map.is_empty() {
            listeners_binding.remove(&uuid);
        }
    }
    Ok(())
}

////////////////////////////////////////
// Global variables
////////////////////////////////////////
static HIDAPI: Lazy<Mutex<Option<HidApi>>> = Lazy::new(|| Mutex::new(None));
static VID_PID_LIST: Lazy<Mutex<VidPidFilters>> = Lazy::new(|| Mutex::new(vec![(0x8089, None)]));
static DEVICE_LIST: Lazy<RwLock<HashMap<u128, Arc<Mutex<HidDevicePackage>>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static SERIAL_NUMBER_TO_UUID: Lazy<RwLock<HashMap<String, u128>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static DEVICE_CONNECTION_LISTENERS: Lazy<RwLock<HashMap<SubscriptionId, ConnectionCallback>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static DEVICE_REPORT_LISTENERS: Lazy<
    RwLock<HashMap<u128, HashMap<SubscriptionId, ReportCallback>>>,
> = Lazy::new(|| RwLock::new(HashMap::new()));

// Polling-thread lifecycle.
static INIT_DONE: AtomicBool = AtomicBool::new(false);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static POLL_THREAD: Lazy<Mutex<Option<std::thread::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));

////////////////////////////////////////
// Internal structures
////////////////////////////////////////
#[derive(Debug)]
struct HidDevicePackage {
    uuid: u128,
    serial_number: String,
    paths: HashSet<String>,
    devices: HashMap<u8, Arc<Mutex<HidDevice>>>,
    descriptors: HidReportDescriptor,
    report_info: HashMap<u8, HidReportInfo>,
    abort: Arc<AtomicBool>,
    /// Master sender for the per-device report channel. Cloned by each reader
    /// thread; `abort()` drops this slot so the dispatcher exits once all
    /// reader-side clones are dropped.
    report_tx: Mutex<Option<SyncSender<Arc<[u8]>>>>,
    /// Current number of unprocessed reports in the channel.
    queue_depth: Arc<AtomicUsize>,
    /// Edge-trigger guard for the "queue depth high" warning.
    queue_warned: Arc<AtomicBool>,
    /// Number of live reader threads attached to this package (one per
    /// physical HID interface). When this reaches zero in a reader's exit
    /// path, the package is considered fully disconnected.
    reader_count: Arc<AtomicUsize>,
}

impl HidDevicePackage {
    pub(crate) fn contains_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub(crate) fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
        // Drop the master sender so the dispatcher exits once all reader-side
        // clones also drop (which happens shortly after readers see the flag).
        if let Ok(mut slot) = self.report_tx.lock() {
            slot.take();
        }
    }

    fn spawn_reading_thread(
        &self,
        reader_device: HidDevice,
        device_path: String,
        descriptor: HidReportDescriptor,
    ) -> Result<std::thread::JoinHandle<()>, HidError> {
        let report_ids: Vec<u8> = descriptor
            .input_reports
            .iter()
            .map(|report| report.report_id)
            .collect();

        // Set blocking mode for a dedicated reader handle
        reader_device
            .set_blocking_mode(true)
            .map_err(|_| HidError::new("Failed to set blocking mode"))?;

        log::debug!(
            "{:?} {:2X?} Set blocking mode to true",
            device_path,
            report_ids
        );

        let uuid = self.uuid;
        let dev_path = device_path;
        let abort = self.abort.clone();
        let tx = self
            .report_tx
            .lock()
            .map_err(|_| HidError::lock_poisoned("HidDevicePackage::report_tx"))?
            .as_ref()
            .ok_or_else(|| HidError::Other("report channel already closed".into()))?
            .clone();
        let queue_depth = self.queue_depth.clone();
        let queue_warned = self.queue_warned.clone();
        let reader_count = self.reader_count.clone();
        reader_count.fetch_add(1, Ordering::Relaxed);

        let handle = std::thread::spawn(move || {
            let mut buffer = vec![0; READ_BUFFER_SIZE];
            log::debug!(
                "Reading thread running for device {:X?} {:02X?}",
                uuid,
                dev_path
            );

            loop {
                if abort.load(Ordering::Relaxed) {
                    log::debug!("Reading thread aborting");
                    break;
                }

                // Perform blocking read on dedicated reader handle (no shared mutex)
                let read_res = reader_device.read(&mut buffer);

                let shared: Arc<[u8]> = match read_res {
                    Ok(0) => {
                        // In blocking mode, Ok(0) is uncommon, but handle gracefully
                        log::debug!("{:2X?} No data read, retrying...", report_ids);
                        sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Ok(len) => Arc::from(buffer[..len].to_vec().into_boxed_slice()),
                    Err(err) => {
                        log::debug!("{:2X?} Failed to read report: {:?}", report_ids, err);
                        sleep(std::time::Duration::from_millis(READ_RETRY_DELAY_MS));
                        break;
                    }
                };

                // Hand off to the per-device dispatcher; never block the reader.
                let new_depth = crate::increment_queue_depth(queue_depth.as_ref());
                match tx.try_send(shared) {
                    Ok(()) => {
                        if new_depth >= REPORT_QUEUE_WARN_THRESHOLD
                            && !queue_warned.swap(true, Ordering::Relaxed)
                        {
                            log::warn!(
                                "report queue backlog {} >= {} for device {:032x}; consumer is slow",
                                new_depth, REPORT_QUEUE_WARN_THRESHOLD, uuid
                            );
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        crate::decrement_queue_depth(queue_depth.as_ref());
                        // Capacity reached; drop the newest packet to keep
                        // the reader running. The high-water warning above
                        // already fired (or is being suppressed).
                        log::warn!(
                            "report queue full ({}); dropping packet for device {:032x}",
                            REPORT_QUEUE_CAP,
                            uuid
                        );
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        crate::decrement_queue_depth(queue_depth.as_ref());
                        log::debug!("dispatcher gone for device {:032x}, reader exiting", uuid);
                        break;
                    }
                }
            }

            // Decrement the reader-count for this package. If we were the
            // last reader, the whole device is now fully disconnected and
            // should be removed from the global list; otherwise leave the
            // package alive so sibling interfaces can keep operating.
            let remaining = reader_count
                .fetch_sub(1, Ordering::Relaxed)
                .saturating_sub(1);
            if remaining > 0 {
                log::debug!(
                    "Reading thread exit {:2X?}; {} reader(s) still live for device {:032x}",
                    report_ids,
                    remaining,
                    uuid
                );
                return;
            }

            log::debug!(
                "Reading thread exit {:2X?}; last reader, removing device {:032x}",
                report_ids,
                uuid
            );
            if let Err(err) = remove_device(uuid) {
                log::debug!("Failed to remove device: {:?}", err);
            }
        });
        Ok(handle)
    }

    pub(crate) fn try_add(
        &mut self,
        device_info: &DeviceInfo,
        api: &HidApi,
    ) -> Result<bool, HidError> {
        let path = device_info
            .path()
            .to_str()
            .map_err(|_| HidError::new("Failed to get device path"))?
            .to_string();

        if self.contains_path(&path) {
            return Ok(false); // Device already exists
        }

        // Open a handle for writing/feature ops (shared via mutex)
        let device =
            Arc::new(Mutex::new(device_info.open_device(api).map_err(|_| {
                HidError::new("Failed to open device (write handle)")
            })?));
        // Open a dedicated handle for blocking reads to avoid mutex contention
        let reader_device = device_info
            .open_device(api)
            .map_err(|_| HidError::new("Failed to open device (reader handle)"))?;

        let descriptor = get_collections_by_device(device.clone())?;

        // Consult the user-supplied device filter (defaults to "accept all"
        // when none is installed). This replaces the previous hardcoded
        // "must have output report 0x02/0x21/0x22" policy.
        if !crate::evaluate_device_filter(&descriptor) {
            return Ok(false);
        }

        let _thread_handle =
            self.spawn_reading_thread(reader_device, path.clone(), descriptor.clone())?;

        // Update device package
        self.paths.insert(path);
        self.descriptors
            .output_reports
            .extend(descriptor.output_reports.clone());
        self.descriptors
            .input_reports
            .extend(descriptor.input_reports.clone());
        self.descriptors
            .feature_reports
            .extend(descriptor.feature_reports.clone());

        for out_report in descriptor.output_reports {
            let report_id = out_report.report_id;
            log::debug!("Adding report id: {:02X}", report_id);
            self.devices.insert(report_id, device.clone());
            self.report_info.insert(report_id, out_report);
        }

        Ok(true)
    }
}

////////////////////////////////////////
// Event notification
////////////////////////////////////////
fn notify_connection_changed(uuid: u128, connected: bool) {
    let listeners: Vec<ConnectionCallback> = match DEVICE_CONNECTION_LISTENERS.read() {
        Ok(binding) => binding.values().cloned().collect(),
        Err(_) => {
            log::debug!("Failed to acquire connection listeners lock");
            return;
        }
    };

    for listener in listeners {
        listener(DeviceId(uuid), connected);
    }
}

fn notify_report_arrive(uuid: u128, shared: Arc<[u8]>) {
    let listeners: Vec<ReportCallback> = match DEVICE_REPORT_LISTENERS.read() {
        Ok(binding) => match binding.get(&uuid) {
            Some(map) => map.values().cloned().collect(),
            None => return, // No listeners for this device
        },
        Err(_) => {
            log::debug!("Failed to acquire report listener lock");
            return;
        }
    };

    // Caller already wrapped the packet in an Arc; just clone the handle
    // for each listener.
    for listener in listeners {
        listener(DeviceId(uuid), shared.clone());
    }
}

////////////////////////////////////////
// Internal functions
////////////////////////////////////////
fn create_api() -> Result<HidApi, HidError> {
    HidApi::new().map_err(|err| {
        log::debug!("Failed to create HidApi: {:?}", err);
        HidError::Io(format!("failed to create HidApi: {err:?}"))
    })
}

fn get_collections_by_device(
    device: Arc<Mutex<HidDevice>>,
) -> Result<HidReportDescriptor, HidError> {
    let mut buf = [0; REPORT_DESCRIPTOR_BUFFER_SIZE];

    let size = device
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevice"))?
        .get_report_descriptor(&mut buf)
        .map_err(|err| HidError::Io(format!("failed to get report descriptor: {err:?}")))?;

    let descriptor = ReportDescriptor::try_from(&buf[..size])
        .map_err(|err| HidError::Other(format!("failed to parse report descriptor: {err:?}")))?;

    Ok(HidReportDescriptor::from_hid_report(descriptor))
}

fn update_device_list(vendor_ids: Vec<(u16, u16)>) -> Result<(), HidError> {
    let mut api_binding = HIDAPI
        .lock()
        .map_err(|_| HidError::lock_poisoned("HIDAPI"))?;

    let api = api_binding.as_mut().ok_or(HidError::NotInitialized)?;

    if let Err(e) = api.reset_devices() {
        log::warn!("hidapi reset_devices failed (continuing): {e:?}");
    }
    for (vid, pid) in vendor_ids {
        if let Err(e) = api.add_devices(vid, pid) {
            log::warn!("hidapi add_devices(vid={vid:#06x}, pid={pid:#06x}) failed: {e:?}");
        }
    }

    let mut seen_serials = HashSet::new();
    let device_infos = api.device_list();
    for device_info in device_infos {
        let serial_number = match device_info.serial_number() {
            Some(serial_number) => serial_number,
            None => {
                log::debug!("Device without serial number");
                continue;
            }
        };
        seen_serials.insert(serial_number.to_string());
        let exist_uuid = {
            let serial_number_to_uuid_binding = SERIAL_NUMBER_TO_UUID
                .read()
                .map_err(|_| HidError::lock_poisoned("SERIAL_NUMBER_TO_UUID"))?;
            serial_number_to_uuid_binding.get(serial_number).copied()
        };
        match exist_uuid {
            Some(uuid) => {
                // Device already exists
                let device_pack = snapshot_device(uuid)?;
                let mut pack = device_pack
                    .lock()
                    .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?;
                if let Ok(true) = pack.try_add(device_info, api) {
                    log::debug!("Sub device updated");
                }
            }
            None => {
                // New device
                // log::debug!("New device found");
                // Mint outside the reserved dummy-device range.
                let uuid = crate::get_uuid();

                // Per-device bounded channel: reader threads `try_send` reports
                // into it; a dedicated dispatcher thread drains it and fires
                // listeners, so slow user callbacks cannot stall the reader
                // (and thus cannot cause OS-level packet loss).
                let (tx, rx) = sync_channel::<Arc<[u8]>>(REPORT_QUEUE_CAP);
                let queue_depth = Arc::new(AtomicUsize::new(0));
                let queue_warned = Arc::new(AtomicBool::new(false));
                {
                    let queue_depth = queue_depth.clone();
                    let queue_warned = queue_warned.clone();
                    std::thread::spawn(move || {
                        log::debug!("dispatcher thread running for device {uuid:032x}");
                        while let Ok(shared) = rx.recv() {
                            let remaining_depth = crate::decrement_queue_depth(queue_depth.as_ref());
                            if remaining_depth < REPORT_QUEUE_WARN_THRESHOLD {
                                queue_warned.store(false, Ordering::Relaxed);
                            }
                            notify_report_arrive(uuid, shared);
                        }
                        log::debug!("dispatcher thread exiting for device {uuid:032x}");
                    });
                }

                let mut device_pack = HidDevicePackage {
                    uuid,
                    serial_number: serial_number.to_string(),
                    paths: HashSet::new(),
                    devices: HashMap::new(),
                    descriptors: HidReportDescriptor::new(),
                    report_info: HashMap::new(),
                    abort: Arc::new(AtomicBool::new(false)),
                    report_tx: Mutex::new(Some(tx)),
                    queue_depth,
                    queue_warned,
                    reader_count: Arc::new(AtomicUsize::new(0)),
                };
                match device_pack.try_add(device_info, api) {
                    Ok(true) => log::debug!("New device added"),
                    Ok(false) => continue, // Rejected by device filter
                    Err(err) => {
                        log::debug!("Failed to add device: {:?}", err);
                        continue;
                    }
                }

                {
                    let mut device_list_binding = DEVICE_LIST
                        .write()
                        .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
                    device_list_binding.insert(uuid, Arc::new(Mutex::new(device_pack)));
                }

                {
                    let mut serial_number_to_uuid_binding = SERIAL_NUMBER_TO_UUID
                        .write()
                        .map_err(|_| HidError::lock_poisoned("SERIAL_NUMBER_TO_UUID"))?;
                    serial_number_to_uuid_binding.insert(serial_number.to_string(), uuid);
                }
                notify_connection_changed(uuid, true);
            }
        }
    }

    prune_missing_devices(&seen_serials)?;

    //log::debug!("Device list updated---------------------------------");
    Ok(())
}

fn remove_device(uuid: u128) -> Result<bool, HidError> {
    let thread_id = std::thread::current().id();
    log::debug!("{:?} Removing device {:X?}", thread_id, uuid);
    let removed = {
        let mut device_list_binding = DEVICE_LIST
            .write()
            .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
        device_list_binding.remove(&uuid)
    };
    let device_binding = match removed {
        Some(binding) => binding,
        None => {
            log::debug!("Device not found");
            return Ok(false);
        }
    };
    let device_pack = device_binding
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?;
    device_pack.abort();
    log::debug!("{:?} About reading thread done", thread_id);

    let serial_number = device_pack.serial_number.clone();
    if let Ok(mut serial_number_to_uuid_binding) = SERIAL_NUMBER_TO_UUID.write() {
        serial_number_to_uuid_binding.remove(&serial_number);
    } else {
        log::debug!("Failed to acquire serial number to uuid lock for cleanup");
    }
    log::debug!("Device removed");
    notify_connection_changed(uuid, false);
    Ok(true)
}

////////////////////////////////////////
// Helper functions
////////////////////////////////////////

/// 通用的设备获取函数，消除代码重复
fn get_device_by_uuid<T, F>(uuid: u128, operation: F) -> Result<T, HidError>
where
    F: FnOnce(&HidDevice) -> Result<T, HidError>,
{
    let device_pack = snapshot_device(uuid)?;

    let device = device_pack
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevicePackage"))?
        .devices
        .values()
        .next()
        .ok_or(HidError::DeviceNotFound(uuid))?
        .clone();
    drop(device_pack);
    let device_guard = device
        .lock()
        .map_err(|_| HidError::lock_poisoned("HidDevice"))?;

    operation(&device_guard)
}

/// Look up a device package by uuid, returning a cloned `Arc` so the caller
/// can release the global `DEVICE_LIST` read lock immediately and only then
/// reach for the per-package mutex.
fn snapshot_device(uuid: u128) -> Result<Arc<Mutex<HidDevicePackage>>, HidError> {
    let device_list_binding = DEVICE_LIST
        .read()
        .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
    device_list_binding
        .get(&uuid)
        .cloned()
        .ok_or(HidError::DeviceNotFound(uuid))
}

fn normalize_vendor_ids(vendor_ids: &[(u16, Option<u16>)]) -> Vec<(u16, u16)> {
    vendor_ids
        .iter()
        .map(|(vid, pid)| (*vid, pid.unwrap_or(0)))
        .collect()
}

fn prune_missing_devices(seen_serials: &HashSet<String>) -> Result<(), HidError> {
    let stale: Vec<u128> = {
        let device_list_binding = DEVICE_LIST
            .read()
            .map_err(|_| HidError::lock_poisoned("DEVICE_LIST"))?;
        device_list_binding
            .iter()
            .filter_map(|(uuid, pack)| {
                let serial = pack.lock().ok()?.serial_number.clone();
                (!seen_serials.contains(&serial)).then_some(*uuid)
            })
            .collect()
    };

    for uuid in stale {
        if let Err(e) = remove_device(uuid) {
            log::warn!("failed to prune stale device {uuid:032x}: {e}");
        }
    }

    Ok(())
}
