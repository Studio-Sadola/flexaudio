//! Check the current OS's three cells without starting capture or requiring devices.
use super::*;

fn unsupported(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::Unsupported | ErrorKind::UnsupportedOsVersion
    )
}

#[test]
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
fn current_os_capture_cells_have_capabilities_and_construction_paths() {
    let snapshot = capabilities();
    for (kind, capability) in [
        (SourceKind::Mic, &snapshot.sources.mic),
        (SourceKind::SystemLoopback, &snapshot.sources.system),
        (SourceKind::ProcessLoopback, &snapshot.sources.process),
    ] {
        let unavailable = match &capability.availability {
            Availability::Unavailable { reason } => match reason {
                // A missing daemon is an environment state, like a missing device.
                UnavailableReason::PipeWireUnavailable => false,
                UnavailableReason::UnsupportedOs | UnavailableReason::NotApplicable => true,
                UnavailableReason::OsVersionTooOld { required, actual } => panic!(
                    "capture cell {kind:?} requires OS version {required:?}, actual runner OS version {actual:?} on {}",
                    std::env::consts::OS,
                ),
                _ => panic!(
                    "classify unlisted unavailable reason {reason:?} for capture cell {kind:?} on {}: runner OS version {:?}",
                    std::env::consts::OS, snapshot.os_version,
                ),
            },
            Availability::Unknown { error } => unsupported(error),
            Availability::Supported => false,
            _ => panic!(
                "classify unlisted availability {:?} for capture cell {kind:?} on {}: runner OS version {:?}",
                capability.availability, std::env::consts::OS, snapshot.os_version,
            ),
        };
        assert!(capability.backend != BackendKind::Unsupported && !unavailable,
            "capture cell {kind:?} is unsupported on {}: runner OS version {:?}; capability {capability:?}",
            std::env::consts::OS, snapshot.os_version);
        let mut config = StreamConfig::new(kind);
        // A positive PID reaches the real process constructor rather than stopping
        // at the missing-PID validator. Construction never starts the backend.
        config.target_pid = Some(std::process::id());
        if let Err(error) = build_backend(&config, &OpenOptions::default()) {
            assert!(!unsupported(&error),
                "capture cell {kind:?} construction is unsupported on {}: runner OS version {:?}; error {error:?}",
                std::env::consts::OS, snapshot.os_version);
        }
    }
}
