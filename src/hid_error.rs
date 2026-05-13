use thiserror::Error;

/// Errors returned by the HID library.
///
/// Most variants carry enough context for callers to react programmatically
/// (e.g. retry, surface to UI). Backend / FFI failures that don't map to a
/// specific variant are captured in [`HidError::Other`] via [`HidError::new`].
#[derive(Debug, Error)]
pub enum HidError {
    /// The requested device is not present in the backend's device list.
    #[error("device not found: {0:032x}")]
    DeviceNotFound(u128),

    /// The device exists but does not expose the requested HID report ID.
    #[error("report id {report_id:#04x} not present on device {uuid:032x}")]
    ReportIdMissing { uuid: u128, report_id: u8 },

    /// The payload supplied to `send_report` exceeds the report size declared
    /// by the device descriptor.
    #[error("data too large for report: max {max} bytes, got {got}")]
    DataTooLarge { max: usize, got: usize },

    /// `send_report` was called with an empty buffer.
    #[error("report data cannot be empty")]
    EmptyData,

    /// The HID backend has not been initialized yet (call [`crate::init`]).
    #[error("HID backend not initialized")]
    NotInitialized,

    /// The current platform/runtime does not support HID.
    #[error("HID is not supported on this platform")]
    NotSupported,

    /// The requested operation is not implemented for the current platform.
    #[error("not implemented on this platform")]
    NotImplemented,

    /// A global synchronization primitive was poisoned. The static `&str`
    /// identifies the lock for diagnostics.
    #[error("HID backend lock poisoned: {0}")]
    LockPoisoned(&'static str),

    /// I/O failure reported by the underlying backend (hidapi / WebHID / JNI).
    #[error("HID I/O error: {0}")]
    Io(String),

    /// Catch-all for backend / FFI errors that don't map to a specific variant.
    #[error("{0}")]
    Other(String),
}

impl HidError {
    /// Backwards-compatible constructor producing [`HidError::Other`].
    ///
    /// Prefer the dedicated variants (`DeviceNotFound`, `DataTooLarge`, ...)
    /// when the failure mode is known.
    pub fn new(msg: &str) -> HidError {
        HidError::Other(msg.to_string())
    }

    /// Convenience constructor used at every `RwLock` / `Mutex` `.map_err` site.
    pub fn lock_poisoned(which: &'static str) -> HidError {
        HidError::LockPoisoned(which)
    }
}
