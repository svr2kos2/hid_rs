use std::{
    collections::{
        HashMap, 
        HashSet
    },
    sync::{
        Arc, 
        Mutex, 
        RwLock,
        atomic::{AtomicBool, Ordering}
    }, 
    thread::sleep
};
use hidapi::{
    DeviceInfo, 
    HidApi, 
    HidDevice
};
use hidreport::ReportDescriptor;
use crate::hid_report_descriptor::{HidReportDescriptor, HidReportInfo};
use crate::{SafeCallback, SafeCallback2};
use crate::hid_error::HidError;
use once_cell::sync::Lazy;

////////////////////////////////////////
// Constants
////////////////////////////////////////
const REPORT_ID_02: u8 = 0x02;
const REPORT_ID_21: u8 = 0x21;
const REPORT_ID_22: u8 = 0x22;
const DEVICE_POLL_INTERVAL_MS: u64 = 1000;
const READ_RETRY_DELAY_MS: u64 = 500;
const READ_BUFFER_SIZE: usize = 1024;
const REPORT_DESCRIPTOR_BUFFER_SIZE: usize = 1024;

////////////////////////////////////////
// Interfaces
////////////////////////////////////////

pub(in crate) fn is_supported() -> bool {
    true
}

pub(in crate) fn available(uuid: u128) -> bool {
    match DEVICE_LIST.read() {
        Ok(binding) => binding.contains_key(&uuid),
        Err(err) => {
            log::debug!("Failed to acquire device list lock: {:?}", err);
            false
        }
    }
}

pub(in crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    get_device_by_uuid(uuid, |device| {
        device.get_device_info()
            .map(|info| info.vendor_id())
            .map_err(|_| HidError::new("Failed to get vendor id"))
    })
}

pub(in crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    get_device_by_uuid(uuid, |device| {
        device.get_device_info()
            .map(|info| info.product_id())
            .map_err(|_| HidError::new("Failed to get product id"))
    })
}

pub(in crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    get_device_by_uuid(uuid, |device| {
        device.get_product_string()
            .map_err(|_| HidError::new("Failed to get product name"))
    })
}

pub(in crate) async fn init() -> Result<(), HidError> {
    let api = create_api()?;
    HIDAPI.lock()
        .map_err(|_| HidError::new("Failed to acquire HIDAPI lock"))?
        .replace(api);

    fn poll_devices() -> Result<(), HidError> {
        let vpids = VID_PID_LIST.lock()
            .map_err(|_| HidError::new("Failed to acquire VID_PID_LIST lock"))?;
        let converted_vpids: Vec<(u16, u16)> = vpids.iter()
            .map(|(vid, pid)| (*vid, pid.unwrap_or(0)))
            .collect();
        drop(vpids);
        update_device_list(converted_vpids)
    }
    
    poll_devices()?;
    std::thread::spawn(move || {
        loop {
            if let Err(e) = poll_devices() {
                log::debug!("Device polling error: {:?}", e);
            }
            
            std::thread::sleep(std::time::Duration::from_millis(DEVICE_POLL_INTERVAL_MS));
        }
    });
    Ok(())
}

pub(in crate) async fn request_device(_vendor_ids: Vec<(u16, Option<u16>)>) -> Result<Vec<u128>, HidError> {
    // Dummy interface for compatibility
    let mut vid_pid_list = VID_PID_LIST.lock()
        .map_err(|_| HidError::new("Failed to acquire VID_PID_LIST lock"))?;
    vid_pid_list.clear();
    vid_pid_list.extend(_vendor_ids);
    Ok(vec![])
}

pub(in crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let device_list_binding = DEVICE_LIST.read()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    let list: Vec<u128> = device_list_binding.keys().cloned().collect();
    Ok(list)
}

pub(in crate) async fn sub_connection_changed(callback: SafeCallback2<u128, bool>) -> Result<(), HidError> {
    let uuids = {
        let device_list_binding = DEVICE_LIST.read()
            .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
        let v = device_list_binding.keys().cloned().collect::<Vec<u128>>();
        v
    };
    
    for uuid in uuids {
        callback.call_blocking(uuid, true);
    }
    
    let mut listeners_binding = DEVICE_CONNECTION_LISTENERS.write()
        .map_err(|_| HidError::new("Failed to acquire device connection listener lock"))?;
    listeners_binding.push(callback);
    Ok(())
}
pub(in crate) async fn unsub_connection_changed(callback: SafeCallback2<u128, bool>) -> Result<(), HidError> {
    let uuids = {
        let device_list_binding = DEVICE_LIST.read()
            .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
        let v = device_list_binding.keys().cloned().collect::<Vec<u128>>();
        v
    };
    
    for uuid in uuids {
        callback.call_blocking(uuid, false);
    }
    
    let mut listeners_binding = DEVICE_CONNECTION_LISTENERS.write()
        .map_err(|_| HidError::new("Failed to acquire device connection listener lock"))?;
    
    if let Some(index) = listeners_binding.iter().position(|x| x.ptr_eq(&callback)) {
        listeners_binding.remove(index);
    }
    Ok(())
}

pub(in crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    let device_list_binding = DEVICE_LIST.read()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    
    let device_pack = device_list_binding.get(&uuid)
        .ok_or_else(|| HidError::new("Device not found"))?;
    
    let descriptor = device_pack.lock()
        .map_err(|_| HidError::new("Failed to acquire device lock"))?
        .descriptors.clone();
    
    Ok(descriptor)
}

pub(in crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let device_list_binding = DEVICE_LIST.read()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    
    let device_binding = device_list_binding.get(&uuid)
        .ok_or_else(|| HidError::new("Device not found in device list"))?;
    
    let device_pack = device_binding.lock()
        .map_err(|_| HidError::new("Failed to acquire device lock"))?;
    Ok(device_pack.report_info.contains_key(&report_id))
}

pub(in crate) async fn send_report(uuid: u128, data: &mut Vec<u8>) -> Result<usize, HidError> {
    if data.is_empty() {
        return Err(HidError::new("Data cannot be empty"));
    }
    
    let report_id = data[0];
    let device_list_binding = DEVICE_LIST.read()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    
    let device_pack = device_list_binding.get(&uuid)
        .ok_or_else(|| HidError::new("Device not found"))?;
    
    let (device, size) = {
        let pack = device_pack.lock()
            .map_err(|_| HidError::new("Failed to acquire device lock"))?;
        
        let device = pack.devices.get(&report_id)
            .ok_or_else(|| HidError::new("Report id not found"))?
            .clone();
        
        let size = pack.report_info.get(&report_id)
            .ok_or_else(|| HidError::new("Report info not found"))?
            .size;
        
        (device, size)
    };
    
    if data.len() > size {
        return Err(HidError::new(&format!("Data size too large max: {}, got: {}", size, data.len())));
    }
    
    drop(device_list_binding);

    let device_guard = device.lock()
        .map_err(|_| HidError::new("Failed to acquire device mutex"))?;
    
    // Pad data to required size
    data.resize(size, 0);
    
    // log::debug!("Sending report: {:02X?}", data);
    let res = match device_guard.write(data) {
        Ok(written_size) => Ok(written_size),
        Err(err) => {
            log::debug!("Failed to write report: {:?}", err);
            drop(device_guard);
            let _ = remove_device(uuid); // Don't propagate removal errors
            Err(HidError::new("Failed to write report"))
        }
    };
    res
}

pub(in crate) async fn send_firmware(_uuid: u128, _firmware: &mut Vec<u8>, 
    _write_data_cmd:u8, _size_addr: u8, _big_endian: u8, _err_for_size: u8, _encrypt: u8, _check_sum: u8,
    _on_progress: SafeCallback<f64>) -> Result<usize, HidError> {
    log::debug!("Send firmware intended for quick firmware transfer on web. This is not supposed to be called on desktop");
    Ok(0)
}

pub(in crate) async fn sub_report_arrive(handle: u128, callback: SafeCallback2<u128, Vec<u8>>) -> Result<(), HidError> {
    let mut listeners_binding = DEVICE_REPORT_LISTENERS.write()
        .map_err(|_| HidError::new("Failed to acquire report listener lock"))?;
    
    let listeners = listeners_binding.entry(handle).or_insert_with(Vec::new);
    listeners.push(callback);
    Ok(())
}
pub(in crate) async fn unsub_report_arrive(handle: u128, callback: SafeCallback2<u128, Vec<u8>>) -> Result<(), HidError> {
    let mut listeners_binding = DEVICE_REPORT_LISTENERS.write()
        .map_err(|_| HidError::new("Failed to acquire report listener lock"))?;
    
    if let Some(listeners) = listeners_binding.get_mut(&handle) {
        if let Some(index) = listeners.iter().position(|x| x.ptr_eq(&callback)) {
            listeners.remove(index);
        }
    }
    Ok(())
}

////////////////////////////////////////
// Global variables
////////////////////////////////////////
static HIDAPI: Lazy<Mutex<Option<HidApi>>> = Lazy::new(|| Mutex::new(None));
static VID_PID_LIST: Lazy<Mutex<Vec<(u16, Option<u16>)>>>
    = Lazy::new(|| Mutex::new(vec![(0x8089, None)]));
static DEVICE_LIST: Lazy<RwLock<HashMap<u128, Mutex<HidDevicePackage>>>>
    = Lazy::new(|| RwLock::new(HashMap::new()));
static SERIAL_NUMBER_TO_UUID: Lazy<RwLock<HashMap<String, u128>>>
    = Lazy::new(|| RwLock::new(HashMap::new()));
static DEVICE_CONNECTION_LISTENERS: Lazy<RwLock<Vec<SafeCallback2<u128, bool>>>>
    = Lazy::new(|| RwLock::new(Vec::new()));
static DEVICE_REPORT_LISTENERS: Lazy<RwLock<HashMap<u128, Vec<SafeCallback2<u128, Vec<u8>>>>>>
    = Lazy::new(|| RwLock::new(HashMap::new()));

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
}

impl HidDevicePackage {
    pub(in crate) fn contains_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub(in crate) fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    fn spawn_reading_thread(&self, reader_device: HidDevice, device_path: String, descriptor: HidReportDescriptor) -> Result<std::thread::JoinHandle<()>, HidError> {
        let report_ids: Vec<u8> = descriptor.input_reports.iter()
            .map(|report| report.report_id)
            .collect();

        // Set blocking mode for a dedicated reader handle
        reader_device
            .set_blocking_mode(true)
            .map_err(|_| HidError::new("Failed to set blocking mode"))?;

        log::debug!("{:?} {:2X?} Set blocking mode to true", device_path, report_ids);
        
        let uuid = self.uuid;
        let dev_path = device_path;
        let abort = self.abort.clone();

        let handle = std::thread::spawn(move || {
            let mut buffer = vec![0; READ_BUFFER_SIZE];
            log::debug!("Reading thread running for device {:X?} {:02X?}", uuid, dev_path);
            
            loop {
                if abort.load(Ordering::Relaxed) {
                    log::debug!("Reading thread aborting");
                    break;
                }
                
                // Perform blocking read on dedicated reader handle (no shared mutex)
                let read_res = reader_device.read(&mut buffer);
                
                let packet = match read_res {
                    Ok(0) => {
                        // In blocking mode, Ok(0) is uncommon, but handle gracefully
                        log::debug!("{:2X?} No data read, retrying...", report_ids);
                        sleep(std::time::Duration::from_millis(10));
                        continue;
                    },
                    Ok(len) => {
                        // log::debug!("{:2X?} Read {} bytes", report_ids, len);
                        buffer[..len].to_vec()
                    },
                    Err(err) => {
                        log::debug!("{:2X?} Failed to read report: {:?}", report_ids, err);
                        sleep(std::time::Duration::from_millis(READ_RETRY_DELAY_MS));
                        break;
                    }
                };
                
                // log::debug!("Report arrived: {:02X?}", packet);
                notify_report_arrive(uuid, packet);
            }
            
            // Only remove device for specific report IDs
            let should_exit = report_ids.contains(&REPORT_ID_21) 
                || report_ids.contains(&REPORT_ID_22) 
                || report_ids.contains(&REPORT_ID_02);

            if !should_exit {
                log::debug!("Reading thread exit {:2X?}, keep others listening", report_ids);
                return;
            }
            
            log::debug!("Reading thread exit {:2X?}, remove device", report_ids);
            if let Err(err) = remove_device(uuid) {
                log::debug!("Failed to remove device: {:?}", err);
            }
        });
        Ok(handle)
    }

    pub(in crate) fn try_add(&mut self, device_info: &DeviceInfo, api: &HidApi) -> Result<bool, HidError> {
        let path = device_info.path().to_str()
            .map_err(|_| HidError::new("Failed to get device path"))?
            .to_string();
            
        if self.contains_path(&path) {
            return Ok(false); // Device already exists
        }
        
        // Open a handle for writing/feature ops (shared via mutex)
        let device = Arc::new(Mutex::new(
            device_info.open_device(api)
                .map_err(|_| HidError::new("Failed to open device (write handle)"))?
        ));
        // Open a dedicated handle for blocking reads to avoid mutex contention
        let reader_device = device_info.open_device(api)
            .map_err(|_| HidError::new("Failed to open device (reader handle)"))?;
        
        let descriptor = get_collections_by_device(device.clone())?;

        // Check for required report IDs
        let has_required_report_id = descriptor.output_reports.iter()
            .any(|r| r.report_id == REPORT_ID_02 
                  || r.report_id == REPORT_ID_21 
                  || r.report_id == REPORT_ID_22);
                  
        if !has_required_report_id {
            return Ok(false); // Device doesn't have required report IDs
        }

    let _thread_handle = self.spawn_reading_thread(reader_device, path.clone(), descriptor.clone())?;

        // Update device package
        self.paths.insert(path);
        self.descriptors.output_reports.extend(descriptor.output_reports.clone());
        self.descriptors.input_reports.extend(descriptor.input_reports.clone());
        self.descriptors.feature_reports.extend(descriptor.feature_reports.clone());
        
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
    let listeners = match DEVICE_CONNECTION_LISTENERS.read() {
        Ok(binding) => binding.clone(),
        Err(_) => {
            log::debug!("Failed to acquire connection listeners lock");
            return;
        }
    };
    
    for listener in listeners.iter() {
        listener.call_blocking(uuid, connected);
    }
}

fn notify_report_arrive(uuid: u128, report: Vec<u8>) {
    let listeners = match DEVICE_REPORT_LISTENERS.read() {
        Ok(binding) => match binding.get(&uuid) {
            Some(listeners) => listeners.clone(),
            None => return, // No listeners for this device
        },
        Err(_) => {
            log::debug!("Failed to acquire report listener lock");
            return;
        }
    };
    
    for listener in listeners.iter() {
        listener.call_blocking(uuid, report.clone());
    }
}

////////////////////////////////////////
// Internal functions
////////////////////////////////////////
fn create_api() -> Result<HidApi, HidError> {
    HidApi::new().map_err(|err| {
        log::debug!("Failed to create HidApi: {:?}", err);
        HidError::new("Failed to create HidApi")
    })
}

fn get_collections_by_device(device: Arc<Mutex<HidDevice>>) -> Result<HidReportDescriptor, HidError> {
    let mut buf = [0; REPORT_DESCRIPTOR_BUFFER_SIZE];
    
    let size = device.lock()
        .map_err(|_| HidError::new("Failed to acquire device lock"))?
        .get_report_descriptor(&mut buf)
        .map_err(|_| HidError::new("Failed to get report descriptor"))?;
    
    let descriptor = ReportDescriptor::try_from(&buf[..size])
        .map_err(|_| HidError::new("Failed to parse report descriptor"))?;
    
    Ok(HidReportDescriptor::from_hid_report(descriptor))
}

fn update_device_list(vendor_ids: Vec<(u16, u16)>) -> Result<(), HidError> {
    let mut api_binding = HIDAPI.lock()
        .map_err(|_| HidError::new("Failed to acquire HIDAPI lock"))?;
    
    let api = api_binding.as_mut()
        .ok_or_else(|| HidError::new("HidApi is not initialized"))?;
    
    let _ = api.reset_devices();
    for (vid, pid) in vendor_ids {
        let _ = api.add_devices(vid, pid);
    }
    
    let device_infos = api.device_list();
    for device_info in device_infos {
        let serial_number = match device_info.serial_number() {
            Some(serial_number) => serial_number,
            None => {
                log::debug!("Device without serial number");
                continue;
            },
        };
        let exist_uuid = {
            let serial_number_to_uuid_binding = SERIAL_NUMBER_TO_UUID.read()
                .map_err(|_| HidError::new("Failed to acquire serial number to uuid lock"))?;
            serial_number_to_uuid_binding.get(serial_number).copied()
        };
        match exist_uuid {
            Some(uuid) => { // Device already exists
                let device_list_binding = DEVICE_LIST.read()
                    .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
                
                let device_binding = device_list_binding.get(&uuid)
                    .ok_or_else(|| HidError::new("Device exists in serial number mapping but not in device list"))?;
                
                let mut device_pack = device_binding.lock()
                    .map_err(|_| HidError::new("Failed to acquire device lock"))?;
                if let Ok(true) = device_pack.try_add(device_info, api) {
                    log::debug!("Sub device updated");
                }
            },
            None => { // New device
                // log::debug!("New device found");
                let uuid = uuid::Uuid::new_v4().as_u128();
                let mut device_pack = HidDevicePackage {
                    uuid,
                    serial_number: serial_number.to_string(),
                    paths: HashSet::new(),
                    devices: HashMap::new(),
                    descriptors: HidReportDescriptor::new(),
                    report_info: HashMap::new(),
                    abort: Arc::new(AtomicBool::new(false)),
                };
                match device_pack.try_add(device_info, api) {
                    Ok(true) => log::debug!("New device added"),
                    Ok(false) => continue, // Device doesn't have required report IDs
                    Err(err) => {
                        log::debug!("Failed to add device: {:?}", err);
                        continue;
                    },
                }

                {
                    let mut device_list_binding = DEVICE_LIST.write()
                        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
                    device_list_binding.insert(uuid, Mutex::new(device_pack));
                }
                
                {
                    let mut serial_number_to_uuid_binding = SERIAL_NUMBER_TO_UUID.write()
                        .map_err(|_| HidError::new("Failed to acquire serial number to uuid lock"))?;
                    serial_number_to_uuid_binding.insert(serial_number.to_string(), uuid);
                }
                notify_connection_changed(uuid, true);
            },
        }
    }

    //log::debug!("Device list updated---------------------------------");
    Ok(())
}

fn remove_device(uuid: u128) -> Result<bool, HidError> {
    let thread_id = std::thread::current().id();
    log::debug!("{:?} Removing device {:X?}", thread_id, uuid);
    log::debug!("{:?} Get device list binding {:?}", thread_id, DEVICE_LIST);
    let mut device_list_binding = DEVICE_LIST.write()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    log::debug!("{:?} Get device binding {:?}", thread_id, device_list_binding);
    let device_binding = match device_list_binding.remove(&uuid) {
        Some(binding) => binding,
        None => {
            log::debug!("Device not found");
            return Ok(false);
        },
    };
    log::debug!("{:?} Removing device from list done", thread_id);
    drop(device_list_binding);
    let device_pack = device_binding.lock()
        .map_err(|_| HidError::new("Failed to acquire device lock"))?;
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
    let device_list_binding = DEVICE_LIST.read()
        .map_err(|_| HidError::new("Failed to acquire device list lock"))?;
    
    let device_pack = device_list_binding.get(&uuid)
        .ok_or_else(|| HidError::new("Device not found"))?;
    
    let device = device_pack.lock()
        .map_err(|_| HidError::new("Failed to acquire device lock"))?
        .devices.values().next()
        .ok_or_else(|| HidError::new("No devices found for this UUID"))?
        .clone();
    drop(device_list_binding);
    let device_guard = device.lock()
        .map_err(|_| HidError::new("Failed to acquire device mutex"))?;
    
    operation(&*device_guard)
}
