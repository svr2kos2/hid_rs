#[cfg(not(target_arch = "wasm32"))]
use hidreport::*;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[derive(Clone, Debug)]
pub struct HidReportInfo {
    pub report_id: u8,
    pub size: usize,
    pub usages: Vec<u32>,
}

impl HidReportInfo {
    #[cfg(target_arch = "wasm32")]
    fn from_js_value(value: JsValue) -> Option<Self> {
        use std::cmp::max;
        let report_id = match js_sys::Reflect::get(&value, &JsValue::from_str("reportId")) {
            Ok(id) => id.as_f64().expect("Can not get report id in from_js_value") as u8,
            Err(_) => return None,
        };

        let mut usages_res: Vec<u32> = Vec::new();

        let items = js_sys::Reflect::get(&value, &JsValue::from_str("items"))
            .expect("Can not get items in HidReportInfo::from_js_value");
        let mut report_size = 31;
        for item in js_sys::Array::from(&items).iter() {
            let size = match js_sys::Reflect::get(&item, &JsValue::from_str("reportSize")) {
                Ok(val) => val
                    .as_f64()
                    .expect("Can not get reportSize in HidReportInfo::from_js_value")
                    as u16,
                Err(_) => continue,
            };

            let count = match js_sys::Reflect::get(&item, &JsValue::from_str("reportCount")) {
                Ok(val) => val
                    .as_f64()
                    .expect("Can not get reportCount in HidReportInfo::from_js_value")
                    as u16,
                Err(_) => continue,
            };

            report_size = max(
                report_size,
                count
                    * match size % 8 {
                        0 => size / 8,
                        _ => size / 8 + 1,
                    },
            );

            match js_sys::Reflect::get(&item, &JsValue::from_str("usages")) {
                Ok(usages) => {
                    if !usages.is_array() {
                        continue;
                    };
                    for usage in js_sys::Array::from(&usages) {
                        let usg = match usage.as_f64() {
                            Some(u) => u as u32,
                            None => continue,
                        };
                        usages_res.push(usg);
                    }
                }
                Err(_) => continue,
            }
        }
        if usages_res.len() == 0 {
            return None;
        }

        Some(HidReportInfo {
            report_id: report_id,
            size: report_size as usize + 1,
            usages: usages_res,
        })
    }
    #[cfg(target_arch = "wasm32")]
    fn from_array(reports: &js_sys::Array) -> Vec<Self> {
        let mut vec = Vec::new();
        for item in reports.iter() {
            match Self::from_js_value(item) {
                Some(info) => vec.push(info),
                None => continue,
            }
        }
        vec
    }

    #[cfg(all(not(target_arch = "wasm32"), not(target_os = "android")))]
    fn from_hid_report(report: &impl Report) -> Option<Self> {
        let report_id = match report.report_id() {
            Some(id) => id.into(),
            None => return None,
        };
        let size: usize = report.size_in_bytes();
        let mut usages: Vec<u32> = Vec::new();
        for field in report.fields() {
            for collection in field.collections() {
                for usage in collection.usages() {
                    usages.push(usage.into());
                }
            }
        }
        Some(HidReportInfo {
            report_id,
            size,
            usages,
        })
    }

    #[cfg(target_os = "android")]
    fn from_hid_report(report: &impl Report) -> Option<Self> {
        // If the descriptor doesn't specify a Report ID, HID semantics treat it as 0
        // and no Report ID prefix byte is present in the actual report data.
        let has_report_id = report.report_id().is_some();
        let report_id: u8 = report.report_id().map(|id| id.into()).unwrap_or(0);
        // size_in_bytes() is the payload size without the Report ID byte; add 1 only when present
        let size: usize = report.size_in_bytes() + if has_report_id { 1 } else { 0 };
        let mut usages: Vec<u32> = Vec::new();
        for field in report.fields() {
            for collection in field.collections() {
                for usage in collection.usages() {
                    usages.push(usage.into());
                }
            }
        }
        Some(HidReportInfo {
            report_id: report_id,
            size: size,
            usages: usages,
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn from_array(array: &[impl Report]) -> Vec<Self> {
        let mut vec = Vec::new();
        for item in array {
            match Self::from_hid_report(item) {
                Some(info) => vec.push(info),
                None => continue,
            }
        }
        vec
    }
}

#[derive(Clone, Debug)]
pub struct HidReportDescriptor {
    pub input_reports: Vec<HidReportInfo>,
    pub output_reports: Vec<HidReportInfo>,
    pub feature_reports: Vec<HidReportInfo>,
}

impl Default for HidReportDescriptor {
    fn default() -> Self {
        Self::new()
    }
}

impl HidReportDescriptor {
    pub fn new() -> Self {
        HidReportDescriptor {
            input_reports: Vec::new(),
            output_reports: Vec::new(),
            feature_reports: Vec::new(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn from_js_value(collection: JsValue) -> Option<Self> {
        let input_reports = js_sys::Reflect::get(&collection, &JsValue::from_str("inputReports"))
            .expect("Can not get inputReports in HidReportDescriptor::from_js_value");
        let output_reports = js_sys::Reflect::get(&collection, &JsValue::from_str("outputReports"))
            .expect("Can not get outputReports in HidReportDescriptor::from_js_value");
        let feature_reports =
            js_sys::Reflect::get(&collection, &JsValue::from_str("featureReports"))
                .expect("Can not get featureReports in HidReportDescriptor::from_js_value");

        let input_reports = HidReportInfo::from_array(&js_sys::Array::from(&input_reports));
        let output_reports = HidReportInfo::from_array(&js_sys::Array::from(&output_reports));
        let feature_reports = HidReportInfo::from_array(&js_sys::Array::from(&feature_reports));

        if input_reports.len() == 0 && output_reports.len() == 0 && feature_reports.len() == 0 {
            return None;
        }

        Some(HidReportDescriptor {
            input_reports: input_reports,
            output_reports: output_reports,
            feature_reports: feature_reports,
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_hid_report(report: ReportDescriptor) -> Self {
        let input_reports = report.input_reports();
        let output_reports = report.output_reports();
        let feature_reports = report.feature_reports();

        HidReportDescriptor {
            input_reports: HidReportInfo::from_array(input_reports),
            output_reports: HidReportInfo::from_array(output_reports),
            feature_reports: HidReportInfo::from_array(feature_reports),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::HidReportDescriptor;
    use hidreport::ReportDescriptor;

    #[test]
    fn from_hid_report_collects_input_output_and_feature_reports() {
        let bytes = [
            0x06, 0x00, 0xff, // Usage Page (Vendor Defined)
            0x09, 0x01, // Usage
            0xa1, 0x01, // Collection (Application)
            0x85, 0x02, //   Report ID (2)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x40, //   Report Count (64)
            0x09, 0x01, //   Usage
            0x81, 0x02, //   Input (Data,Var,Abs)
            0x85, 0x21, //   Report ID (33)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x20, //   Report Count (32)
            0x09, 0x02, //   Usage
            0x91, 0x02, //   Output (Data,Var,Abs)
            0x85, 0x22, //   Report ID (34)
            0x75, 0x08, //   Report Size (8)
            0x95, 0x10, //   Report Count (16)
            0x09, 0x03, //   Usage
            0xb1, 0x02, //   Feature (Data,Var,Abs)
            0xc0, // End Collection
        ];

        let report = ReportDescriptor::try_from(bytes.as_slice()).expect("descriptor should parse");
        let descriptor = HidReportDescriptor::from_hid_report(report);

        assert_eq!(descriptor.input_reports.len(), 1);
        assert_eq!(descriptor.output_reports.len(), 1);
        assert_eq!(descriptor.feature_reports.len(), 1);

        let input = &descriptor.input_reports[0];
        assert_eq!(input.report_id, 0x02);
        assert_eq!(input.size, 65);
        assert_eq!(input.usages.len(), 64);
        assert!(input.usages.iter().all(|&usage| usage >> 16 == 0xff00));

        let output = &descriptor.output_reports[0];
        assert_eq!(output.report_id, 0x21);
        assert_eq!(output.size, 33);
        assert_eq!(output.usages.len(), 32);
        assert!(output.usages.iter().all(|&usage| usage >> 16 == 0xff00));

        let feature = &descriptor.feature_reports[0];
        assert_eq!(feature.report_id, 0x22);
        assert_eq!(feature.size, 17);
        assert_eq!(feature.usages.len(), 16);
        assert!(feature.usages.iter().all(|&usage| usage >> 16 == 0xff00));
    }
}
