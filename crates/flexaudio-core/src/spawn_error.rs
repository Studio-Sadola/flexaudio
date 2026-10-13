//! Safe shared projection for worker creation failures.
use crate::{Error, ErrorContext, Operation};

/// Keep the I/O classification and numeric OS cause without exposing paths or payloads.
pub fn spawn_error(call: &'static str, error: std::io::Error) -> Error {
    Error::Backend(format!("{call} worker creation failed (I/O kind {:?}, OS code {:?}); release system resources and retry", error.kind(), error.raw_os_error()))
        .with_context(ErrorContext::new(Operation::Start))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spawn_failure_retains_kind_and_os_code_without_private_payload() {
        let error = spawn_error("capture", std::io::Error::from_raw_os_error(11));
        assert!(error.to_string().contains("WouldBlock"));
        assert!(error.to_string().contains("Some(11)"));
        let error = spawn_error("capture", std::io::Error::other("private payload"));
        assert!(!error.to_string().contains("private payload"));
    }
}
