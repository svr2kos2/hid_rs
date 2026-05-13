## 项目概览

`hid_rs` 是一个多平台 HID 库（Windows/Linux/macOS + Android + WASM/WebHID），核心抽象在 lib.rs，三个平台后端在 os_hid.rs、android_hid.rs、web_hid.rs。

> **状态标记**：`[DONE]` 已完成、`[SKIP]` 评估后跳过、`[TODO]` 待办、`[PART]` 部分完成。

---

## 一、架构层面（影响最大）

### 1. [SKIP] 平台后端缺少 trait 抽象，重复代码严重
三个 `platform_hid` 通过 `#[path = "..."]` 切换，但它们暴露的是 *自由函数*，没有公共 trait。
**问题**：
- 函数签名不一致风险高（编译期才发现）
- 三个文件各自重新声明全局状态（`DEVICE_LIST`、`DEVICE_CONNECTION_LISTENERS`、`DEVICE_REPORT_LISTENERS`）和 polling 线程
- 监听器注册/注销/通知的逻辑在 os_hid 和 android_hid 中几乎一样

**建议**：抽出一个 `trait PlatformHid`，再把"监听器集合 + 设备增/删 diff 通知 + UUID/SN 映射"做成跨平台共享模块（例如 `src/common/listeners.rs`、`src/common/device_registry.rs`）。三个后端只实现真正平台相关的部分（hidapi/JNI/web-sys）。

**评估结论（跳过）**：后端通过 `#[cfg] #[path]` 在编译期就被锁定为单一模块，从不在运行时切换；lib.rs 已经通过具体的 `platform_hid::xxx()` 调用强制了接口契约。引入 trait 不会带来 dyn dispatch 价值，也无法跨平台捕获契约漂移（每次 `cargo check` 只编译一个 target），只会增加一层间接和三份样板 impl。**对静态 cfg 选择的后端来说，trait 是过度抽象**——共享 listener/registry 这部分仍有价值，可以单独作为内部模块抽出，无需 trait。

### 2. [DONE] 已移除空 `Hid` 包装；公共 API 保留跨平台统一的 `async` 形态
当前 `lib.rs` 已改为直接导出顶层函数（`init()` / `request_device()` / `device_list()` / `on_connection_changed()`）与 `HidDevice`，原先“空 struct + 全关联函数”的问题已经消除。

**评估结论**：`init()` 等公共接口不再继续尝试“去 async 化”。虽然桌面/Android 端有些实现目前是同步完成的，但 WebHID 的 `init()` / `request_device()` / `send_report()` 确实需要真实异步；在 `#[cfg]` 编译期切换的后端模型下，保留统一的异步签名比为同步平台额外分叉 API 更合理。这里的剩余工作不是改成 sync，而是把跨平台语义说明清楚。

### 3. [DONE] `SafeCallback` / `SafeCallback2` 应统一为单个泛型
lib.rs 两个几乎一模一样的结构体，仅差一个参数。
**建议**：定义 `SafeCallback<Args, R>`（用 tuple 作 Args）或者用 trait 别名 + macro，删除一份代码。或干脆引入 `async-trait` 风格 `BoxFuture` 别名。

---

## 二、并发与正确性（潜在 bug）

### 4. [DONE] `call_blocking` 在锁保护下被调用 → 死锁/长时间阻塞风险
os_hid.rs `notify_report_arrive` 拿到 `RwLock` 读锁后 clone listeners 再释放，这一处处理得当；但 os_hid.rs `sub_connection_changed` 在读取 `DEVICE_LIST` 之后、push listener **之前**调用 `callback.call_blocking`——若用户回调里又调用 `Hid::get_device_list()` 会再次锁 `DEVICE_LIST`（虽然是读锁，不致死锁，但若回调发起 `unsub_connection_changed` 会写锁→**死锁**）。Android 端 android_hid.rs 类似。
**建议**：所有外部回调一律在"无锁状态 + 单独线程/任务"中触发；将通知与状态变更分离（先 diff → drop 所有锁 → 再 fire）。

### 5. [DONE] `unsub_connection_changed` 在订阅期间向已订阅者发 `false`，语义可疑
os_hid.rs：取消订阅时，会对所有现有设备调用 `callback(uuid, false)`——即使设备没断开。这容易让上层误以为设备掉线。
**建议**：取消订阅就是取消订阅，不应触发 `false` 通知；或者文档显式声明此语义。

### 6. [DONE] Polling 线程没有退出机制
os_hid.rs 的 `std::thread::spawn(move || loop { ... })` 永不退出，`init` 被多次调用会重复起线程（虽然 `HID_INITIALIZED` 保护了顶层 init，但内部线程对全局状态的依赖意味着库无法卸载/重置）。
**建议**：使用 `AtomicBool` shutdown flag + `JoinHandle`，并提供 `shutdown()` API。

### 7. [DONE] `DEVICE_LIST` 用 `RwLock<HashMap<u128, Mutex<HidDevicePackage>>>`，但操作序列里频繁先 read 再 lock
许多函数（如 `send_report`、`has_report_id`）先 `read()` 后 `lock()`，若另一线程在 `update_device_list` 里 `write()` 删除该项，会先短暂阻塞所有读。设备数量增加时容易抖动。
**建议**：考虑 `dashmap::DashMap<u128, Arc<Mutex<HidDevicePackage>>>`，或者把 device 用 `Arc` 持有，read 完拷贝 Arc 立即释放外层锁。

### 8. [DONE] `reading_thread` 错误处理：`Err` 后 `sleep(500ms)` 再 `break`，且只对特定 report id 移除设备
os_hid.rs 这段逻辑把"哪些子设备掉线后要从顶层 device list 删除"硬编码成 `REPORT_ID_02/0x21/0x22`，这是业务专属规则，不应埋在通用库里。
**建议**：把"何时认定设备彻底失联"做成可配置策略（callback / 规则对象），或抽到上层应用。

---

## 三、API 设计

### 9. [DONE] `HidError` 过于简单，没有 variant
hid_error.rs 只有 `details: String`，调用方无法 match 错误类型。项目已经依赖 `thiserror`，但没用上。
**建议**：
```rust
#[derive(Debug, thiserror::Error)]
pub enum HidError {
    #[error("device not found: {0}")] DeviceNotFound(u128),
    #[error("report id {0} not present")] ReportIdMissing(u8),
    #[error("data too large: max {max}, got {got}")] DataTooLarge { max: usize, got: usize },
    #[error("lock poisoned: {0}")] LockPoisoned(&'static str),
    #[error("hidapi: {0}")] Backend(#[from] hidapi::HidError),
    // ...
}
```
这样能消除散落各处的 `.map_err(|_| HidError::new("Failed to acquire ... lock"))` 噪音。

### 10. [DONE] 桌面端 `request_device` 现在会立即刷新并返回匹配设备
os_hid.rs 先前只是更新 `VID_PID_LIST`，要等下一轮 polling 才能在 `device_list()` 中观察到变化，调用方体验和 Web 端相差太大。

**实际实现**：桌面端 `request_device` 现在会同步完成三件事：
- 更新 VID/PID 过滤器；
- 立即触发一次设备重扫；
- 返回当前符合过滤条件的设备列表。

这仍然不等价于 Web 端“弹权限选择框”的语义，但至少保证调用 `request_device()` 后，后续 `device_list()` 看到的是最新扫描结果，而不是依赖后台 poll 线程稍后才收敛。

### 11. [DONE] `send_report` 修改入参 `Vec<u8>`
os_hid.rs 接收 `&mut Vec<u8>` 并内部 `data.resize(size, 0)`——副作用泄漏。lib.rs 已 `let mut buffer = data;` 拷贝，所以参数其实可以是 `Vec<u8>` by value，或返回写入字节数即可。

### 12. [DONE] 设备 ID 用裸 `u128`
跨 FFI（JNI/JS）来回转 UUID 字符串，类型不安全。
**建议**：包成 `pub struct DeviceId(u128);` 并提供 `Display`/`FromStr`。

### 13. [DONE] 大量 `clone()` 在热路径
- `notify_report_arrive` 每条报告对每个 listener `report.clone()`。
- Android `update_device_list` 风格也类似。
若 listener 较多/报告频率高（HID 通常很高），可改为传 `Arc<Vec<u8>>` 或 `Arc<[u8]>`。

---

## 四、清洁度与小问题

### 14. [DONE] os_hid.rs 顶部有 `use std::thread::sleep;`，又在代码里写 `std::thread::sleep(...)`，风格不一致。
### 15. [DONE] os_hid.rs `HidDevicePackage::abort()` 方法定义了但似乎从未被调用——只有 `abort` 字段直接被 reader 线程读。
### 16. [DONE] `HidReportInfo` 和 `HidReportDescriptor` 已改为 `#[derive(Clone, Debug)]`
### 17. [DONE] os_hid.rs 输出的 `log::debug!("{:?} ...", DEVICE_LIST)` 直接 Debug 打印整个全局 `RwLock`，会触发额外的 fmt 调用且泄漏调试信息——属于历史调试代码，应清理。
### 18. [DONE] `HidDevicePackage.serial_number` 字段保存了但只有写入没读取（写完后通过 `SERIAL_NUMBER_TO_UUID` 反向查询）。可以删。
### 19. [DONE] 几个 `let _ = api.reset_devices();` / `let _ = api.add_devices(...)` 吞掉错误——至少 `log::warn!` 一下。
### 20. [DONE] lib.rs 文件末尾的 `has_report_id` 出错时返回 `false`——把 "lock poisoned" 和 "not present" 当成同样结果，不利于调试。改为返回 `Result<bool, HidError>` 或至少 log warn。
### 21. [DONE] simple_test.rs 与 `send_firmware.js` 放在 src——后者应该挪到 examples 或 `tools/`，源码目录里出现 JS 让人困惑。
### 22. [DONE] Cargo.toml 里 `env_logger` 在两个 target 条件里重复声明（Cargo.toml 和 Cargo.toml），简化为一处。
### 23. [DONE] Android 端到处 `#[cfg(target_os = "android")]` 块嵌套在 `pub fn` 里，且 fallback 路径都是 `Ok(Vec::new())`/`false`。可以把整个 android_hid.rs 文件用 `#[cfg(target_os = "android")]` 包住函数体内容，或者直接拆 `android_hid_real.rs` + `android_hid_stub.rs`。

**实际实现**：发现 lib.rs 已在模块引入处用 `#[cfg(all(not(target_arch = "wasm32"), target_os = "android"))]` 门控了整个 `android_hid.rs`，文件内部所有 `#[cfg(target_os = "android")]` 均为重言陈述、唯一的 `#[cfg(not(target_os = "android"))]` 分支为完全不可达代码。直接在原文件内批量剔除这些冗余 cfg guard（~20+ 处），避免拆文件带来的两套重复函数签名。代码从 807 行减到 781 行，所有 `pub fn` 体内仅保留实际逻辑 + 末尾 `#[allow(unreachable_code)]` fallback（依然保留以避免 unreachable 警告）。
### 24. [DONE] `FnLogger` 的 `enter/exit` 日志噪音很大（719 行里几乎每个 pub fn 都有），生产环境应 `log::trace!` 而非 `debug!`。

---

## 五、测试与构建

### 25. [PART] 测试基础已补齐，但仍可继续扩充
目前已经补上：
- `HidReportDescriptor::from_hid_report` 的原生单元测试；
- `DeviceId` / `uuid::Uuid` / UUID 字符串之间的 round-trip 测试；
- 基础 `DeviceId` / `HidError` 单元测试与桌面/wasm 分平台测试骨架。

**剩余空间**：旧版 `SafeCallback::ptr_eq` 已不存在，因此原建议中的该项已过时；后续更适合继续补订阅生命周期、`request_device()` 语义与更多无硬件路径测试。
### 26. [DONE] 已补上 fmt / clippy / tests 的 CI 检查
已新增 GitHub Actions 工作流，默认执行：
- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace --tests`

同时已对仓库执行一次 `cargo fmt`，并修复了被 `-D warnings` 放大的若干 clippy 问题（包含 `new_without_default`、冗余字段初始化、显式 auto-deref 等），保证这条检查链在当前仓库状态下可实际通过，而不是“只把命令写进 CI”。
### 27. [SKIP] `cdylib + rlib` 组合是平台桥接需要，不必因此拆 workspace
当前仓库的定位仍然是“给 Rust 项目调用的 HID 库”；`cdylib` 的存在主要是为了 Android/JNI 与 wasm 目标产出平台所需动态产物，而不是为了发布通用 C ABI 给外部语言直接调用。

**评估结论（跳过）**：既然当前单 crate 维护成本可接受，就没有必要为了“角色纯化”强行拆成 workspace。后续只有在平台桥接层继续明显膨胀、已经开始拖累 core API 与依赖管理时，再考虑拆成 `core + platform glue` 多 crate 结构。

---

## 建议优先级

| 优先级 | 项目 |
|---|---|
| **高（正确性）** | ~~4~~, ~~6~~, ~~8~~, ~~9~~ |
| **高（架构）** | ~~1~~（跳过）, ~~3~~, ~~23~~ |
| **中（API 清晰度）** | ~~2~~, ~~10~~, ~~11~~, ~~12~~, ~~5~~ |
| **中（性能）** | ~~7~~, ~~13~~ |
| **低（清洁度）** | ~~14~~–~~22~~（除 **16**）, ~~24~~ |
| **基建** | ~~25~~（partial）, ~~26~~, ~~27~~（skip） |

剩余 TODO：**25** 的扩展测试部分。