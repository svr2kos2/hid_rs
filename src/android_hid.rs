use crate::hid_error::HidError;
use crate::hid_report_descriptor::HidReportDescriptor;
use crate::{SafeCallback, SafeCallback2};
use std::collections::{HashMap, HashSet};
use std::sync::{RwLock, Arc, atomic::{AtomicBool, Ordering}};

#[cfg(target_os = "android")]
use jni::{objects::{GlobalRef, JObject, JString, JValue, JClass, JByteArray}, JNIEnv, JavaVM};
#[cfg(target_os = "android")]
use once_cell::sync::{OnceCell, Lazy};

#[cfg(target_os = "android")]
static ANDROID_APP_CTX: OnceCell<GlobalRef> = OnceCell::new();
#[cfg(target_os = "android")]
static ANDROID_JVM: OnceCell<JavaVM> = OnceCell::new();
#[cfg(target_os = "android")]
static BRIDGE_CLASS: OnceCell<GlobalRef> = OnceCell::new();

// ===== Android-specific connection change tracking =====
static DEVICE_SET: Lazy<RwLock<HashSet<u128>>> = Lazy::new(|| RwLock::new(HashSet::new())) ;
static DEVICE_CONNECTION_LISTENERS: Lazy<RwLock<Vec<SafeCallback2<u128, bool>>>> = Lazy::new(|| RwLock::new(Vec::new()));
static POLLER_RUNNING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));
static POLLER_ABORT: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

// Per-device report listeners and background readers
static DEVICE_REPORT_LISTENERS: Lazy<RwLock<HashMap<u128, Vec<SafeCallback2<u128, Vec<u8>>>>>> = Lazy::new(|| RwLock::new(HashMap::new()));
static REPORT_READERS: Lazy<RwLock<HashSet<u128>>> = Lazy::new(|| RwLock::new(HashSet::new()));
static REPORT_ABORT_FLAGS: Lazy<RwLock<HashMap<u128, Arc<AtomicBool>>>> = Lazy::new(|| RwLock::new(HashMap::new()));

fn log_enter(name: &str) {
    log::debug!("ENTER {}", name);
}

fn log_exit(name: &str) {
    log::debug!("EXIT {}", name);
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

pub(in crate) fn is_supported() -> bool {
    let _fn_logger = FnLogger::new("android_hid::is_supported");
    #[cfg(target_os = "android")]
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

pub(in crate) async fn init() -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::init");
    log::debug!("android_hid init placeholder");
    Ok(())
}

pub(in crate) async fn request_device(vendor_ids: Vec<(u16, Option<u16>)>) -> Result<Vec<u128>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::request_device");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let json = serde_json::to_string(&vendor_ids.iter().map(|(v,p)| {
            serde_json::json!({"vendorId": v, "productId": p})
        }).collect::<Vec<_>>()).map_err(|e| HidError::new(&format!("json: {e}")))?;
        let jfilters = env.new_string(json).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let jctx = ctx.as_obj();
        let jfilters_obj: JObject = jfilters.into();
        let args = vec![JValue::Object(&jctx), JValue::Object(&jfilters_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
        let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let arr = env
            .call_static_method(
                jclass,
                "requestDevicesJson",
                "(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call requestDevicesJson: {e:?}")))?;
        let jstr: JString = arr.l().map_err(|e| HidError::new(&format!("to obj: {e:?}")))?.into();
        let rust_str = env.get_string(&jstr).map_err(|e| HidError::new(&format!("get_string: {e:?}")))?;
        let uuids: Vec<String> = serde_json::from_str(rust_str.to_str().unwrap_or("[]")).unwrap_or_default();
        let ids = uuids.into_iter().filter_map(|s| uuid::Uuid::try_parse(&s).ok().map(|u| u.as_u128())).collect();
        return Ok(ids);
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

pub(in crate) fn get_device_list() -> Result<Vec<u128>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_device_list");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let jctx = ctx.as_obj();
        let args = vec![JValue::Object(&jctx)];
    let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
    let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
    let jclass: JClass = JClass::from(local_class);
        let arr = env
            .call_static_method(
                jclass,
                "getDeviceListJson",
                "(Landroid/content/Context;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call getDeviceListJson: {e:?}")))?;
        let jstr: JString = arr.l().map_err(|e| HidError::new(&format!("to obj: {e:?}")))?.into();
        let rust_str = env.get_string(&jstr).map_err(|e| HidError::new(&format!("get_string: {e:?}")))?;
        let uuids: Vec<String> = serde_json::from_str(rust_str.to_str().unwrap_or("[]")).unwrap_or_default();
        let ids = uuids.into_iter().filter_map(|s| uuid::Uuid::try_parse(&s).ok().map(|u| u.as_u128())).collect();
        return Ok(ids);
    }
    #[allow(unreachable_code)]
    Ok(Vec::new())
}

pub(in crate) async fn sub_connection_changed(_callback: SafeCallback2<u128, bool>) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::sub_connection_changed");
    let callback = _callback;

    let current_ids = get_device_list().unwrap_or_default();
    for id in &current_ids { callback.call_blocking(*id, true); }
    
    {
        let mut listeners = DEVICE_CONNECTION_LISTENERS
            .write()
            .map_err(|_| HidError::new("Failed to acquire connection listeners lock"))?;
        listeners.push(callback);
    }

    let need_start = !POLLER_RUNNING.swap(true, Ordering::SeqCst);
    if need_start {
        POLLER_ABORT.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = DEVICE_SET.write() { 
            guard.clear();
            guard.extend(current_ids.into_iter());
        }
        std::thread::spawn(move || {
            #[cfg(target_os = "android")]
            let _attach_guard = if let Ok((vm, _)) = get_vm_and_context() {
                vm.attach_current_thread().ok()
            } else {
                None
            };

            loop {
                if POLLER_ABORT.load(Ordering::Relaxed) { break; }
                let new_list = get_device_list().unwrap_or_default();
                let new_set: HashSet<u128> = new_list.into_iter().collect();

                let (added, removed) = {
                    let mut added = Vec::new();
                    let mut removed = Vec::new();
                    if let Ok(mut cur) = DEVICE_SET.write() {
                        for id in cur.iter() {
                            if !new_set.contains(id) { removed.push(*id); }
                        }
                        for id in new_set.iter() {
                            if !cur.contains(id) { added.push(*id); }
                        }
                        *cur = new_set;
                    }
                    (added, removed)
                };

                if !added.is_empty() || !removed.is_empty() {
                    if let Ok(listeners) = DEVICE_CONNECTION_LISTENERS.read() {
                        for id in added.iter() {
                            log::debug!("Device connected: {}", uuid::Uuid::from_u128(*id));
                            for cb in listeners.iter() { 
                                log::debug!("Calling listener for connected device: {}", uuid::Uuid::from_u128(*id));
                                cb.call_blocking(*id, true); 
                            }
                        }
                        for id in removed.iter() {
                            log::debug!("Device disconnected: {}", uuid::Uuid::from_u128(*id));
                            for cb in listeners.iter() { 
                                log::debug!("Calling listener for disconnected device: {}", uuid::Uuid::from_u128(*id));
                                cb.call_blocking(*id, false); 
                            }
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

pub(in crate) async fn unsub_connection_changed(_callback: SafeCallback2<u128, bool>) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::unsub_connection_changed");
    let callback = _callback;

    let snapshot: Vec<u128> = {
        match DEVICE_SET.read() { Ok(s) => s.iter().cloned().collect(), Err(_) => Vec::new() }
    };
    for id in snapshot { callback.call_blocking(id, false); }

    {
        let mut listeners = DEVICE_CONNECTION_LISTENERS
            .write()
            .map_err(|_| HidError::new("Failed to acquire connection listeners lock"))?;
        if let Some(pos) = listeners.iter().position(|x| x.ptr_eq(&callback)) { listeners.remove(pos); }
        if listeners.is_empty() {
            POLLER_ABORT.store(true, Ordering::SeqCst);
        }
    }

    Ok(())
}

pub(in crate) fn available(uuid: u128) -> bool {
    let _fn_logger = FnLogger::new("android_hid::available");
    #[cfg(target_os = "android")]
    {
        if let Ok((vm, ctx)) = get_vm_and_context() {
            if let Ok(mut env) = vm.attach_current_thread() {
                if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                    if let Ok(local_class) = env.new_local_ref(gclass.as_obj()) {
                        let jclass: JClass = JClass::from(local_class);
                        let juuid = match env.new_string(uuid::Uuid::from_u128(uuid).to_string()) { Ok(s) => s, Err(_) => return false };
                        let juuid_obj: JObject = juuid.into();
                        let args = vec![JValue::Object(&ctx.as_obj()), JValue::Object(&juuid_obj)];
                        if let Ok(val) = env.call_static_method(
                            jclass,
                            "available",
                            "(Landroid/content/Context;Ljava/lang/String;)Z",
                            &args,
                        ) { return val.z().unwrap_or(false); }
                    }
                }
            }
        }
    }
    #[allow(unreachable_code)]
    false
}

pub(in crate) fn vid(uuid: u128) -> Result<u16, HidError> {
    let _fn_logger = FnLogger::new("android_hid::vid");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let juuid = env.new_string(uuid::Uuid::from_u128(uuid).to_string()).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
    let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
    let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
    let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(
                jclass,
                "getVid",
                "(Ljava/lang/String;)I",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call getVid: {e:?}")))?;
        let vid = val.i().map_err(|e| HidError::new(&format!("to int: {e:?}")))?;
        if vid >= 0 { return Ok(vid as u16); }
        return Err(HidError::new("device not found"));
    }
    #[allow(unreachable_code)]
    Err(HidError::new("not implemented"))
}

pub(in crate) fn pid(uuid: u128) -> Result<u16, HidError> {
    let _fn_logger = FnLogger::new("android_hid::pid");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let juuid = env.new_string(uuid::Uuid::from_u128(uuid).to_string()).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
        let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(
                jclass,
                "getPid",
                "(Ljava/lang/String;)I",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call getPid: {e:?}")))?;
        let pid = val.i().map_err(|e| HidError::new(&format!("to int: {e:?}")))?;
        if pid >= 0 { return Ok(pid as u16); }
        return Err(HidError::new("device not found"));
    }
    #[allow(unreachable_code)]
    Err(HidError::new("not implemented"))
}

pub(in crate) fn get_product_name(uuid: u128) -> Result<Option<String>, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_product_name");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let juuid = env.new_string(uuid::Uuid::from_u128(uuid).to_string()).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&juuid_obj)];
    let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
    let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
    let jclass: JClass = JClass::from(local_class);
        let val = env
            .call_static_method(
                jclass,
                "getProductName",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call getProductName: {e:?}")))?;
        let s_obj = val.l().map_err(|e| HidError::new(&format!("to obj: {e:?}")))?;
        if s_obj.is_null() { return Ok(None); }
        let jstr: JString = s_obj.into();
        let rust_str = env.get_string(&jstr).map_err(|e| HidError::new(&format!("get_string: {e:?}")))?;
        return Ok(Some(rust_str.to_str().unwrap_or("").to_string()));
    }
    #[allow(unreachable_code)]
    Ok(None)
}

pub(in crate) fn get_collections(uuid: u128) -> Result<HidReportDescriptor, HidError> {
    let _fn_logger = FnLogger::new("android_hid::get_collections");
    #[cfg(target_os = "android")]
    {
        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
        let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let juuid = env.new_string(uuid::Uuid::from_u128(uuid).to_string()).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let args = vec![JValue::Object(&ctx.as_obj()), JValue::Object(&juuid_obj), JValue::Int(1024)];
        let val = env
            .call_static_method(
                jclass,
                "getReportDescriptor",
                "(Landroid/content/Context;Ljava/lang/String;I)[B",
                &args,
            )
            .map_err(|e| HidError::new(&format!("call getReportDescriptor: {e:?}")))?;
        let obj = val.l().map_err(|e| HidError::new(&format!("to obj: {e:?}")))?;
        if obj.is_null() { return Err(HidError::new("No report descriptor")); }
        let jarr: JByteArray = JByteArray::from(obj);
        let bytes = env.convert_byte_array(&jarr).map_err(|e| HidError::new(&format!("convert_byte_array: {e:?}")))?;

        #[cfg(not(target_arch = "wasm32"))]
        {
            let report = hidreport::ReportDescriptor::try_from(bytes.as_slice())
                .map_err(|_| HidError::new("Failed to parse report descriptor"))?;
            let res = crate::hid_report_descriptor::HidReportDescriptor::from_hid_report(report);

            return Ok(res);
        }
        #[cfg(target_arch = "wasm32")]
        {
            return Err(HidError::new("not supported on wasm"));
        }
    }
    #[allow(unreachable_code)]
    Err(HidError::new("not implemented"))
}

pub(in crate) fn has_report_id(uuid: u128, report_id: u8) -> Result<bool, HidError> {
    let _fn_logger = FnLogger::new("android_hid::has_report_id");
    log::debug!("has_report_id called in android_hid {:02X?}", report_id);
    let desc = get_collections(uuid)?;

    let found = desc.input_reports.iter().any(|r| r.report_id == report_id)
        || desc.output_reports.iter().any(|r| r.report_id == report_id)
        || desc.feature_reports.iter().any(|r| r.report_id == report_id);
    Ok(found)
}

pub(in crate) async fn send_report(uuid: u128, data: &mut Vec<u8>) -> Result<usize, HidError> {
    let _fn_logger = FnLogger::new("android_hid::send_report");
    #[cfg(target_os = "android")]
    {
        if data.is_empty() { return Err(HidError::new("Data cannot be empty")); }
        let report_id = data[0];
        let desc = get_collections(uuid)?;
        let size_opt = desc
            .output_reports
            .iter()
            .find(|r| r.report_id == report_id)
            .map(|r| r.size)
            .or_else(|| {
                desc.input_reports.iter().find(|r| r.report_id == report_id).map(|r| r.size)
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
            return Err(HidError::new(&format!(
                "Data size too large for report {:02X} max: {}, got: {}",
                report_id, size, data.len()
            )));
        }
        let to_send: Vec<u8> = if data.len() < size {
            let mut v = vec![0u8; size];
            v[..data.len()].copy_from_slice(&data[..]);
            v
        } else {
            data.clone()
        };

        let (vm, ctx) = get_vm_and_context().map_err(|e| HidError::new(&e))?;
        let mut env = vm.attach_current_thread().map_err(|e| HidError::new(&format!("attach_current_thread: {e:?}")))?;
        let gclass = load_bridge_class(&mut env, &ctx).map_err(|e| HidError::new(&e))?;
        let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| HidError::new(&format!("new_local_ref(class): {e:?}")))?;
        let jclass: JClass = JClass::from(local_class);
        let juuid = env.new_string(uuid::Uuid::from_u128(uuid).to_string()).map_err(|e| HidError::new(&format!("new_string: {e:?}")))?;
        let juuid_obj: JObject = juuid.into();
        let jdata = env.byte_array_from_slice(&to_send[..]).map_err(|e| HidError::new(&format!("byte_array_from_slice: {e:?}")))?;
        let jbytearr: JObject = JObject::from(jdata);
        let args = vec![JValue::Object(&ctx.as_obj()), JValue::Object(&juuid_obj), JValue::Object(&jbytearr)];
        let res = env.call_static_method(
            jclass,
            "sendOutputReport",
            "(Landroid/content/Context;Ljava/lang/String;[B)I",
            &args,
        ).map_err(|e| HidError::new(&format!("call sendOutputReport: {e:?}")))?;
        let wrote = res.i().map_err(|e| HidError::new(&format!("to int: {e:?}")))?;
        if wrote < 0 { return Err(HidError::new("send failed")); }
        return Ok(wrote as usize);
    }
    #[allow(unreachable_code)]
    Err(HidError::new("not implemented"))
}

pub(in crate) async fn send_firmware(
    _uuid: u128,
    _firmware: &mut Vec<u8>,
    _write_data_cmd:u8,
    _size_addr: u8,
    _big_endian: u8,
    _err_for_size: u8,
    _encrypt: u8,
    _check_sum: u8,
    _on_progress: SafeCallback<f64>,
) -> Result<usize, HidError> {
    let _fn_logger = FnLogger::new("android_hid::send_firmware");
    Err(HidError::new("not implemented"))
}

pub(in crate) async fn sub_report_arrive(uuid: u128, callback: SafeCallback2<u128, Vec<u8>>) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::sub_report_arrive");
    log::debug!("sub_report_arrive called for {:?}", uuid::Uuid::from_u128(uuid));
    {
        let mut map = DEVICE_REPORT_LISTENERS
            .write()
            .map_err(|_| HidError::new("Failed to acquire report listeners lock"))?;
        let entry = map.entry(uuid).or_insert_with(Vec::new);
        entry.push(callback);
    }

    {
        if let Ok(readers) = REPORT_READERS.read() {
            if readers.contains(&uuid) { return Ok(()); }
        }
    }

    let abort_flag = {
        let mut aborts = REPORT_ABORT_FLAGS
            .write()
            .map_err(|_| HidError::new("Failed to acquire abort flags lock"))?;
        aborts.entry(uuid).or_insert_with(|| Arc::new(AtomicBool::new(false))).clone()
    };

    abort_flag.store(false, Ordering::SeqCst);

    {
        let mut readers_guard = REPORT_READERS
            .write()
            .map_err(|_| HidError::new("Failed to acquire readers lock"))?;
        if readers_guard.contains(&uuid) {
            log::debug!("reader for {:?} already exists, not spawning", uuid::Uuid::from_u128(uuid));
            return Ok(());
        }
        readers_guard.insert(uuid);
        log::debug!("spawning report reader for {:?}", uuid::Uuid::from_u128(uuid));
    }

    let _ = std::thread::spawn(move || {
        log::debug!("Report reader for {:?} started", uuid::Uuid::from_u128(uuid));
        loop {
            if abort_flag.load(Ordering::Relaxed) { 
                log::debug!("Report reader for {:?} aborting", uuid::Uuid::from_u128(uuid));
                break; 
            }
            #[cfg(target_os = "android")]
            {
                if let Ok((vm, ctx)) = get_vm_and_context() {
                    if let Ok(mut env) = vm.attach_current_thread() {
                        if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                            if let Ok(local_class_start) = env.new_local_ref(gclass.as_obj()) {
                                let jclass_start: JClass = JClass::from(local_class_start);
                                let juuid = match env.new_string(uuid::Uuid::from_u128(uuid).to_string()) { Ok(s) => s, Err(_) => { std::thread::sleep(std::time::Duration::from_millis(200)); continue; } };
                                let juuid_obj: JObject = juuid.into();
                                let start_args = vec![
                                    JValue::Object(&ctx.as_obj()),
                                    JValue::Object(&juuid_obj),
                                    JValue::Int(1024),
                                ];
                                let _ = env.call_static_method(
                                    jclass_start,
                                    "startInputListener",
                                    "(Landroid/content/Context;Ljava/lang/String;I)Z",
                                    &start_args,
                                );

                                let local_class_take = match env.new_local_ref(gclass.as_obj()) { Ok(c) => c, Err(_) => { std::thread::sleep(std::time::Duration::from_millis(10)); continue; } };
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
                                                        if let Ok(listeners_map) = DEVICE_REPORT_LISTENERS.read() {
                                                            if let Some(list) = listeners_map.get(&uuid) {
                                                                for cb in list.iter() { cb.call_blocking(uuid, bytes.clone()); }
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
            #[cfg(not(target_os = "android"))]
            {
                break; 
            }
        }
        log::debug!("Report reader for {:?} exiting", uuid::Uuid::from_u128(uuid));
        #[cfg(target_os = "android")]
        if let Ok((vm, ctx)) = get_vm_and_context() {
            if let Ok(mut env) = vm.attach_current_thread() {
                if let Ok(gclass) = load_bridge_class(&mut env, &ctx) {
                    if let Ok(local_class_stop) = env.new_local_ref(gclass.as_obj()) {
                        let jclass_stop: JClass = JClass::from(local_class_stop);
                        if let Ok(juuid) = env.new_string(uuid::Uuid::from_u128(uuid).to_string()) {
                            let juuid_obj: JObject = juuid.into();
                            let _ = env.call_static_method(
                                jclass_stop,
                                "stopInputListener",
                                "(Ljava/lang/String;)V",
                                &[JValue::Object(&juuid_obj)],
                            );
                        }
                    }
                }
            }
        }
        if let Ok(mut readers) = REPORT_READERS.write() {
            readers.remove(&uuid);
        } else {
            log::debug!("Report reader for {:?} could not clear readers set", uuid::Uuid::from_u128(uuid));
        }
    });

    Ok(())
}

pub(in crate) async fn unsub_report_arrive(uuid: u128, callback: SafeCallback2<u128, Vec<u8>>) -> Result<(), HidError> {
    let _fn_logger = FnLogger::new("android_hid::unsub_report_arrive");
    let mut should_stop = false;
    {
        let mut map = DEVICE_REPORT_LISTENERS
            .write()
            .map_err(|_| HidError::new("Failed to acquire report listeners lock"))?;
        if let Some(list) = map.get_mut(&uuid) {
            if let Some(pos) = list.iter().position(|x| x.ptr_eq(&callback)) { list.remove(pos); }
            if list.is_empty() { map.remove(&uuid); should_stop = true; }
        }
    }

    if should_stop {
        if let Ok(mut flags) = REPORT_ABORT_FLAGS.write() {
            if let Some(flag) = flags.get(&uuid) { flag.store(true, Ordering::Relaxed); }
            flags.remove(&uuid);
        }
        if let Ok(mut readers) = REPORT_READERS.write() {
            readers.remove(&uuid);
        }
    }

    Ok(())
}

// ===== JNI helpers (Android only) =====
#[cfg(target_os = "android")]
fn get_vm_and_context() -> Result<(&'static JavaVM, GlobalRef), String> {
    let _fn_logger = FnLogger::new("android_hid::get_vm_and_context");
    match (ANDROID_JVM.get(), ANDROID_APP_CTX.get()) {
        (Some(vm), Some(ctx)) => Ok((vm, ctx.clone())),
        _ => Err("android context was not initialized".to_string()),
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext(
    env: JNIEnv,
    _class: JObject,
    context: JObject,
) {
    let _fn_logger = FnLogger::new("android_hid::Java_com_sayodevice_hid_1rs_HidInit_initAndroidContext");
    #[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
fn call_bridge_is_supported(env: &mut JNIEnv<'_>, context: &GlobalRef) -> Result<bool, String> {
    let _fn_logger = FnLogger::new("android_hid::call_bridge_is_supported");
    log::debug!("call_bridge_is_supported entered");
    let gclass = load_bridge_class(env, context)?;
    let local_class = env.new_local_ref(gclass.as_obj()).map_err(|e| format!("new_local_ref(class): {e:?}"))?;
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

#[cfg(target_os = "android")]
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
    
    let global_clazz = env.new_global_ref(clazz_obj)
        .map_err(|e| format!("new_global_ref for class failed: {e:?}"))?;
    
    let _ = BRIDGE_CLASS.set(global_clazz.clone());
    Ok(global_clazz)
}
