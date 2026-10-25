#![allow(dead_code, unused_imports)]
use std::panic::{catch_unwind, AssertUnwindSafe};
// CPAL fixture. The adapter below is read verbatim from lib.rs on every run.
mod cpal {
    use std::cell::RefCell;
    #[derive(Clone, Debug)]
    pub struct Device { pub key: u32, pub name: std::result::Result<String, String>, pub config: std::result::Result<SupportedStreamConfig, String> }
    #[derive(Clone, Default)]
    pub struct Host { pub devices: Vec<Device>, pub failure: Option<String> }
    thread_local! { pub static HOST: RefCell<Host> = RefCell::new(Host::default()); }
    pub fn default_host() -> Host { HOST.with(|h| h.borrow().clone()) }
    #[derive(Clone, Copy, Debug)]
    pub enum SampleFormat { F32, I16, U16, I32, Other }
    #[derive(Clone, Copy, Debug)]
    pub struct SampleRate(pub u32);
    #[derive(Clone, Debug)]
    pub struct SupportedStreamConfig { pub sample_rate: SampleRate, pub channels: u16 }
    impl SupportedStreamConfig {
        pub fn sample_rate(&self) -> SampleRate { self.sample_rate }
        pub fn channels(&self) -> u16 { self.channels }
        pub fn sample_format(&self) -> SampleFormat { SampleFormat::F32 }
    }
    pub struct StreamConfig { pub sample_rate: SampleRate, pub channels: u16 }
    impl From<SupportedStreamConfig> for StreamConfig {
        fn from(c: SupportedStreamConfig) -> Self { Self { sample_rate: c.sample_rate, channels: c.channels } }
    }
    pub struct InputCallbackInfo;
    #[derive(Debug)] #[non_exhaustive] pub enum StreamError {
        DeviceNotAvailable,
        BackendSpecific { err: BackendSpecificError },
    }
    #[derive(Debug)] pub struct BackendSpecificError { pub description: &'static str }
    impl std::fmt::Display for BackendSpecificError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.description) }
    }
    impl std::fmt::Display for StreamError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::DeviceNotAvailable => f.write_str("device not available"),
                Self::BackendSpecific { err } => err.fmt(f),
            }
        }
    }
    pub trait Sample: Copy { fn fixture(value: f32) -> Self; }
    impl Sample for f32 { fn fixture(value: f32) -> Self { value } }
    impl Sample for i16 { fn fixture(_: f32) -> Self { 0 } }
    impl Sample for u16 { fn fixture(_: f32) -> Self { 32768 } }
    impl Sample for i32 { fn fixture(_: f32) -> Self { 0 } }
    pub struct Stream { pub data: RefCell<Box<dyn FnMut(&[f32])>>, pub error: RefCell<Box<dyn FnMut(StreamError)>> }
    pub mod traits {
        use super::*;
        pub trait HostTrait {
            fn default_input_device(&self) -> Option<Device>;
            fn input_devices(&self) -> std::result::Result<std::vec::IntoIter<Device>, String>;
        }
        impl HostTrait for Host {
            fn default_input_device(&self) -> Option<Device> { self.devices.first().cloned() }
            fn input_devices(&self) -> std::result::Result<std::vec::IntoIter<Device>, String> {
                match &self.failure { Some(e) => Err(e.clone()), None => Ok(self.devices.clone().into_iter()) }
            }
        }
        pub trait DeviceTrait {
            fn name(&self) -> std::result::Result<String, String>;
            fn default_input_config(&self) -> std::result::Result<SupportedStreamConfig, String>;
            fn build_input_stream<T: Sample + 'static, D: FnMut(&[T], &InputCallbackInfo) + 'static, E: FnMut(StreamError) + 'static>(
                &self, _: &StreamConfig, data: D, error: E, _: Option<std::time::Duration>
            ) -> std::result::Result<Stream, String>;
        }
        impl DeviceTrait for Device {
            fn name(&self) -> std::result::Result<String, String> { self.name.clone() }
            fn default_input_config(&self) -> std::result::Result<SupportedStreamConfig, String> { self.config.clone() }
            fn build_input_stream<T: Sample + 'static, D: FnMut(&[T], &InputCallbackInfo) + 'static, E: FnMut(StreamError) + 'static>(
                &self, _: &StreamConfig, mut data: D, error: E, _: Option<std::time::Duration>
            ) -> std::result::Result<Stream, String> {
                Ok(Stream { data: RefCell::new(Box::new(move |samples| {
                    let values: Vec<T> = samples.iter().copied().map(T::fixture).collect();
                    data(&values, &InputCallbackInfo);
                })), error: RefCell::new(Box::new(error)) })
            }
        }
    }
}
// Include the live callback mailbox; run.py compiles from the repo root.
mod callback_mailbox {
    use crate::cpal;
    include!(concat!(env!("ADAPTER_ROOT"), "/crates/flexaudio-mic/src/callback_mailbox.rs"));
}
mod input_config { // LIVE_INPUT_CONFIG
}
mod permission {
    use flexaudio_core::types::Result;
    #[derive(Clone, Copy)]
    pub enum Purpose { Capture, Enumeration }
    pub fn preflight(_: Purpose) -> Result<()> { Ok(()) }
    pub fn query_after_preflight<T>(check: impl FnOnce() -> Result<()>, query: impl FnOnce() -> Result<T>) -> Result<T> {
        check()?;
        query()
    }
    pub fn after_failure(error: flexaudio_core::Error) -> flexaudio_core::Error { error }
}
mod capture_owner {
    pub fn run(_: flexaudio_core::RawSink, _: Option<String>, _: std::sync::Arc<std::sync::atomic::AtomicBool>, _: std::sync::mpsc::Sender<flexaudio_core::types::Result<()>>, _: std::sync::Arc<crate::callback_mailbox::CallbackMailbox>) {
        panic!("fixture forbids opening native capture; call build_stream directly");
    }
}
// LIVE_ADAPTER

// Exercise the live facade aggregation with silent providers. PipeWire DeviceInfo
// construction includes the shared Linux module; error wrappers are extracted unchanged.
mod facade {
    use flexaudio_core::{DeviceInfo, Error, ErrorContext, NamespacedDevice, Operation, Result, SourceKind};
    mod flexaudio_mic {
        pub fn list_devices() -> flexaudio_core::Result<Vec<flexaudio_core::DeviceInfo>> {
            crate::list_devices()
        }
        pub fn list_devices_with_namespaces() -> flexaudio_core::Result<Vec<flexaudio_core::NamespacedDevice>> {
            crate::list_devices_with_namespaces()
        }
    }
    pub mod flexaudio_os_linux {
        use super::*;
        use std::cell::Cell;
        thread_local! { pub static FAIL_QUERY: Cell<bool> = const { Cell::new(false) }; }
        const NATIVE_RATE: u32 = 48_000;
        const NATIVE_CHANNELS: u16 = 2;
        mod device_info {
            include!(concat!(env!("ADAPTER_ROOT"), "/crates/flexaudio-os-linux/src/device_info.rs"));
        }
        use device_info::{namespaced_devices, EnumState, NodeRecord};
        fn enumerate_pw() -> Result<Vec<NamespacedDevice>> {
            if FAIL_QUERY.get() { return Err(Error::Backend("PipeWire discovery could not connect to the daemon".into())); }
            let state = EnumState {
                nodes: vec![
                    NodeRecord { media_class: "Audio/Source".into(), node_name: "alsa_input.card0".into(), description: "USB microphone".into(), rate: Some(48_000), channels: Some(1) },
                    NodeRecord { media_class: "Audio/Sink".into(), node_name: "alsa_output.card0".into(), description: "Speakers".into(), rate: Some(48_000), channels: Some(2) },
                ],
                default_sink: Some("alsa_output.card0".into()), default_source: Some("alsa_input.card0".into()),
            };
            Ok(namespaced_devices(&state))
        }
        // LIVE_PIPEWIRE_LIST_DEVICES
    }
    // LIVE_FACADE_DEVICES
}

fn device(key: u32, name: &str) -> cpal::Device {
    cpal::Device { key, name: Ok(name.into()), config: Ok(cpal::SupportedStreamConfig { sample_rate: cpal::SampleRate(48_000), channels: 1 }) }
}
fn host(devices: Vec<cpal::Device>, failure: Option<&str>) {
    cpal::HOST.with(|h| *h.borrow_mut() = cpal::Host { devices, failure: failure.map(str::to_owned) });
}
fn stream_error(description: &'static str) -> cpal::StreamError {
    cpal::StreamError::BackendSpecific { err: cpal::BackendSpecificError { description } }
}
fn callback_fixture() -> (CpalMicBackend, cpal::Stream, flexaudio_core::raw_ring::RawConsumer) {
    host(vec![device(1, "mic")], None);
    let backend = CpalMicBackend::with_format(None, (48_000, 1));
    let (producer, consumer) = flexaudio_core::raw_ring(16);
    let stream = build_stream(RawSink::new(producer, 48_000, 1), None, backend.stop_flag.clone(), backend.callback_errors.clone()).unwrap();
    (backend, stream, consumer)
}
#[test]
fn microphone_runtime_error() {
    let (mut backend, stream, _) = callback_fixture();
    stream.error.borrow_mut()(stream_error("injected USB device lost"));
    assert!(backend.stop_flag.load(Ordering::SeqCst), "runtime failure must gate PCM immediately");
    assert!(matches!(backend.poll_event(), Some(Event::TerminalError { error: Error::Backend(message) }) if message.contains("injected USB device lost")), "CPAL cause must survive control-thread reporting");
    assert!(backend.poll_event().is_none());
}
#[test]
fn microphone_runtime_error_control() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    stream.data.borrow_mut()(&[0.25, -0.25]);
    let mut values = [0.0; 2];
    assert_eq!(consumer.pop_slice(&mut values), 2);
    assert_eq!(values, [0.25, -0.25]);
    assert!(backend.poll_event().is_none());
}
#[test]
fn runtime_failure_closes_pcm_retains_distinct_causes_and_survives_stop() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    stream.error.borrow_mut()(stream_error("first native failure"));
    stream.error.borrow_mut()(stream_error("second native failure"));
    stream.data.borrow_mut()(&[0.5]);
    assert_eq!(consumer.pop_slice(&mut [0.0; 16]), 0);
    backend.stop();
    assert_eq!(backend.poll_event(), Some(Event::TerminalError {
        error: Error::Backend("microphone capture failed: first native failure".into()),
    }));
    assert!(backend.poll_event().is_none());
    let expected = Error::Multiple(flexaudio_core::ErrorGroup::new(
        Error::Backend("microphone capture failed: first native failure".into()),
        Error::Backend("microphone capture failed: second native failure".into()),
        Vec::new(),
    ));
    assert_eq!(backend.shutdown_report().unwrap().primary(), Some(&expected));
    let (producer, _) = flexaudio_core::raw_ring(16);
    assert_eq!(backend.start(RawSink::new(producer, 48_000, 1)),
        Err(expected));
}
#[test]
fn microphone_duplicate_name() {
    host(vec![device(1, "same"), device(2, "same")], None);
    let selected = resolve_input_device(&cpal::default_host(), Some("same"));
    assert!(matches!(selected, Err(Error::AmbiguousDeviceName)), "expected typed ambiguous-name rejection");
}
#[test]
fn microphone_duplicate_name_control() {
    host(vec![device(1, "first"), device(2, "second")], None);
    assert_eq!(resolve_input_device(&cpal::default_host(), Some("second")).unwrap().key, 2);
    let devices = list_devices().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d.is_default).count(), 1);
}
#[test]
fn microphone_query_failure() {
    host(vec![], Some("injected ALSA enumeration failure"));
    let found = list_devices();
    assert!(found.is_err(), "injected enumeration failure returned {found:?}");
}
#[test]
fn microphone_query_failure_control() {
    host(vec![], None);
    assert_eq!(list_devices().unwrap(), vec![]);
}
#[test]
fn legacy_constructor_discovery_failure_is_only_returned_from_start() {
    host(vec![], None);
    let mut backend = CpalMicBackend::new(None);
    assert_eq!(backend.native_format(), FALLBACK_FORMAT);
    for _ in 0..2 {
        assert!(backend.poll_event().is_none());
        let (producer, _consumer) = flexaudio_core::raw_ring(16);
        assert_eq!(backend.start(RawSink::new(producer, FALLBACK_FORMAT.0, FALLBACK_FORMAT.1)), Err(Error::DeviceNotFound));
        assert!(backend.handle.is_none());
        assert_eq!(backend.stop_checked(), Ok(()));
        assert_eq!(backend.shutdown_report(), Some(&ShutdownReport::new(None, Vec::new())));
        assert!(backend.poll_event().is_none());
    }
}
#[test]
fn microphone_config_failure() {
    let mut broken = device(1, "mic");
    broken.config = Err("injected configuration query failure".into());
    host(vec![broken], None);
    let found = list_devices().unwrap();
    assert_eq!(found.len(), 1);
    assert!(!found[0].format_available);
    assert_eq!((found[0].sample_rate, found[0].channels), (0, 0));
}
#[test]
fn microphone_config_failure_control() {
    host(vec![device(1, "mic")], None);
    assert_eq!(list_devices().unwrap().len(), 1);
}
#[test]
fn unavailable_metadata_stays_unknown_and_lookup_fails_closed() {
    for broken_name in [false, true] {
        let mut broken = device(2, "private microphone label");
        if broken_name { broken.name = Err("injected name query failure".into()); }
        else { broken.config = Err("injected configuration query failure".into()); }
        host(vec![device(1, "healthy"), broken], None);
        let found = list_devices().unwrap();
        assert_eq!(found.len(), if broken_name { 1 } else { 2 });
        if !broken_name {
            assert!(!found[1].format_available);
            assert_eq!((found[1].sample_rate, found[1].channels), (0, 0));
        }
        if broken_name {
            assert_eq!(resolve_input_device(&cpal::default_host(), Some("healthy")).unwrap_err().kind(), flexaudio_core::ErrorKind::Backend);
        }
    }
}
#[test]
fn advertised_cpal_ids_roundtrip_and_unknown_id_never_selects_default() {
    host(vec![device(1, "first"), device(2, "second")], None);
    for (info, key) in list_devices().unwrap().into_iter().zip([1, 2]) {
        assert_eq!(resolve_input_device(&cpal::default_host(), Some(&info.id)).unwrap().key, key);
    }
    assert!(matches!(resolve_input_device(&cpal::default_host(), Some("unknown")), Err(Error::DeviceNotFound)));
}
#[test]
fn microphone_format_guard_control() {
    host(vec![device(1, "mic")], None);
    let (producer, _) = flexaudio_core::raw_ring(16);
    let result = build_stream(RawSink::new(producer, 44_100, 2), None, Arc::new(AtomicBool::new(false)), Arc::new(callback_mailbox::CallbackMailbox::new(Arc::new(AtomicBool::new(false)))));
    assert!(matches!(result, Err(Error::NativeFormatChanged { advertised: (44_100, 2), actual: (48_000, 1) })));
    let (_backend, stream, mut consumer) = callback_fixture();
    stream.data.borrow_mut()(&[0.1]);
    assert_eq!(consumer.pop_slice(&mut [0.0]), 1);
}
#[test]
fn microphone_priming_loss() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    for _ in 0..100 { stream.data.borrow_mut()(&[1.1]); }
    let got = consumer.pop_slice(&mut [0.0; 16]);
    assert!(got > 0 || backend.poll_event().is_some(), "100 priming blocks dropped; delivered=0, diagnostic=None");
}
#[test]
fn microphone_priming_loss_control() {
    let (_backend, stream, mut consumer) = callback_fixture();
    stream.data.borrow_mut()(&[0.5]);
    stream.data.borrow_mut()(&[1.1]);
    let mut values = [0.0; 2];
    assert_eq!(consumer.pop_slice(&mut values), 2);
    assert_eq!(values, [0.5, 1.1]);
}
fn owner_fixture(panics: bool) -> CpalMicBackend {
    let mut backend = CpalMicBackend::with_format(None, (48_000, 1));
    backend.handle = Some(std::thread::spawn(move || {
        if panics { panic!("injected owner teardown panic"); }
    }));
    backend
}
#[test]
fn microphone_join_failure() {
    let mut backend = owner_fixture(true);
    let result = backend.stop_checked();
    assert!(result.is_err(), "owner join failure must fail checked teardown");
    assert_eq!(backend.stop_checked(), result);
    backend.stop();
    assert!(matches!(backend.poll_event(), Some(Event::ShutdownError { .. })), "owner join failed; diagnostic=None");
    assert!(backend.poll_event().is_none(), "repeated shutdown emitted duplicate diagnostic");
}
#[test]
fn microphone_join_failure_control() {
    let mut backend = owner_fixture(false);
    backend.stop();
    assert!(backend.handle.is_none());
    assert!(backend.poll_event().is_none());
}
#[test]
fn failed_start_returns_start_cause_and_preserves_only_shutdown_cleanup() {
    // The fixture owner exits by panic before readiness without any native calls.
    for previous_panics in [false, true] {
        let mut backend = owner_fixture(previous_panics);
        let previous_shutdown = backend.stop_checked();
        let previous_report = backend.shutdown_report().unwrap().clone();
        assert_eq!(previous_report.result(), previous_shutdown);
        while backend.poll_event().is_some() {}
        let (producer, _) = flexaudio_core::raw_ring(16);
        let error = backend.start(RawSink::new(producer, 48_000, 1)).unwrap_err();
        let Error::Multiple(group) = &error else { panic!("start and join failures must both survive") };
        assert!(matches!(group.primary(), Error::Context { context, .. } if context.operation() == Operation::Start));
        let cleanup: Vec<_> = group.secondary().collect();
        assert_eq!(cleanup.len(), previous_report.cleanup().len() + 1);
        assert_eq!(&cleanup[..previous_report.cleanup().len()], previous_report.cleanup().iter().collect::<Vec<_>>().as_slice());
        assert!(cleanup.iter().all(|error| matches!(error, Error::Context { context, .. } if context.operation() == Operation::Join)));
        let join_error = Error::Backend("microphone capture owner panicked".into())
            .with_context(ErrorContext::new(Operation::Join));
        assert_eq!(cleanup.last(), Some(&&join_error));
        let report = backend.shutdown_report().unwrap().clone();
        assert!(report.primary().is_none());
        assert_eq!(report.cleanup().iter().collect::<Vec<_>>(), cleanup);
        let expected_stop = ShutdownReport::new(None, cleanup.iter().map(|error| (*error).clone()).collect()).result();
        assert_eq!(report.result(), expected_stop);
        assert!(backend.handle.is_none());
        assert!(backend.stop_flag.load(Ordering::SeqCst));
        assert_eq!(backend.stop_checked(), expected_stop);
        assert_eq!(backend.shutdown_report(), Some(&report));
        backend.stop();
        assert_eq!(backend.shutdown_report(), Some(&report));
        assert_eq!(backend.poll_event(), Some(Event::ShutdownError { error: join_error }));
        assert!(backend.poll_event().is_none());
    }
}
#[test]
fn microphone_pipewire_mic_identity() {
    // Retain the original mismatch: CPAL knows the name, while PipeWire knows
    // node.name. Only IDs usable by the mic backend may enter the facade inventory.
    host(vec![device(1, "USB microphone")], None);
    let native = facade::flexaudio_os_linux::list_devices().unwrap();
    assert!(native.iter().any(|d| d.source_kind == SourceKind::Mic && d.id == "alsa_input.card0"));
    let advertised = facade::devices().unwrap();
    let microphones: Vec<_> = advertised.iter().filter(|d| d.source_kind == SourceKind::Mic).collect();
    assert_eq!(microphones.len(), 1, "PipeWire must not duplicate the cpal microphone");
    for info in microphones {
        let selected = resolve_input_device(&cpal::default_host(), Some(&info.id)).unwrap();
        assert_eq!(selected.key, 1, "the advertised ID must select the requested microphone");
        let (producer, _) = flexaudio_core::raw_ring(16);
        assert!(build_stream(RawSink::new(producer, info.sample_rate, info.channels), Some(&info.id), Arc::new(AtomicBool::new(false)), Arc::new(callback_mailbox::CallbackMailbox::new(Arc::new(AtomicBool::new(false))))).is_ok());
    }
    assert!(advertised.iter().any(|d| d.source_kind == SourceKind::SystemLoopback && d.id == "alsa_output.card0"));
    assert!(matches!(resolve_input_device(&cpal::default_host(), Some("alsa_input.card0")), Err(Error::DeviceNotFound)), "an unadvertised PipeWire ID must never select the default microphone");
}
#[test]
fn microphone_pipewire_mic_identity_control() {
    host(vec![device(1, "USB microphone")], None);
    assert_eq!(resolve_input_device(&cpal::default_host(), Some("USB microphone")).unwrap().key, 1);
}
#[test]
fn facade_device_provenance_survives_aggregation() {
    host(vec![device(1, "USB microphone")], None);
    facade::flexaudio_os_linux::FAIL_QUERY.set(false);
    let entries = facade::devices_with_namespaces().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].id_namespace, flexaudio_core::DeviceIdNamespace::CpalDeviceName);
    assert_eq!(entries[0].device.id, "USB microphone");
    assert_eq!(entries[1].id_namespace, flexaudio_core::DeviceIdNamespace::PipeWireNodeName);
    assert_eq!(entries[1].device.id, "alsa_output.card0");
    assert_eq!(facade::devices().unwrap(), entries.into_iter().map(|entry| entry.device).collect::<Vec<_>>());
}
#[test]
fn facade_discovery_failure_has_one_enumerate_context() {
    // Both actual provider wrappers attach Enumerate. The facade must propagate
    // them once and must reject partial success when the other provider succeeds.
    for fail_pipewire in [false, true] {
        host(vec![device(1, "USB microphone")], if fail_pipewire { None } else { Some("microphone discovery failed") });
        facade::flexaudio_os_linux::FAIL_QUERY.set(fail_pipewire);
        let error = facade::devices().unwrap_err();
        assert_eq!(error.root().kind(), flexaudio_core::ErrorKind::Backend);
        let mut source = &error;
        let mut enumerate_contexts = 0;
        loop {
            match source {
                Error::Context { source: inner, context } => {
                    if context.operation() == Operation::Enumerate { enumerate_contexts += 1; }
                    source = inner;
                }
                Error::Diagnostic { source: inner, .. } => source = inner,
                _ => break,
            }
        }
        assert!(matches!(source, Error::Backend(_)));
        assert_eq!(enumerate_contexts, 1, "Enumerate must appear exactly once");
    }
    facade::flexaudio_os_linux::FAIL_QUERY.set(false);
}
#[test]
fn microphone_callback_panic_attempt() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    // Cover nonfinite/extreme data after priming, and scratch capacity growth.
    stream.data.borrow_mut()(&[0.25]);
    let result = catch_unwind(AssertUnwindSafe(|| {
        stream.data.borrow_mut()(&[f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::MAX, f32::MIN]);
        let mut scratch = Vec::with_capacity(1);
        fill_scratch(&mut scratch, &[0i16; 1000], |s| f32::from(s) / 32768.0);
        assert_eq!(scratch.len(), 1000);
    }));
    assert!(result.is_ok()); assert_eq!(consumer.pop_slice(&mut [0.0; 16]), 6);
    assert!(backend.poll_event().is_none());
}
#[test]
fn microphone_callback_panic_attempt_control() { microphone_runtime_error_control(); }
#[test]
fn microphone_terminal_retention_control() {
    let mut backend = CpalMicBackend::with_format(None, (48_000, 1));
    let error = Error::Backend("injected terminal owner error".into());
    let event = Event::TerminalError { error: error.clone() };
    backend.event_tx.send(event.clone()).unwrap();
    assert_eq!(backend.poll_event(), Some(event)); backend.stop();
    let (producer, _) = flexaudio_core::raw_ring(16);
    assert_eq!(backend.start(RawSink::new(producer, 48_000, 1)), Err(error));
}
