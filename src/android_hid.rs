use crate::hid_error::HidError;
use crate::hid_report_descriptor::HidReportDescriptor;
use crate::{ConnectionCallback, DeviceId, ProgressCallback, ReportCallback, SubscriptionId};
use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{sync_channel, SyncSender, TrySendError},
    Arc, RwLock,
};

/// Per-device bounded report queue capacity (drop newest on overflow).
const REPORT_QUEUE_CAP: usize = 4096;
/// Edge-triggered warning threshold for backlog depth.
const REPORT_QUEUE_WARN_THRESHOLD: usize = 256;

use jni::{
    objects::{GlobalRef, JByteArray, JClass, JObject, JString, JValue},
    JNIEnv, JavaVM,
};
use once_cell::sync::{Lazy, OnceCell};

static ANDROID_APP_CTX: OnceCell<GlobalRef> = OnceCell::new();
static ANDROID_JVM: OnceCell<JavaVM> = OnceCell::new();
static BRIDGE_CLASS: OnceCell<GlobalRef> = OnceCell::new();

// ===== Android-specific connection change tracking =====
static DEVICE_SET: Lazy<RwLock<HashSet<u128>>> = Lazy::new(|| RwLock::new(HashSet::new()));
static DEVICE_CONNECTION_LISTENERS: Lazy<RwLock<HashMap<SubscriptionId, ConnectionCallback>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static POLLER_RUNNING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));
static POLLER_ABORT: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

// Per-device report listeners and background readers
static DEVICE_REPORT_LISTENERS: Lazy<
    RwLock<HashMap<u128, HashMap<SubscriptionId, ReportCallback>>>,
> = Lazy::new(|| RwLock::new(HashMap::new()));
static REPORT_READERS: Lazy<RwLock<HashSet<u128>>> = Lazy::new(|| RwLock::new(HashSet::new()));
static REPORT_ABORT_FLAGS: Lazy<RwLock<HashMap<u128, Arc<AtomicBool>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// Per-device report dispatch channel: reader threads `try_send` packets into
// it; a dedicated dispatcher thread drains it and invokes user listeners.
// This decouples device I/O latency from user-callback latency.
struct ReportChannel {
    // Held only to keep the channel alive; dropping the struct closes the
    // sender side so the dispatcher thread can exit. The reader thread holds
    // its own clones of tx/depth/warned for the hot path.
    #[allow(dead_code)]
    tx: SyncSender<Arc<[u8]>>,
    #[allow(dead_code)]
    depth: Arc<AtomicUsize>,
    #[allow(dead_code)]
    warned: Arc<AtomicBool>,
}
static REPORT_CHANNELS: Lazy<RwLock<HashMap<u128, ReportChannel>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn log_enter(name: &str) {
    log::trace!("ENTER {}", name);
}

fn log_exit(name: &str) {
    log::trace!("EXIT {}", name);
}

struct FnLogger<'a> {
    name: &'a str,
}

impl<'a> FnLogger<'a> {
    fn new(name: &'a str) -> Self {
        log_enter(name);
        Self { name }
    }
}

impl<'a> Drop for FnLogger<'a> {
    fn drop(&mut self) {
        log_exit(self.name);
    }
}

pub(crate) fn is_supported() -> bool {
    let _fn_logger = FnLogger::new("android_hid::is_supported");
    {
        log::debug!("android_hid is_supported check");
        match get_vm_and_context() {
            Ok((vm, ctx)) => {
                log::debug!("JNI env/context obtained");
                let mut env = match vm.attach_current_thread() {
                    Ok(e) => e,
                    Err(e) => {
                        log::debug!("attach_current_thread failed: {:?}", e);
                        return true;
                    }
                };
                match call_bridge_is_supported(&mut env, &ctx) {
                    Ok(res) => return res,
                    Err(e) => log::debug!("JNI isSupported failed: {:?}", e),
                }
            }
            Err(e) => log::debug!("JNI env/context unavailable: {:?}", e),
        }
    }
    true
}

pub(crate) async fn init() -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::init");
    log::debug!("android_hid init placeholder");
    Ok(())
}

/// Stop the connection-poller thread (if running). Idempotent.
pub(crate) fn shutdown() -> Result<(), HidError> {
    POLLER_ABORT.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) async fn request_device(
    vendor_ids: Vec<(u16, Option<u16>)>,
) -> Result<Vec<u128>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::request_device");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let json = serde_json::to_string(
            &vendor_ids
                .iter()
                .map(|(v, p)| serde_json::json!({"vendorId": v, "productId": p}))
                .collect::<Vec<_>>(),
        )
        .map_err(|e| HidError::Io(format!("json: {e}")))?;
        let jfilters = env
            .new_string(json)
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let jctx = ctx.as_obj();
        let jfilters_obj: JObject = jfilters.into();
        let args = vec![JValue::Object(&jctx), JValue::Object(&jfilters_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let arr = env
            .call_static_method(
                jclass,
                "requestDevicesJson",
                "(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::Io(format!("call requestDevicesJson: {e:?}")))?;
        let jstr: JString = arr
            .l()
            .map_err(|e| HidError::Io(format!("to obj: {e:?}")))?
            .into();
        let rust_str = env
            .get_string(&jstr)
            .map_err(|e| HidError::Io(format!("get_string: {e:?}")))?;
        let uuids: Vec<String> =
            serde_json::from_str(rust_str.to_str().unwrap_or("[]")).unwrap_or_default();
        let ids = uuids
            .into_iter()
            .filter_map(|s| uuid::Uuid::try_parse(&s).ok().map(|u| u.as_u128()))
            .collect();
        return Ok(ids);
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

pub(crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_device_list");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let jctx = ctx.as_obj();
        let args = vec![JValue::Object(&jctx)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let arr = env
            .call_static_method(
                jclass,
                "getDeviceListJson",
                "(Landroid/content/Context;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::Io(format!("call getDeviceListJson: {e:?}")))?;
        let jstr: JString = arr
            .l()
            .map_err(|e| HidError::Io(format!("to obj: {e:?}")))?
            .into();
        let rust_str = env
            .get_string(&jstr)
            .map_err(|e| HidError::Io(format!("get_string: {e:?}")))?;
        let uuids: Vec<String> =
            serde_json::from_str(rust_str.to_str().unwrap_or("[]")).unwrap_or_default();
        let ids = uuids
            .into_iter()
            .filter_map(|s| uuid::Uuid::try_parse(&s).ok().map(|u| u.as_u128()))
            .collect();
        return Ok(ids);
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

pub(crate) fn register_connection_listener(
    id: SubscriptionId,
    callback: ConnectionCallback,
) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::register_connection_listener");

    let current_ids = get_device_list().unwrap_or_default();

    {
        let mut listeners = DEVICE_CONNECTION_LISTENERS
            .write()
            .map_err(|_| HidError::lock_poisoned("DEVICE_CONNECTION_LISTENERS"))?;
        listeners.insert(id, callback.clone());
    }

    // Replay the current connection state to the new subscriber without holding any locks.
    for id in &current_ids {
        callback(DeviceId(*id), true);
    }

    let need_start = !POLLER_RUNNING.swap(true, Ordering::SeqCst);
    if need_start {
        POLLER_ABORT.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = DEVICE_SET.write() {
            guard.clear();
            guard.extend(current_ids.into_iter());
        }
        std::thread::spawn(move || {
            let _attach_guard = if let Ok((vm, _)) = get_vm_and_context() {
                vm.attach_current_thread().ok()
            } else {
                None
            };

            loop {
                if POLLER_ABORT.load(Ordering::Relaxed) {
                    break;
                }
                let new_list = get_device_list().unwrap_or_default();
                let new_set: HashSet<u128> = new_list.into_iter().collect();

                let (added, removed) = {
                    let mut added = Vec::new();
                    let mut removed = Vec::new();
                    if let Ok(mut cur) = DEVICE_SET.write() {
                        for id in cur.iter() {
                            if !new_set.contains(id) {
                                removed.push(*id);
                            }
                        }
                        for id in new_set.iter() {
                            if !cur.contains(id) {
                                added.push(*id);
                            }
                        }
                        *cur = new_set;
                    }
                    (added, removed)
                };

                if !added.is_empty() || !removed.is_empty() {
                    let listeners: Vec<ConnectionCallback> =
                        match DEVICE_CONNECTION_LISTENERS.read() {
                            Ok(map) => map.values().cloned().collect(),
                            Err(_) => Vec::new(),
                        };
                    for id in added.iter() {
                        log::debug!("Device connected: {}", uuid::Uuid::from_u128(*id));
                        for cb in &listeners {
                            cb(DeviceId(*id), true);
                        }
                    }
                    for id in removed.iter() {
                        log::debug!("Device disconnected: {}", uuid::Uuid::from_u128(*id));
                        for cb in &listeners {
                            cb(DeviceId(*id), false);
                        }
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
            POLLER_RUNNING.store(false, Ordering::SeqCst);
        });
    }

    Ok(())
}

pub(crate) fn unregister_connection_listener(id: SubscriptionId) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::unregister_connection_listener");

    let mut listeners = DEVICE_CONNECTION_LISTENERS
        .write()
        .map_err(|_| HidError::lock_poisoned("DEVICE_CONNECTION_LISTENERS"))?;
    listeners.remove(&id);
    if listeners.is_empty() {
        POLLER_ABORT.store(true, Ordering::SeqCst);
    }
    Ok(())
}

pub(crate) fn available(uuid: u128) -> bool {
    let _fn_logger = FnLogger::new("android_hid::available");
    {
        if let Ok((vm, ctx)) = get_vm_and_context() {
            if let Ok(mut env) = vm.attach_current_thread() {
                if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                    if let Ok(local_class) = env.new_local_ref(gclass.as_obj()) {
                        let jclass: JClass = JClass::from(local_class);
                        let juuid = match env.new_string(uuid::Uuid::from_u128(uuid).to_string()) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                        let juuid_obj: JObject = juuid.into();
                        let args = vec![JValue::Object(&ctx.as_obj()), JValue::Object(&juuid_obj)];
                        if let Ok(val) = env.call_static_method(
                            jclass,
                            "available",
                            "(Landroid/content/Context;Ljava/lang/String;)Z",
                            &args,
                        ) {
                            return val.z().unwrap_or(false);
                        }
                    }
                }
            }
        }
    }
    #[allow(unreachable_code)]
    false
}

pub(crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    let _fn_logger = FnLogger::new("android_hid::vid");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let juuid = env
            .new_string(uuid::Uuid::from_u128(uuid).to_string())
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(jclass, "getVid", "(Ljava/lang/String;)I", &args)
            .map_err(|e| HidError::Io(format!("call getVid: {e:?}")))?;
        let vid = val
            .i()
            .map_err(|e| HidError::Io(format!("to int: {e:?}")))?;
        if vid >= 0 {
            return Ok(vid as u16);
        }
        return Err(HidError::DeviceNotFound(uuid));
    }
    #[allow(unreachable_code)]
    Err(HidError::NotImplemented)
}

pub(crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    let _fn_logger = FnLogger::new("android_hid::pid");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let juuid = env
            .new_string(uuid::Uuid::from_u128(uuid).to_string())
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(jclass, "getPid", "(Ljava/lang/String;)I", &args)
            .map_err(|e| HidError::Io(format!("call getPid: {e:?}")))?;
        let pid = val
            .i()
            .map_err(|e| HidError::Io(format!("to int: {e:?}")))?;
        if pid >= 0 {
            return Ok(pid as u16);
        }
        return Err(HidError::DeviceNotFound(uuid));
    }
    #[allow(unreachable_code)]
    Err(HidError::NotImplemented)
}

pub(crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_product_name");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let juuid = env
            .new_string(uuid::Uuid::from_u128(uuid).to_string())
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(
                jclass,
                "getProductName",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::Io(format!("call getProductName: {e:?}")))?;
        let s_obj = val
            .l()
            .map_err(|e| HidError::Io(format!("to obj: {e:?}")))?;
        if s_obj.is_null() {
            return Ok(None);
        }
        let jstr: JString = s_obj.into();
        let rust_str = env
            .get_string(&jstr)
            .map_err(|e| HidError::Io(format!("get_string: {e:?}")))?;
        return Ok(Some(rust_str.to_str().unwrap_or("").to_string()));
    }
    #[allow(unreachable_code)]
    Ok(None)
}

pub(crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_collections");
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let juuid = env
            .new_string(uuid::Uuid::from_u128(uuid).to_string())
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![
            JValue::Object(&ctx.as_obj()),
            JValue::Object(&juuid_obj),
            JValue::Int(1024),
        ];
        let val = env
            .call_static_method(
                jclass,
                "getReportDescriptor",
                "(Landroid/content/Context;Ljava/lang/String;I)[B",
                &args,
            )
            .map_err(|e| HidError::Io(format!("call getReportDescriptor: {e:?}")))?;
        let obj = val
            .l()
            .map_err(|e| HidError::Io(format!("to obj: {e:?}")))?;
        if obj.is_null() {
            return Err(HidError::Other("no report descriptor".to_string()));
        }
        let jarr: JByteArray = JByteArray::from(obj);
        let bytes = env
            .convert_byte_array(&jarr)
            .map_err(|e| HidError::Io(format!("convert_byte_array: {e:?}")))?;

        let report = hidreport::ReportDescriptor::try_from(bytes.as_slice())
            .map_err(|_| HidError::Other("failed to parse report descriptor".to_string()))?;
        let res = crate::hid_report_descriptor::HidReportDescriptor::from_hid_report(report);

        return Ok(res);
    }
    #[allow(unreachable_code)]
    Err(HidError::NotImplemented)
}

pub(crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let _fn_logger = FnLogger::new("android_hid::has_report_id");
    log::debug!("has_report_id called in android_hid {:02X?}", report_id);
    let desc = get_collections(uuid)?;

    let found = desc.input_reports.iter().any(|r| r.report_id == report_id)
        || desc.output_reports.iter().any(|r| r.report_id == report_id)
        || desc
            .feature_reports
            .iter()
            .any(|r| r.report_id == report_id);
    Ok(found)
}

pub(crate) async fn send_report(uuid: u128, data: Vec<u8>) -> Result<usize, HidError> {
    let _fn_logger = FnLogger::new("android_hid::send_report");
    {
        if data.is_empty() {
            return Err(HidError::EmptyData);
        }
        let report_id = data[0];
        let desc = get_collections(uuid)?;
        let size_opt = desc
            .output_reports
            .iter()
            .find(|r| r.report_id == report_id)
            .map(|r| r.size)
            .or_else(|| {
                desc.input_reports
                    .iter()
                    .find(|r| r.report_id == report_id)
                    .map(|r| r.size)
            });
        let size = match size_opt {
            Some(s) if s > 0 => s,
            _ => {
                log::debug!(
                    "android_hid send_report: unknown size for report id {:02X}, sending {} bytes as-is",
                    report_id,
                    data.len()
                );
                data.len()
            }
        };
        if data.len() > size {
            return Err(HidError::Io(format!(
                "Data size too large for report {:02X} max: {}, got: {}",
                report_id,
                size,
                data.len()
            )));
        }
        let to_send: Vec<u8> = if data.len() < size {
            let mut v = vec![0u8; size];
            v[..data.len()].copy_from_slice(&data[..]);
            v
        } else {
            data.clone()
        };

        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::Other(e))?;
        let mut env = vm
            .attach_current_thread()
            .map_err(|e| HidError::Io(format!("attach_current_thread: {e:?}")))?;
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::Other(e))?;
        let local_class = env
            .new_local_ref(gclass.as_obj())
            .map_err(|e| HidError::Io(format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let juuid = env
            .new_string(uuid::Uuid::from_u128(uuid).to_string())
            .map_err(|e| HidError::Io(format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let jdata = env
            .byte_array_from_slice(&to_send[..])
            .map_err(|e| HidError::Io(format!("byte_array_from_slice: {e:?}")))?;
        let jbytearr: JObject = JObject::from(jdata);
        let args = vec![
            JValue::Object(&ctx.as_obj()),
            JValue::Object(&juuid_obj),
            JValue::Object(&jbytearr),
        ];
        let res = env
            .call_static_method(
                jclass,
                "sendOutputReport",
                "(Landroid/content/Context;Ljava/lang/String;[B)I",
                &args,
            )
            .map_err(|e| HidError::Io(format!("call sendOutputReport: {e:?}")))?;
        let wrote = res
            .i()
            .map_err(|e| HidError::Io(format!("to int: {e:?}")))?;
        if wrote < 0 {
            return Err(HidError::Io("send failed".to_string()));
        }
        return Ok(wrote as usize);
    }
    #[allow(unreachable_code)]
    Err(HidError::NotImplemented)
}

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
    let _fn_logger = FnLogger::new("android_hid::send_firmware");
    Err(HidError::NotImplemented)
}

fn notify_report_arrive(uuid: u128, shared: Arc<[u8]>) {
    let listeners: Vec<ReportCallback> = match DEVICE_REPORT_LISTENERS.read() {
        Ok(map) => map
            .get(&uuid)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default(),
        Err(_) => return,
    };
    for cb in listeners {
        cb(DeviceId(uuid), shared.clone());
    }
}

pub(crate) fn register_report_listener(
    uuid: u128,
    id: SubscriptionId,
    callback: ReportCallback,
) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::register_report_listener");
    log::debug!(
        "register_report_listener called for {:?}",
        uuid::Uuid::from_u128(uuid)
    );
    {
        let mut map = DEVICE_REPORT_LISTENERS
            .write()
            .map_err(|_| HidError::lock_poisoned("DEVICE_REPORT_LISTENERS"))?;
        map.entry(uuid).or_default().insert(id, callback);
    }

    {
        if let Ok(readers) = REPORT_READERS.read() {
            if readers.contains(&uuid) {
                return Ok(());
            }
        }
    }

    let abort_flag = {
        let mut aborts = REPORT_ABORT_FLAGS
            .write()
            .map_err(|_| HidError::lock_poisoned("REPORT_ABORT_FLAGS"))?;
        aborts
            .entry(uuid)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };

    abort_flag.store(false, Ordering::SeqCst);

    // Set up per-device dispatch channel + dispatcher thread. The reader
    // thread (spawned below) only does `try_send` so slow user callbacks
    // cannot stall device I/O.
    let (tx, rx) = sync_channel::<Arc<[u8]>>(REPORT_QUEUE_CAP);
    let queue_depth = Arc::new(AtomicUsize::new(0));
    let queue_warned = Arc::new(AtomicBool::new(false));
    let reader_tx = tx.clone();
    let reader_depth = queue_depth.clone();
    let reader_warned = queue_warned.clone();
    {
        let mut channels = REPORT_CHANNELS
            .write()
            .map_err(|_| HidError::lock_poisoned("REPORT_CHANNELS"))?;
        channels.insert(
            uuid,
            ReportChannel {
                tx,
                depth: queue_depth.clone(),
                warned: queue_warned.clone(),
            },
        );
    }
    {
        let dispatch_depth = queue_depth;
        let dispatch_warned = queue_warned;
        std::thread::spawn(move || {
            log::debug!("dispatcher thread running for device {uuid:032x}");
            while let Ok(shared) = rx.recv() {
                let remaining_depth = crate::decrement_queue_depth(dispatch_depth.as_ref());
                if remaining_depth < REPORT_QUEUE_WARN_THRESHOLD {
                    dispatch_warned.store(false, Ordering::Relaxed);
                }
                notify_report_arrive(uuid, shared);
            }
            log::debug!("dispatcher thread exiting for device {uuid:032x}");
        });
    }

    {
        let mut readers_guard = REPORT_READERS
            .write()
            .map_err(|_| HidError::lock_poisoned("REPORT_READERS"))?;
        if readers_guard.contains(&uuid) {
            log::debug!(
                "reader for {:?} already exists, not spawning",
                uuid::Uuid::from_u128(uuid)
            );
            return Ok(());
        }
        readers_guard.insert(uuid);
        log::debug!(
            "spawning report reader for {:?}",
            uuid::Uuid::from_u128(uuid)
        );
    }

    let _ = std::thread::spawn(move || {
        log::debug!(
            "Report reader for {:?} started",
            uuid::Uuid::from_u128(uuid)
        );
        loop {
            if abort_flag.load(Ordering::Relaxed) {
                log::debug!(
                    "Report reader for {:?} aborting",
                    uuid::Uuid::from_u128(uuid)
                );
                break;
            }
            {
                if let Ok((vm, ctx)) = get_vm_and_context() {
                    if let Ok(mut env) = vm.attach_current_thread() {
                        if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                            if let Ok(local_class_start) = env.new_local_ref(gclass.as_obj()) {
                                let jclass_start: JClass = JClass::from(local_class_start);
                                let juuid = match env
                                    .new_string(uuid::Uuid::from_u128(uuid).to_string())
                                {
                                    Ok(s) => s,
                                    Err(_) => {
                                        std::thread::sleep(std::time::Duration::from_millis(200));
                                        continue;
                                    }
                                };
                                let juuid_obj: JObject = juuid.into();
                                let start_args = vec![
                                    JValue::Object(&ctx.as_obj()),
                                    JValue::Object(&juuid_obj),
                                    JValue::Int(1024),
                                ];
                                if let Err(e) = env.call_static_method(
                                    jclass_start,
                                    "startInputListener",
                                    "(Landroid/content/Context;Ljava/lang/String;I)Z",
                                    &start_args,
                                ) {
                                    log::warn!("JNI startInputListener({uuid:032x}) failed: {e:?}");
                                }

                                let local_class_take = match env.new_local_ref(gclass.as_obj()) {
                                    Ok(c) => c,
                                    Err(_) => {
                                        std::thread::sleep(std::time::Duration::from_millis(10));
                                        continue;
                                    }
                                };
                                let jclass_take: JClass = JClass::from(local_class_take);
                                let take_args = vec![
                                    JValue::Object(&ctx.as_obj()),
                                    JValue::Object(&juuid_obj),
                                    JValue::Int(0),
                                ];
                                match env.call_static_method(
                                    jclass_take,
                                    "takeInputReport",
                                    "(Landroid/content/Context;Ljava/lang/String;I)[B",
                                    &take_args,
                                ) {
                                    Ok(val) => {
                                        if let Ok(obj) = val.l() {
                                            if !obj.is_null() {
                                                let jarr: JByteArray = JByteArray::from(obj);
                                                if let Ok(bytes) = env.convert_byte_array(&jarr) {
                                                    log::debug!(
                                                        "Report reader for {:?} received {} bytes from takeInputReport",
                                                        uuid::Uuid::from_u128(uuid),
                                                        bytes.len()
                                                    );
                                                    if !bytes.is_empty() {
                                                        let shared: Arc<[u8]> =
                                                            Arc::from(bytes.into_boxed_slice());
                                                        let new_depth = crate::increment_queue_depth(
                                                            reader_depth.as_ref(),
                                                        );
                                                        match reader_tx.try_send(shared) {
                                                            Ok(()) => {
                                                                if new_depth
                                                                    >= REPORT_QUEUE_WARN_THRESHOLD
                                                                    && !reader_warned.swap(
                                                                        true,
                                                                        Ordering::Relaxed,
                                                                    )
                                                                {
                                                                    log::warn!(
                                                                        "report queue backlog for {:?} reached {} (threshold {})",
                                                                        uuid::Uuid::from_u128(uuid),
                                                                        new_depth,
                                                                        REPORT_QUEUE_WARN_THRESHOLD
                                                                    );
                                                                }
                                                            }
                                                            Err(TrySendError::Full(_)) => {
                                                                crate::decrement_queue_depth(
                                                                    reader_depth.as_ref(),
                                                                );
                                                                log::warn!(
                                                                    "report queue for {:?} full ({} cap), dropping packet",
                                                                    uuid::Uuid::from_u128(uuid),
                                                                    REPORT_QUEUE_CAP
                                                                );
                                                            }
                                                            Err(TrySendError::Disconnected(_)) => {
                                                                crate::decrement_queue_depth(
                                                                    reader_depth.as_ref(),
                                                                );
                                                                log::debug!(
                                                                    "report channel for {:?} disconnected; reader exiting",
                                                                    uuid::Uuid::from_u128(uuid)
                                                                );
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            log::debug!("Report reader for {:?} takeInputReport returned non-object", uuid::Uuid::from_u128(uuid));
                                        }
                                    }
                                    Err(e) => {
                                        log::debug!(
                                            "Report reader for {:?} failed to call takeInputReport: {:?}",
                                            uuid::Uuid::from_u128(uuid),
                                            e
                                        );
                                        if let Ok(has_exc) = env.exception_check() {
                                            if has_exc {
                                                let _ = env.exception_describe();
                                                let _ = env.exception_clear();
                                            }
                                        }
                                        std::thread::sleep(std::time::Duration::from_millis(10));
                                    }
                                }
                            }
                        } else {
                            log::debug!("Failed to load bridge class in report reader");
                        }
                    } else {
                        log::debug!("Failed to attach_current_thread in report reader");
                    }
                } else {
                    log::debug!("Failed to get JNI env/context in report reader");
                }
            }
        }
        log::debug!(
            "Report reader for {:?} exiting",
            uuid::Uuid::from_u128(uuid)
        );
        if let Ok((vm, ctx)) = get_vm_and_context() {
            if let Ok(mut env) = vm.attach_current_thread() {
                if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                    if let Ok(local_class_stop) = env.new_local_ref(gclass.as_obj()) {
                        let jclass_stop: JClass = JClass::from(local_class_stop);
                        if let Ok(juuid) = env.new_string(uuid::Uuid::from_u128(uuid).to_string()) {
                            let juuid_obj: JObject = juuid.into();
                            if let Err(e) = env.call_static_method(
                                jclass_stop,
                                "stopInputListener",
                                "(Ljava/lang/String;)V",
                                &[JValue::Object(&juuid_obj)],
                            ) {
                                log::warn!("JNI stopInputListener({uuid:032x}) failed: {e:?}");
                            }
                        }
                    }
                }
            }
        }
        if let Ok(mut readers) = REPORT_READERS.write() {
            readers.remove(&uuid);
        } else {
            log::debug!(
                "Report reader for {:?} could not clear readers set",
                uuid::Uuid::from_u128(uuid)
            );
        }
    });

    Ok(())
}

pub(crate) fn unregister_report_listener(uuid: u128, id: SubscriptionId) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::unregister_report_listener");
    let mut should_stop = false;
    {
        let mut map = DEVICE_REPORT_LISTENERS
            .write()
            .map_err(|_| HidError::lock_poisoned("DEVICE_REPORT_LISTENERS"))?;
        if let Some(list) = map.get_mut(&uuid) {
            list.remove(&id);
            if list.is_empty() {
                map.remove(&uuid);
                should_stop = true;
            }
        }
    }

    if should_stop {
        if let Ok(mut flags) = REPORT_ABORT_FLAGS.write() {
            if let Some(flag) = flags.get(&uuid) {
                flag.store(true, Ordering::Relaxed);
            }
            flags.remove(&uuid);
        }
        if let Ok(mut readers) = REPORT_READERS.write() {
            readers.remove(&uuid);
        }
        // Drop the master sender so the dispatcher exits once the reader
        // also drops its clone (after observing the abort flag).
        if let Ok(mut channels) = REPORT_CHANNELS.write() {
            channels.remove(&uuid);
        }
    }

    Ok(())
}

// ===== JNI helpers (Android only) =====
fn get_vm_and_context() -> Result<(&'static JavaVM, GlobalRef), String> {
    let _fn_logger = FnLogger::new("android_hid::get_vm_and_context");
    match (ANDROID_JVM.get(), ANDROID_APP_CTX.get()) {
        (Some(vm), Some(ctx)) => Ok((vm, ctx.clone())),
        _ => Err("android context was not initialized".to_string()),
    }
}

#[no_mangle]
pub extern "system" fn Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext(
    env: JNIEnv,
    _class: JObject,
    context: JObject,
) {
    let _fn_logger =
        FnLogger::new("android_hid::Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext");
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Trace),
    );

    log::error!("Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext called");
    if let Ok(vm) = env.get_java_vm() {
        log::error!("Got JavaVM");
        let _ = ANDROID_JVM.set(vm);
    }
    if let Ok(global_ctx) = env.new_global_ref(context) {
        log::error!("Got GlobalRef");
        let _ = ANDROID_APP_CTX.set(global_ctx);
    }
    log::error!("Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext finished");
}

fn call_bridge_is_supported(env: &mut JNIEnv<'_>, context: &GlobalRef) -> Result<bool, String> {
    let _fn_logger = FnLogger::new("android_hid::call_bridge_is_supported");
    log::debug!("call_bridge_is_supported entered");
    let gclass = load_bridge_class(env, context)?;
    let local_class = env
        .new_local_ref(gclass.as_obj())
        .map_err(|e| format!("new_local_ref(class): {e:?}"))?;
    let jclass: JClass = JClass::from(local_class);
    let res = env
        .call_static_method(
            jclass,
            "isSupported",
            "(Landroid/content/Context;)Z",
            &[JValue::Object(context.as_obj())],
        )
        .map_err(|e| format!("call isSupported failed: {e:?}"))?;
    res.z().map_err(|e| format!("to bool: {e:?}"))
}

fn load_bridge_class(env: &mut JNIEnv<'_>, context: &GlobalRef) -> Result<GlobalRef, String> {
    if let Some(clazz) = BRIDGE_CLASS.get() {
        return Ok(clazz.clone());
    }

    let _fn_logger = FnLogger::new("android_hid::load_bridge_class");
    let loader_obj = env
        .call_method(
            context.as_obj(),
            "getClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )
        .map_err(|e| format!("getClassLoader call failed: {e:?}"))?
        .l()
        .map_err(|e| format!("getClassLoader to obj failed: {e:?}"))?;

    let name = env
        .new_string("com.sayodevice.hid_rs.UsbHidBridge")
        .map_err(|e| format!("new_string failed: {e:?}"))?;
    let name_obj: JObject = name.into();
    let clazz_obj = env
        .call_method(
            loader_obj,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name_obj)],
        )
        .map_err(|e| format!("ClassLoader.loadClass failed: {e:?}"))?
        .l()
        .map_err(|e| format!("loadClass to obj failed: {e:?}"))?;

    let global_clazz = env
        .new_global_ref(clazz_obj)
        .map_err(|e| format!("new_global_ref for class failed: {e:?}"))?;

    let _ = BRIDGE_CLASS.set(global_clazz.clone());
    Ok(global_clazz)
}
