//! Stable snapshot ownership and fail-closed projection of non-exhaustive Rust capabilities.
use crate::{capability_records::*, v2_storage::FlexErrorInfoV2};
use flexaudio::{Availability, BackendKind, Capabilities, OsVersion, PidScope, UnavailableReason};

/// Owned immutable platform snapshot. Accessor views and errors live until capabilities_free_v1.
pub struct FlexCapabilitiesV1 {
    pub(crate) snapshot: Capabilities,
    errors: [Option<FlexErrorInfoV2>; 8],
}

impl FlexCapabilitiesV1 {
    pub(crate) fn new(snapshot: Capabilities) -> Self {
        let availability = [
            &snapshot.sources.mic.availability,
            &snapshot.sources.system.availability,
            &snapshot.sources.process.availability,
            &snapshot.sources.mix.availability,
            &snapshot.system_exclusions.availability,
            &snapshot.device_watcher.availability,
            &snapshot.side_effects.silence_probe.availability,
            &snapshot
                .side_effects
                .classic_loopback_keepalive
                .availability,
        ];
        let errors = availability.map(|value| match value {
            Availability::Unknown { error } => Some(FlexErrorInfoV2::new(error.clone())),
            Availability::Supported | Availability::Unavailable { .. } => None,
            _ => Some(FlexErrorInfoV2::new(flexaudio::Error::Unsupported)),
        });
        Self { snapshot, errors }
    }

    pub(crate) fn availability(&self, value: &Availability, index: usize) -> FlexAvailabilityV1 {
        let mut out = FlexAvailabilityV1 {
            status: FLEX_CAP_UNKNOWN,
            reason: FLEX_CAP_REASON_NONE,
            has_os_versions: 0,
            required: Default::default(),
            actual: Default::default(),
            unknown: self.errors[index]
                .as_ref()
                .map_or(std::ptr::null(), std::ptr::from_ref),
        };
        match value {
            Availability::Supported => out.status = FLEX_CAP_SUPPORTED,
            Availability::Unavailable { reason } => {
                out.status = FLEX_CAP_UNAVAILABLE;
                out.reason = match reason {
                    UnavailableReason::UnsupportedOs => FLEX_CAP_UNSUPPORTED_OS,
                    UnavailableReason::OsVersionTooOld { required, actual } => {
                        out.has_os_versions = 1;
                        out.required = version(required);
                        out.actual = version(actual);
                        FLEX_CAP_OS_VERSION_TOO_OLD
                    }
                    UnavailableReason::PipeWireUnavailable => FLEX_CAP_PIPEWIRE_UNAVAILABLE,
                    UnavailableReason::NotApplicable => FLEX_CAP_NOT_APPLICABLE,
                    _ => FLEX_CAP_OTHER,
                };
            }
            Availability::Unknown { .. } => {}
            _ => out.reason = FLEX_CAP_OTHER,
        }
        out
    }
}

pub(crate) fn version(value: &OsVersion) -> FlexOsVersionV1 {
    FlexOsVersionV1 {
        major: value.major,
        minor: value.minor,
        has_patch: u32::from(value.patch.is_some()),
        patch: value.patch.unwrap_or(0),
        has_build: u32::from(value.build.is_some()),
        build: value.build.unwrap_or(0),
    }
}

pub(crate) fn backend(value: &BackendKind) -> u32 {
    match value {
        BackendKind::Cpal => FLEX_BACKEND_CPAL,
        BackendKind::PipeWire => FLEX_BACKEND_PIPEWIRE,
        BackendKind::WasapiLoopback => FLEX_BACKEND_WASAPI_LOOPBACK,
        BackendKind::WasapiProcessLoopback => FLEX_BACKEND_WASAPI_PROCESS_LOOPBACK,
        BackendKind::CoreAudioProcessTap => FLEX_BACKEND_CORE_AUDIO_PROCESS_TAP,
        BackendKind::Composite => FLEX_BACKEND_COMPOSITE,
        BackendKind::Unsupported => FLEX_BACKEND_UNSUPPORTED,
        _ => FLEX_CAP_OTHER,
    }
}

pub(crate) fn scope(value: &PidScope) -> u32 {
    match value {
        PidScope::ExactPid => FLEX_PID_EXACT,
        PidScope::ProcessTree => FLEX_PID_PROCESS_TREE,
        PidScope::NotApplicable => FLEX_PID_NOT_APPLICABLE,
        _ => FLEX_CAP_OTHER,
    }
}
