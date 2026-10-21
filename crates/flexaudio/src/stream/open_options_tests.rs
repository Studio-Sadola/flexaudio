//! Device-free option retention and source-switch rollback checks.
use super::*;
use std::sync::atomic::AtomicUsize;

struct TestBackend {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    fail_start: bool,
    sink: Option<RawSink>,
}

impl CaptureBackend for TestBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }

    fn start(&mut self, sink: RawSink) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.sink = Some(sink);
        if self.fail_start {
            Err(Error::DeviceNotFound)
        } else {
            Ok(())
        }
    }

    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.sink = None;
    }
}

fn backend(fail_start: bool) -> TestBackend {
    TestBackend {
        starts: Arc::new(AtomicUsize::new(0)),
        stops: Arc::new(AtomicUsize::new(0)),
        fail_start,
        sink: None,
    }
}

fn options(enabled: bool) -> OpenOptions {
    OpenOptions::new().windows_classic_loopback_keepalive(enabled)
}

#[test]
fn options_default_and_builder_preserve_legacy_keepalive() {
    assert!(OpenOptions::default().windows_classic_loopback_keepalive);
    assert_eq!(OpenOptions::new(), OpenOptions::default());
    assert!(!options(false).windows_classic_loopback_keepalive);
    assert_eq!(
        options(false).windows_classic_loopback_keepalive(true),
        options(true)
    );

    let stream = Stream::open(StreamConfig::default(), Box::new(backend(false))).unwrap();
    assert_eq!(stream.open_options(), &OpenOptions::default());
}

#[test]
fn facade_open_matches_explicit_default_options_without_hardware() {
    let configs = [
        {
            let mut config_value = StreamConfig::default();
            config_value.chunk_ms = 10;
            config_value
        },
        {
            let mut config_value = StreamConfig::default();
            config_value.output = OutputFormat {
                sample_rate: 0,
                channels: 2,
            };
            config_value
        },
        {
            let mut config_value = StreamConfig::new(crate::SourceKind::SystemLoopback);
            config_value.exclude_pids = vec![0];
            config_value
        },
        {
            let mut config_value = StreamConfig::new(crate::SourceKind::ProcessLoopback);
            config_value.target_pid = None;
            config_value
        },
    ];
    for config in configs {
        let legacy_error = crate::open(config.clone())
            .err()
            .expect("invalid configuration");
        let explicit_error = crate::open_with_options(config, OpenOptions::default())
            .err()
            .expect("invalid configuration");
        assert_eq!(legacy_error, explicit_error);
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[test]
fn facade_open_retains_default_and_explicit_options_before_capture_starts() {
    // Process backend construction stores the PID and format without opening
    // native capture, so the successful facade path needs no device or consent.
    let mut config = StreamConfig::new(crate::SourceKind::ProcessLoopback);
    config.target_pid = Some(1);
    let legacy = crate::open(config.clone()).unwrap();
    let explicit = crate::open_with_options(config.clone(), OpenOptions::default()).unwrap();
    assert_eq!(legacy.config(), explicit.config());
    assert_eq!(legacy.open_options(), explicit.open_options());
    assert_eq!(legacy.open_options(), &OpenOptions::default());
    // Retain the control even though process capture cannot apply it.
    let disabled_options = options(false).strict(false);
    let disabled = crate::open_with_options(config, disabled_options.clone()).unwrap();
    assert_eq!(disabled.open_options(), &disabled_options);
}

#[test]
fn source_switches_retain_current_options_with_and_without_denoise() {
    let initial = StreamConfig::default();
    let mut stream = Stream::open_with_backend_options(
        initial.clone(),
        Box::new(backend(false)),
        options(false),
    )
    .unwrap();
    stream.start().unwrap();
    for denoise in [None, Some(true)] {
        stream
            .switch_source_using(initial.clone(), denoise, None, |_, retained| {
                assert_eq!(retained, &options(false));
                Ok(Box::new(backend(false)))
            })
            .unwrap();
        assert_eq!(stream.open_options(), &options(false));
    }
    assert!(stream.shared.denoise_enabled.load(Ordering::SeqCst));
    stream.stop_checked().unwrap();
}

#[test]
fn successful_source_switch_commits_explicit_options() {
    let initial = StreamConfig::default();
    let mut stream = Stream::open(initial.clone(), Box::new(backend(false))).unwrap();
    stream.start().unwrap();
    let mut replacement = initial;
    replacement.kind = crate::SourceKind::SystemLoopback;
    // This test commits an explicitly permissive, potentially inapplicable control.
    let requested_options = options(false).strict(false);
    stream
        .switch_source_using(
            replacement.clone(),
            None,
            Some(requested_options.clone()),
            |_, requested| {
                assert_eq!(requested, &requested_options);
                Ok(Box::new(backend(false)))
            },
        )
        .unwrap();
    assert_eq!(stream.open_options(), &requested_options);
    assert_eq!(stream.config(), &replacement);
    stream.stop_checked().unwrap();
}

#[test]
fn source_construction_failure_keeps_previous_options_and_backend() {
    let old_backend = backend(false);
    let old_starts = old_backend.starts.clone();
    let old_stops = old_backend.stops.clone();
    let initial = StreamConfig::default();
    let mut stream = Stream::open(initial.clone(), Box::new(old_backend)).unwrap();
    stream.start().unwrap();
    // Reach the construction failure instead of rejecting the inapplicable control.
    let requested_options = options(false).strict(false);
    assert_eq!(
        stream.switch_source_using(
            initial.clone(),
            None,
            Some(requested_options.clone()),
            |_, requested| {
                assert_eq!(requested, &requested_options);
                Err(Error::DeviceNotFound)
            }
        ),
        Err(Error::DeviceNotFound)
    );
    assert_eq!(stream.open_options(), &options(true));
    assert_eq!(stream.config(), &initial);
    assert_eq!(old_starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_stops.load(Ordering::SeqCst), 0);
    stream.stop_checked().unwrap();
}

#[test]
fn source_start_failure_restores_backend_and_keeps_previous_options() {
    let old_backend = backend(false);
    let old_starts = old_backend.starts.clone();
    let rejected_backend = backend(true);
    let rejected_stops = rejected_backend.stops.clone();
    let initial = StreamConfig::default();
    let mut stream =
        Stream::open_with_backend_options(initial.clone(), Box::new(old_backend), options(false))
            .unwrap();
    stream.start().unwrap();
    let mut replacement = initial.clone();
    replacement.kind = crate::SourceKind::SystemLoopback;
    assert_eq!(
        stream.switch_source_using(replacement, None, Some(options(true)), |_, requested| {
            assert_eq!(requested, &options(true));
            Ok(Box::new(rejected_backend))
        }),
        Err(Error::DeviceNotFound)
    );
    assert_eq!(stream.open_options(), &options(false));
    assert_eq!(stream.config(), &initial);
    assert_eq!(old_starts.load(Ordering::SeqCst), 2);
    assert_eq!(rejected_stops.load(Ordering::SeqCst), 1);
    stream.stop_checked().unwrap();
}

#[test]
fn public_options_switch_does_not_commit_on_validation_failure() {
    let initial = StreamConfig::default();
    let mut stream = Stream::open(initial.clone(), Box::new(backend(false))).unwrap();
    stream.start().unwrap();
    let mut replacement = initial.clone();
    replacement.output = OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    };
    assert!(matches!(
        stream.switch_source_with_options(replacement, options(false)),
        Err(Error::InvalidArg(_))
    ));
    assert_eq!(stream.open_options(), &options(true));
    assert_eq!(stream.config(), &initial);
    stream.stop_checked().unwrap();
}
