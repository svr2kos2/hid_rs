use std::fmt;
use std::error::Error;

// Define a custom error type
#[derive(Debug)]
pub struct HidError {
    pub details: String,
}

impl HidError {
    pub fn new(msg: &str) -> HidError {
        HidError { details: msg.to_string() }
    }
}

impl fmt::Display for HidError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.details)
    }
}

impl Error for HidError {
    fn description(&self) -> &str {
        &self.details
    }
}
