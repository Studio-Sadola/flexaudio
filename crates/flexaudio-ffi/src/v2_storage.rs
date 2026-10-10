//! Immutable owners for v2 borrowed views. No caller fields govern deallocation.
use crate::types::FlexDeviceInfo;
use crate::v2_records::*;
use flexaudio::{
    AudioLoss, AudioPath, Error, ErrorContext, Event, LossReason, MixLane, NativeStatus, Operation,
    OutputTap,
};
use std::ffi::CString;

pub(crate) fn cstring(s: impl Into<String>) -> CString {
    CString::new(s.into())
        .unwrap_or_else(|_| CString::new("invalid string").expect("static string"))
}
pub(crate) fn lane(lane: Option<MixLane>) -> u32 {
    match lane {
        None => 0,
        Some(MixLane::Microphone) => 1,
        Some(MixLane::SystemAudio) => 2,
        _ => u32::MAX,
    }
}
pub(crate) fn permission(p: flexaudio::Permission) -> i32 {
    match p {
        flexaudio::Permission::Microphone => 1,
        flexaudio::Permission::SystemAudio => 2,
        _ => -1,
    }
}
struct Context {
    context: ErrorContext,
    call: Option<CString>,
}
impl Context {
    fn new(context: ErrorContext) -> Self {
        let call = match context.native_status() {
            Some(NativeStatus::HResult { call, .. } | NativeStatus::OsStatus { call, .. }) => {
                Some(cstring(call))
            }
            _ => None,
        };
        Self { context, call }
    }
    fn record(&self) -> FlexErrorContextV2 {
        let operation = match self.context.operation() {
            Operation::Enumerate => 0,
            Operation::Start => 1,
            Operation::Normalize => 2,
            Operation::Flush => 3,
            Operation::Reopen => 4,
            Operation::Rollback => 5,
            Operation::Stop => 6,
            Operation::Join => 7,
            Operation::Link => 8,
            _ => u32::MAX,
        };
        let (native_code_kind, native_code) = match self.context.native_status() {
            Some(NativeStatus::HResult { bits, .. }) => (1, i64::from(bits)),
            Some(NativeStatus::OsStatus { value, .. }) => (2, i64::from(value)),
            _ => (0, 0),
        };
        FlexErrorContextV2 {
            operation,
            lane: lane(self.context.lane()),
            native_code_kind,
            native_code,
            native_call: self.call.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        }
    }
}
/// Owned immutable error tree. Borrowed views live until this tree's explicit free.
pub struct FlexErrorInfoV2 {
    pub(crate) root: Error,
    pub(crate) message: CString,
    contexts: Vec<Context>,
    pub(crate) secondary: Vec<FlexErrorInfoV2>,
}
impl FlexErrorInfoV2 {
    pub(crate) fn new(error: Error) -> Self {
        let mut contexts = Vec::new();
        let mut secondary = Vec::new();
        fn collect(
            error: &Error,
            contexts: &mut Vec<Context>,
            secondary: &mut Vec<FlexErrorInfoV2>,
        ) {
            match error {
                Error::Context { source, context } => {
                    contexts.push(Context::new(*context));
                    collect(source, contexts, secondary);
                }
                Error::Multiple(group) => {
                    collect(group.primary(), contexts, secondary);
                    secondary.extend(group.secondary().cloned().map(FlexErrorInfoV2::new));
                }
                _ => {}
            }
        }
        collect(&error, &mut contexts, &mut secondary);
        Self {
            root: error.root().clone(),
            message: cstring(error.to_string()),
            contexts,
            secondary,
        }
    }
    pub(crate) fn context_count(&self) -> usize {
        self.contexts.len()
    }
    pub(crate) fn context(&self, i: usize) -> Option<FlexErrorContextV2> {
        self.contexts.get(i).map(Context::record)
    }
}
/// Owned event with immutable payloads. Free with flexaudio_event_free_v2.
pub struct FlexEventV2 {
    pub(crate) kind: i32,
    pub(crate) count: Option<u64>,
    pub(crate) permission: Option<i32>,
    pub(crate) error: Option<FlexErrorInfoV2>,
    pub(crate) loss: Option<FlexAudioLossV2>,
    pub(crate) message: Option<CString>,
}
impl FlexEventV2 {
    pub(crate) fn new(event: Event) -> Self {
        let mut out = Self {
            kind: 6,
            count: None,
            permission: None,
            error: None,
            loss: None,
            message: None,
        };
        match event {
            Event::ChunkDropped { count } => {
                out.kind = 0;
                out.count = Some(count);
            }
            Event::StreamStalled => out.kind = 1,
            Event::StreamRecovered => out.kind = 2,
            Event::PermissionDenied { permission: p, .. } => {
                out.kind = 3;
                out.permission = Some(permission(p));
                out.message = Some(cstring(
                    Error::PermissionDenied {
                        permission: p,
                        detail: String::new(),
                    }
                    .to_string(),
                ));
            }
            Event::DeviceLost => out.kind = 4,
            Event::Error(_) => {
                out.kind = 5;
                out.message = Some(cstring("legacy backend failure"));
            }
            Event::SilenceWhileSourceActive { .. } => {
                out.kind = 7;
                out.message = Some(cstring(
                    "Silence observed while source is active; check recording permissions.",
                ));
            }
            Event::PermissionPending { permission: p, .. } => {
                out.kind = 8;
                out.permission = Some(permission(p));
                out.message = Some(cstring(p.guidance()));
            }
            Event::PermissionGranted => {
                out.kind = FLEX_EVENT_KIND_PERMISSION_GRANTED;
                out.permission = Some(1);
            }
            Event::AudioLoss { loss } => {
                out.kind = FLEX_EVENT_KIND_AUDIO_LOSS;
                out.loss = Some(loss_record(loss));
            }
            Event::ShutdownError { error } => {
                out.kind = FLEX_EVENT_KIND_SHUTDOWN_ERROR;
                out.error = Some(FlexErrorInfoV2::new(error));
            }
            Event::TerminalError { error } => {
                out.kind = FLEX_EVENT_KIND_TERMINAL_ERROR;
                out.error = Some(FlexErrorInfoV2::new(error));
            }
            Event::RecoverableError { error } => {
                out.kind = FLEX_EVENT_KIND_RECOVERABLE_ERROR;
                out.error = Some(FlexErrorInfoV2::new(error));
            }
            Event::Clipped => out.kind = FLEX_EVENT_KIND_CLIPPED,
            _ => out.message = Some(cstring("unknown stream event")),
        }
        out
    }
}
fn loss_record(loss: AudioLoss) -> FlexAudioLossV2 {
    let mut out = FlexAudioLossV2 {
        sample_rate: loss.sample_rate(),
        channels: loss.channels(),
        ..Default::default()
    };
    match loss.path() {
        AudioPath::Capture { lane: l } => {
            out.path = 0;
            out.lane = lane(l);
        }
        AudioPath::MixFifo { lane: l } => {
            out.path = 1;
            out.lane = lane(Some(l));
        }
        AudioPath::Output { tap } => {
            out.path = 2;
            out.tap = match tap {
                OutputTap::Primary => 0,
                OutputTap::Secondary => 1,
                _ => u32::MAX,
            };
        }
        _ => out.path = u32::MAX,
    }
    out.reason = match loss.reason() {
        LossReason::RawOverflow => 0,
        LossReason::MixFifoOverflow => 1,
        LossReason::CorruptBuffer => 2,
        LossReason::MalformedBuffer => 3,
        LossReason::CallbackRejected => 4,
        LossReason::OutputOverflow => 5,
        _ => u32::MAX,
    };
    if let Some(samples) = loss.samples() {
        out.count_known = 1;
        out.samples = samples.get();
    }
    out
}
/// Owned watcher event. Device and string views are borrowed until explicit free.
pub struct FlexDeviceEventV2 {
    pub(crate) kind: i32,
    pub(crate) default_kind: Option<i32>,
    pub(crate) dropped_events: Option<u64>,
    pub(crate) device: Option<FlexDeviceInfo>,
    pub(crate) id: Option<CString>,
}
impl FlexDeviceEventV2 {
    pub(crate) fn new(event: flexaudio::DeviceEvent) -> Self {
        use flexaudio::{DefaultDeviceKind, DeviceEvent};
        let mut out = Self {
            kind: 3,
            default_kind: None,
            dropped_events: None,
            device: None,
            id: None,
        };
        let default = |k| match k {
            DefaultDeviceKind::Microphone => 0,
            DefaultDeviceKind::SystemAudio => 1,
            _ => -1,
        };
        match event {
            DeviceEvent::Added(info) => {
                out.kind = 0;
                out.id = Some(cstring(info.id.clone()));
                out.device = Some(crate::convert::device_info_to_c(info));
            }
            DeviceEvent::Removed { id } => {
                out.kind = 1;
                out.id = Some(cstring(id));
            }
            DeviceEvent::DefaultChanged { kind, id } => {
                out.kind = 2;
                out.default_kind = Some(default(kind));
                out.id = Some(cstring(id));
            }
            DeviceEvent::DefaultCleared { kind } => {
                out.kind = FLEX_DEVICE_EVENT_KIND_DEFAULT_CLEARED;
                out.default_kind = Some(default(kind));
            }
            DeviceEvent::RescanRequired { dropped_events } => {
                out.kind = FLEX_DEVICE_EVENT_KIND_RESCAN_REQUIRED;
                out.dropped_events = Some(dropped_events);
            }
            _ => {}
        }
        out
    }
}
impl Drop for FlexDeviceEventV2 {
    fn drop(&mut self) {
        if let Some(device) = self.device.take() {
            unsafe {
                drop(CString::from_raw(device.id));
                drop(CString::from_raw(device.name));
            }
        }
    }
}
/// Owned completed shutdown report; all child pointers are borrowed.
pub struct FlexShutdownReportV2 {
    pub(crate) primary: Option<FlexErrorInfoV2>,
    pub(crate) cleanup: Vec<FlexErrorInfoV2>,
}
impl FlexShutdownReportV2 {
    pub(crate) fn new(report: flexaudio::ShutdownReport) -> Self {
        Self {
            primary: report.primary().cloned().map(FlexErrorInfoV2::new),
            cleanup: report
                .cleanup()
                .iter()
                .cloned()
                .map(FlexErrorInfoV2::new)
                .collect(),
        }
    }
}
