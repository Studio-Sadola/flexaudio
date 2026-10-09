#![allow(dead_code, unused_imports)]
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
    #[derive(Debug)] pub struct StreamError(pub &'static str);
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
mod input_config { // LIVE_INPUT_CONFIG
}
mod permission {
    use flexaudio_core::types::Result;
    pub fn can_query_format() -> bool { true }
    pub fn preflight() -> Result<bool> { Ok(false) }
}
mod capture_owner {
    pub fn run(_: flexaudio_core::RawSink, _: Option<String>, _: std::sync::Arc<std::sync::atomic::AtomicBool>, _: std::sync::mpsc::Sender<flexaudio_core::types::Result<()>>, _: std::sync::mpsc::Sender<flexaudio_core::types::Event>) {
        panic!("fixture forbids opening native capture; call build_stream directly");
    }
}
// LIVE_ADAPTER

fn device(key: u32, name: &str) -> cpal::Device {
    cpal::Device { key, name: Ok(name.into()), config: Ok(cpal::SupportedStreamConfig { sample_rate: cpal::SampleRate(48_000), channels: 1 }) }
}
fn host(devices: Vec<cpal::Device>, failure: Option<&str>) {
    cpal::HOST.with(|h| *h.borrow_mut() = cpal::Host { devices, failure: failure.map(str::to_owned) });
}
fn callback_fixture() -> (CpalMicBackend, cpal::Stream, flexaudio_core::raw_ring::RawConsumer) {
    host(vec![device(1, "mic")], None);
    let backend = CpalMicBackend::with_format(None, (48_000, 1));
    let (producer, consumer) = flexaudio_core::raw_ring(16);
    let stream = build_stream(RawSink::new(producer, 48_000, 1), None, backend.stop_flag.clone()).unwrap();
    (backend, stream, consumer)
}
#[test]
#[ignore = "repro: F03"]
fn repro_p5_runtime_error() {
    let (mut backend, stream, _) = callback_fixture();
    stream.error.borrow_mut()(cpal::StreamError("injected USB device lost"));
    assert!(backend.poll_event().is_some(), "F03: CPAL reported USB device lost; backend event = None");
}
#[test]
fn repro_p5_runtime_error_control() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    stream.data.borrow_mut()(&[0.25, -0.25]);
    let mut values = [0.0; 2];
    assert_eq!(consumer.pop_slice(&mut values), 2);
    assert_eq!(values, [0.25, -0.25]);
    assert!(backend.poll_event().is_none());
}
#[test]
#[ignore = "repro: F19"]
fn repro_p5_duplicate_name() {
    host(vec![device(1, "same"), device(2, "same")], None);
    let selected = resolve_input_device(&cpal::default_host(), Some("same"));
    assert!(selected.is_err(), "F19: ambiguous name selected key {:?}; expected rejection", selected.map(|d| d.key));
}
#[test]
fn repro_p5_duplicate_name_control() {
    host(vec![device(1, "first"), device(2, "second")], None);
    assert_eq!(resolve_input_device(&cpal::default_host(), Some("second")).unwrap().key, 2);
    let devices = list_devices().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices.iter().filter(|d| d.is_default).count(), 1);
}
#[test]
#[ignore = "repro: F20"]
fn repro_p5_query_failure() {
    host(vec![], Some("injected ALSA enumeration failure"));
    let found = list_devices();
    assert!(found.is_err(), "F20: injected enumeration failure returned {found:?}");
}
#[test]
fn repro_p5_query_failure_control() {
    host(vec![], None);
    assert_eq!(list_devices().unwrap(), vec![]);
}
#[test]
#[ignore = "repro: F20"]
fn repro_p5_config_failure() {
    let mut broken = device(1, "mic");
    broken.config = Err("injected configuration query failure".into());
    host(vec![broken], None);
    let found = list_devices();
    assert!(found.is_err(), "F20: unreadable mandatory configuration returned {found:?}");
}
#[test]
fn repro_p5_config_failure_control() {
    host(vec![device(1, "mic")], None);
    assert_eq!(list_devices().unwrap().len(), 1);
}
#[test]
fn repro_p5_format_guard_control() {
    host(vec![device(1, "mic")], None);
    let (producer, _) = flexaudio_core::raw_ring(16);
    let result = build_stream(RawSink::new(producer, 44_100, 2), None, Arc::new(AtomicBool::new(false)));
    assert!(matches!(result, Err(Error::NativeFormatChanged { advertised: (44_100, 2), actual: (48_000, 1) })));
    let (_backend, stream, mut consumer) = callback_fixture();
    stream.data.borrow_mut()(&[0.1]);
    assert_eq!(consumer.pop_slice(&mut [0.0]), 1);
}
#[test]
fn repro_p5_priming_loss() {
    let (mut backend, stream, mut consumer) = callback_fixture();
    for _ in 0..100 { stream.data.borrow_mut()(&[1.1]); }
    let got = consumer.pop_slice(&mut [0.0; 16]);
    assert!(got > 0 || backend.poll_event().is_some(), "M25: 100 priming blocks dropped; delivered=0, diagnostic=None");
}
#[test]
fn repro_p5_priming_loss_control() {
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
#[ignore = "repro: F37"]
fn repro_p5_join_failure() {
    let mut backend = owner_fixture(true);
    backend.stop();
    assert!(backend.poll_event().is_some(), "F37: owner join failed; stop returned and diagnostic=None");
}
#[test]
fn repro_p5_join_failure_control() {
    let mut backend = owner_fixture(false);
    backend.stop();
    assert!(backend.handle.is_none());
    assert!(backend.poll_event().is_none());
}
#[test]
#[ignore = "repro: NEW Linux mic identity"]
fn repro_p5_pipewire_mic_identity() {
    // The Linux live DeviceInfo construction is separately checked by
    // repro_p7_mic_identity_control. Facade routes Mic IDs to this resolver.
    host(vec![device(1, "USB microphone")], None);
    let result = resolve_input_device(&cpal::default_host(), Some("alsa_input.card0"));
    assert!(result.is_ok(), "NEW mic identity: advertised PipeWire node.name ID returned {result:?} from CPAL name resolver");
}
#[test]
fn repro_p5_pipewire_mic_identity_control() {
    host(vec![device(1, "USB microphone")], None);
    assert_eq!(resolve_input_device(&cpal::default_host(), Some("USB microphone")).unwrap().key, 1);
}
#[test]
fn repro_p5_callback_panic_attempt() {
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
fn repro_p5_callback_panic_attempt_control() { repro_p5_runtime_error_control(); }
#[test]
fn repro_p5_terminal_retention_control() {
    let mut backend = CpalMicBackend::with_format(None, (48_000, 1));
    let error = Error::Backend("injected terminal owner error".into());
    let event = Event::TerminalError { error: error.clone() };
    backend.event_tx.send(event.clone()).unwrap();
    assert_eq!(backend.poll_event(), Some(event)); backend.stop();
    let (producer, _) = flexaudio_core::raw_ring(16);
    assert_eq!(backend.start(RawSink::new(producer, 48_000, 1)), Err(error));
}
