#[cfg(target_arch = "wasm32")]
#[path = "web_hid.rs"]
mod platform_hid;

#[cfg(all(not(target_arch = "wasm32"), target_os = "android"))]
#[path = "android_hid.rs"]
mod platform_hid;

#[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
#[path = "os_hid.rs"]
mod platform_hid;

pub mod logger;
pub mod hid_error;
pub mod hid_report_descriptor;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use hid_error::HidError;
use hid_report_descriptor::HidReportDescriptor;
use futures::future::BoxFuture;

static HID_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct SafeCallback<T, R> {
    callback: Arc<dyn Fn(T) -> BoxFuture<'static, R> + Send + Sync>,
}

impl<T, R> std::fmt::Debug for SafeCallback<T, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafeCallback")
            .field("callback", &"<callback>")
            .finish()
    }
}
    
impl<T, R> SafeCallback<T, R> {
    pub fn new<F>(f: F) -> Self 
    where F: Fn(T) -> BoxFuture<'static, R> + Send + Sync + 'static 
    {
        Self { callback: Arc::new(f) }
    }

    pub async fn call(&self, arg: T) -> R {
        (self.callback)(arg).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn call_blocking(&self, arg: T) -> R {
        pollster::block_on((self.callback)(arg))
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.callback, &other.callback)
    }
}

#[derive(Clone)]
pub struct SafeCallback2<T1, T2, R> {
    callback: Arc<dyn Fn(T1, T2) -> BoxFuture<'static, R> + Send + Sync>,
}

impl<T1, T2, R> std::fmt::Debug for SafeCallback2<T1, T2, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafeCallback2")
            .field("callback", &"<callback>")
            .finish()
    }
}

impl<T1, T2, R> SafeCallback2<T1, T2, R> {
    pub fn new<F>(f: F) -> Self 
    where F: Fn(T1, T2) -> BoxFuture<'static, R> + Send + Sync + 'static 
    {
        Self { callback: Arc::new(f) }
    }

    pub async fn call(&self, arg1: T1, arg2: T2) -> R {
        (self.callback)(arg1, arg2).await
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn call_blocking(&self, arg1: T1, arg2: T2) -> R {
        pollster::block_on((self.callback)(arg1, arg2))
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.callback, &other.callback)
    }
}

pub struct Hid {}
    impl Hid {
        pub async fn init_hid() -> Result<(), HidError> {
        // 检查是否已经初始化
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
            },
            Err(e) => {
                log::debug!("hid_api init failed: {:?}", e);
                Err(e)
            },
        }
    }

    pub async fn is_supported() -> bool {
        platform_hid::is_supported()
    }

    pub async fn request_device(vpid: Vec<(u16, Option<u16>)>) -> Result<Vec<u128>, HidError> {
        match platform_hid::request_device(vpid).await {
            Ok(ids) => {
                log::debug!("request_device success: {:?}", ids);
                Ok(ids)
            },
            Err(e) => {
                log::error!("request_device failed: {:?}", e);
                Err(e)
            },
        }
    }

    pub fn get_device_list() -> Result<Vec<HidDevice>, HidError> {
        let devices = match  platform_hid::get_device_list() {
            Ok(devs) => {
                log::debug!("get_device_list success: {} devices", devs.len());
                devs
            },
            Err(e) => {
                log::error!("get_device_list failed: {:?}", e);
                return Err(e);
            },
        };
        Ok(devices.into_iter().map(HidDevice::from).collect())
    }

    pub async fn sub_connection_changed(callback: SafeCallback2::<u128, bool, ()>) -> Result<(), HidError> {
        log::debug!("sub_connection_changed called in hid_api");
        platform_hid::sub_connection_changed(callback).await
    }

    pub async fn unsub_connection_changed(callback: SafeCallback2::<u128, bool, ()>) -> Result<(), HidError> {
        platform_hid::unsub_connection_changed(callback).await
    }

}

pub struct HidDevice {
    pub uuid: u128,
}

impl PartialEq for HidDevice {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl From<u128> for HidDevice {
    fn from(uuid: u128) -> Self {
        HidDevice { uuid }
    }
}

impl From<HidDevice> for u128 {
    fn from(device: HidDevice) -> u128 {
        device.uuid
    }
}

impl HidDevice {
    pub fn new(handle: u128) -> Self {
        HidDevice {
            uuid: handle,
        }
    }

    pub fn available(&self) -> bool {
        platform_hid::available(self.uuid)
    }

    pub fn vid(&self) -> Result<u16, HidError> {
        platform_hid::vid(self.uuid)
    }

    pub fn pid(&self) -> Result<u16, HidError> {
        platform_hid::pid(self.uuid)
    }

    pub fn get_product_name(&self) -> Result<Option<String>, HidError> {
        platform_hid::get_product_name(self.uuid)
    }

    pub fn get_collections(&self) -> Result<HidReportDescriptor, HidError> {
        platform_hid::get_collections(self.uuid)
    }

    pub async fn send_report(&self, data: Vec<u8>) -> Result<(), HidError> {
        let mut buffer = data;
        platform_hid::send_report(self.uuid, &mut buffer).await
            .map(|_| ())
    }

    pub async fn send_report_slice(&self, data: &[u8]) -> Result<(), HidError> {
        let mut buffer = data.to_vec();
        platform_hid::send_report(self.uuid, &mut buffer).await
            .map(|_| ())
    }

    pub async fn send_firmware(&self, firmware: Vec<u8>, 
        write_data_cmd: u8, size_addr: u8, big_endian: u8, err_for_size: u8, encrypt: u8, check_sum: u8,
        on_progress: SafeCallback<f64, ()>) -> Result<usize, HidError> {
        let mut buffer = firmware;
        platform_hid::send_firmware(
            self.uuid, 
            &mut buffer, 
            write_data_cmd,
            size_addr, 
            big_endian, 
            err_for_size, 
            encrypt,
            check_sum,
            on_progress
        ).await
    }

    pub async fn add_report_listener(&self, callback: &SafeCallback2::<u128, Vec<u8>, ()>) -> Result<(), HidError> {
        platform_hid::sub_report_arrive(self.uuid, callback.clone()).await
    }

    pub async fn remove_report_listener(&self, callback: &SafeCallback2::<u128, Vec<u8>, ()>) -> Result<(), HidError> {
        platform_hid::unsub_report_arrive(self.uuid, callback.clone()).await
    }

    pub fn has_report_id(&self, report_id: u8) -> bool {
        log::debug!("hid has_report_id {:02X?}", report_id);
        platform_hid::has_report_id(self.uuid, report_id)
            .unwrap_or_else(|e| {
                log::debug!("has_report_id failed: {:?}", e);
                false
            })
    }
}
