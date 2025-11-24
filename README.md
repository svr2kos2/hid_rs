# hid_rs

A cross-platform Rust HID (Human Interface Device) library with support for Windows, Linux, macOS, Android, and Web (WASM).

## Features

- **Cross-Platform**: Unified API for desktop, mobile, and web.
- **Async Support**: Built with async/await in mind.
- **Android Support**: Includes a complete Android library module with JNI bindings and Kotlin helpers.
- **Web Support**: Uses `web-sys` for WebHID API support.

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
hid_rs = { git = "https://github.com/svr2kos2/hid_rs.git" }
```

## Usage

### General Rust Usage

```rust
// Example usage (pseudo-code)
use hid_rs::HidDevice;

async fn example() {
    let devices = hid_rs::get_device_list().await.unwrap();
    for device in devices {
        println!("Found device: {:?}", device);
    }
}
```

### Android Integration

This library is designed to be used as an Android Library Module. It includes the necessary Kotlin bridge code and JNI bindings.

#### 1. Add to `settings.gradle`

Include the `hid_rs` project in your Android project's `settings.gradle`. You can use a Git submodule or clone it locally.

```gradle
// settings.gradle
include ':hid_rs'
project(':hid_rs').projectDir = new File('path/to/hid_rs/android')
```

#### 2. Add Dependency

Add the dependency to your app's `build.gradle.kts`:

```kotlin
// app/build.gradle.kts
dependencies {
    implementation(project(":hid_rs"))
}
```

#### 3. Initialize in Application/Activity

You must initialize the library with the Android Context before using any HID functions. This loads the native library and sets up the JNI context.

```kotlin
import com.sayodevice.hid_rs.HidInit

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        
        // Initialize hid_rs
        HidInit.init(this)
    }
}
```

#### 4. Permissions

The library automatically includes the necessary `AndroidManifest.xml` entries.
It handles USB permissions using `UsbPermissionReceiver` which listens for `com.sayodevice.hid_rs.USB_PERMISSION`.

Ensure your app requests permissions if needed, though the library handles the USB permission intent flow.

### Web (WASM)

When compiling for `wasm32-unknown-unknown`, the library uses the browser's WebHID API.
**Note**: WebHID is considered an unstable API in `web-sys`, so you must enable the `web_sys_unstable_apis` config flag.

You can do this by setting the `RUSTFLAGS` environment variable:

```bash
RUSTFLAGS=--cfg=web_sys_unstable_apis cargo build --target wasm32-unknown-unknown
```

Or by adding it to your project's `.cargo/config.toml`:

```toml
[target.wasm32-unknown-unknown]
rustflags = ["--cfg=web_sys_unstable_apis"]
```

Ensure your site is served over HTTPS (required for WebHID).

### Desktop (Windows/Linux/macOS)

Uses `hidapi` under the hood. No special setup required usually, but on Linux you might need `libudev-dev` and `libusb-1.0-0-dev`.

```bash
sudo apt-get install libudev-dev libusb-1.0-0-dev
```

## License

[MIT](LICENSE)
