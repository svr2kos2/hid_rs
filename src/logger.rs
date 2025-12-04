
/// Initialize the logger for the current platform.
/// 
/// - Android: Logs to logcat with tag "RustHID"
/// - iOS: Logs to OS logging system
/// - WASM: Logs to browser console
/// - Desktop (Windows/Linux/macOS): Logs to stdout/stderr using env_logger
pub fn init() {
    #[cfg(target_os = "android")]
    {
        use log::LevelFilter;
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(LevelFilter::Info)
                .with_tag("RustHID"),
        );
    }

    #[cfg(target_os = "ios")]
    {
        use log::LevelFilter;
        let _ = oslog::OsLogger::new("com.rust.hid")
            .level_filter(LevelFilter::Info)
            .init();
    }

    #[cfg(target_arch = "wasm32")]
    {
        use log::LevelFilter;
        let _ = console_log::init_with_level(LevelFilter::Info);
    }

    #[cfg(not(any(target_os = "android", target_os = "ios", target_arch = "wasm32")))]
    {
        // Only initialize if not already initialized
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .try_init();
    }
}
