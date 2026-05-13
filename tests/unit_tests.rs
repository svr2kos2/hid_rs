//! Pure unit tests — no hardware required, compile and run on all native platforms.
//!
//! Run with:
//!   cargo test --test unit_tests

use hid_rs::{hid_error::HidError, DeviceId};

// ── DeviceId ──────────────────────────────────────────────────────────────────

#[test]
fn device_id_roundtrip() {
    let raw: u128 = 0xDEAD_BEEF_CAFE_1234_5678_9ABC_DEF0_1234;
    let id = DeviceId::new(raw);
    assert_eq!(id.as_u128(), raw);
}

#[test]
fn device_id_equality() {
    let a = DeviceId::new(42);
    let b = DeviceId::new(42);
    let c = DeviceId::new(99);
    assert_eq!(a, b);
    assert_ne!(a, c);
}

#[test]
fn device_id_from_into_u128() {
    let raw: u128 = 12345;
    let id: DeviceId = raw.into();
    assert_eq!(id.as_u128(), raw);
    let back: u128 = id.into();
    assert_eq!(back, raw);
}

#[test]
fn device_id_display_is_32_hex_digits() {
    let id = DeviceId::new(0xABCD);
    let s = format!("{}", id);
    // 128-bit number rendered as zero-padded lowercase hex — always 32 chars.
    assert_eq!(s.len(), 32);
    assert_eq!(s, "0000000000000000000000000000abcd");
}

#[test]
fn device_id_ordering() {
    let a = DeviceId::new(1);
    let b = DeviceId::new(2);
    assert!(a < b);
    assert!(b > a);
    assert!(a <= a);
}

#[test]
fn device_id_copy() {
    let a = DeviceId::new(7);
    let b = a; // Copy
    assert_eq!(a, b);
}

#[test]
fn device_id_uuid_roundtrip() {
    let raw: u128 = 0x1234_5678_9abc_def0_1357_2468_ace0_bdf1;
    let uuid = uuid::Uuid::from_u128(raw);
    assert_eq!(uuid.as_u128(), raw);
    assert_eq!(DeviceId::new(uuid.as_u128()).as_u128(), raw);
}

#[test]
fn uuid_string_roundtrip_matches_device_id() {
    let raw: u128 = 0xfedc_ba98_7654_3210_0123_4567_89ab_cdef;
    let uuid = uuid::Uuid::from_u128(raw);
    let parsed = uuid::Uuid::try_parse(&uuid.to_string()).expect("uuid string should parse");
    assert_eq!(parsed.as_u128(), DeviceId::new(raw).as_u128());
}

// ── HidError ──────────────────────────────────────────────────────────────────

#[test]
fn hid_error_other_constructor() {
    let e = HidError::new("something went wrong");
    let s = format!("{}", e);
    assert!(s.contains("something went wrong"));
}

#[test]
fn hid_error_device_not_found_display() {
    let e = HidError::DeviceNotFound(0x1234);
    let s = format!("{}", e);
    assert!(s.contains("device not found"));
    assert!(s.contains("1234"));
}

#[test]
fn hid_error_report_id_missing_display() {
    let e = HidError::ReportIdMissing {
        uuid: 0xABCD,
        report_id: 0x21,
    };
    let s = format!("{}", e);
    assert!(s.contains("0x21"));
}

#[test]
fn hid_error_data_too_large_display() {
    let e = HidError::DataTooLarge { max: 64, got: 128 };
    let s = format!("{}", e);
    assert!(s.contains("64"));
    assert!(s.contains("128"));
}

#[test]
fn hid_error_empty_data_display() {
    let e = HidError::EmptyData;
    assert!(!format!("{}", e).is_empty());
}

#[test]
fn hid_error_not_initialized_display() {
    let e = HidError::NotInitialized;
    assert!(!format!("{}", e).is_empty());
}

#[test]
fn hid_error_not_supported_display() {
    let e = HidError::NotSupported;
    assert!(!format!("{}", e).is_empty());
}

#[test]
fn hid_error_lock_poisoned_constructor() {
    let e = HidError::lock_poisoned("test_lock");
    let s = format!("{}", e);
    assert!(s.contains("test_lock"));
}

#[test]
fn hid_error_io_display() {
    let e = HidError::Io("read timeout".to_string());
    let s = format!("{}", e);
    assert!(s.contains("read timeout"));
}

#[test]
fn hid_error_is_debug() {
    // Ensure HidError implements Debug (required for ?-operator and unwrap messages)
    let e = HidError::new("debug test");
    let _ = format!("{:?}", e);
}
