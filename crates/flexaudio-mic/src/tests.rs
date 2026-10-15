use super::*;
use flexaudio_core::raw_ring;

#[test]
fn legacy_constructor_keeps_windows_capture_check_before_its_single_format_query() {
    let source = include_str!("lib.rs");
    let constructor = source
        .split("pub fn new(device_id:")
        .nth(1)
        .expect("legacy microphone constructor")
        .split("pub fn try_new")
        .next()
        .expect("legacy constructor body");
    assert!(
        constructor.contains("#[cfg(target_os = \"windows\")]"),
        "legacy Windows constructor must preserve its existing capability check"
    );
    assert!(constructor.contains("let purpose = permission::Purpose::Capture;"));
    assert!(constructor.contains("let purpose = permission::Purpose::Enumeration;"));
    assert_eq!(
        constructor.matches("query_native_format(").count(),
        1,
        "legacy construction must keep one native query implementation"
    );
}

#[test]
fn every_native_host_query_requires_an_explicit_consent_purpose() {
    let source = include_str!("lib.rs");
    let host = source
        .split("fn cpal_default_host(")
        .nth(1)
        .expect("CPAL host acquisition function")
        .split("pub struct CpalMicBackend")
        .next()
        .expect("host function body");
    let gate = host
        .find("permission::query_after_preflight")
        .expect("native host acquisition must check consent before any HAL query");
    let query = host
        .find("cpal::default_host()")
        .expect("native host acquisition must be centralized");
    assert!(gate < query, "consent must precede native host acquisition");
    let enumeration = source
        .split("pub fn list_devices()")
        .nth(1)
        .expect("microphone enumeration function")
        .split("impl CaptureBackend")
        .next()
        .expect("enumeration body");
    assert!(
        enumeration.contains("cpal_default_host(permission::Purpose::Enumeration)"),
        "device enumeration must never enter the prompting capture gate"
    );
    let capture = source
        .split("fn build_stream(")
        .nth(1)
        .expect("capture owner stream builder");
    assert!(
        capture.contains("cpal_default_host(permission::Purpose::Capture)"),
        "each capture generation must check consent before native device resolution"
    );
    assert_eq!(
        source.matches("cpal::default_host()").count(),
        2,
        "only the host helper and its documentation may reference native host acquisition"
    );
}

#[test]
fn unavailable_runtime_device_uses_typed_device_lost_without_text_classification() {
    let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    backend
        .callback_errors
        .record(cpal::StreamError::DeviceNotAvailable);
    assert_eq!(
        backend.poll_event(),
        Some(Event::TerminalError {
            error: Error::DeviceLost
        })
    );
    assert_eq!(backend.terminal_error, Some(Error::DeviceLost));
    assert!(backend.poll_event().is_none());
}

#[test]
fn runtime_mailbox_has_one_writer_and_one_delivery_per_generation() {
    let stop = Arc::new(AtomicBool::new(false));
    let mailbox = Arc::new(callback_mailbox::CallbackMailbox::new(stop.clone()));
    thread::scope(|scope| {
        for _ in 0..8 {
            let mailbox = mailbox.clone();
            scope.spawn(move || {
                mailbox.record(cpal::StreamError::BackendSpecific {
                    err: cpal::BackendSpecificError {
                        description: "injected CPAL failure".into(),
                    },
                })
            });
        }
    });
    assert!(stop.load(Ordering::SeqCst));
    let Some(callback_mailbox::CallbackFailure::Native(error)) = mailbox.take() else {
        panic!("native CPAL cause must remain typed");
    };
    assert_eq!(
        error.to_string(),
        "A backend-specific error has occurred: injected CPAL failure"
    );
    assert!(mailbox.take().is_none());
    mailbox.record(cpal::StreamError::DeviceNotAvailable);
    assert!(mailbox.take().is_none());
}

#[test]
fn unpolled_terminal_and_shutdown_events_survive_stop_without_restart() {
    let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    let primary = Error::Backend("injected capture failure".into());
    backend
        .event_tx
        .send(Event::TerminalError {
            error: primary.clone(),
        })
        .unwrap();
    backend.handle = Some(thread::spawn(|| panic!("injected cleanup panic")));
    let cleanup = backend.stop_checked().unwrap_err();
    assert_eq!(backend.terminal_error, Some(primary.clone()));
    assert_eq!(
        backend.poll_event(),
        Some(Event::TerminalError {
            error: primary.clone()
        })
    );
    assert_eq!(
        backend.poll_event(),
        Some(Event::ShutdownError {
            error: cleanup.clone()
        })
    );
    assert_eq!(backend.stop_checked(), Err(cleanup));
    assert!(backend.poll_event().is_none());
    let (producer, _) = raw_ring(16);
    assert_eq!(
        backend.start(RawSink::new(producer, 48_000, 1)),
        Err(primary)
    );
}

#[test]
fn backend_terminal_query_failure_preserves_cause_and_rejects_restart() {
    let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    let error = Error::Backend("injected authorization query failure".into());
    let event = Event::TerminalError {
        error: error.clone(),
    };
    backend.event_tx.send(event.clone()).unwrap();
    assert_eq!(backend.poll_event(), Some(event));
    backend.stop();
    let (prod, _cons) = raw_ring(16);
    assert_eq!(
        backend.start(RawSink::new(prod, FALLBACK_FORMAT.0, FALLBACK_FORMAT.1)),
        Err(error)
    );
}

#[test]
fn backend_permission_event_retains_terminal_cause_and_rejects_restart() {
    let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    let event = Event::PermissionDenied {
        permission: flexaudio_core::types::Permission::Microphone,
        detail: "injected late denial".into(),
    };
    backend.event_tx.send(event.clone()).unwrap();
    assert_eq!(backend.poll_event(), Some(event));
    backend.stop();
    let (prod, _cons) = raw_ring(16);
    let result = backend.start(RawSink::new(prod, FALLBACK_FORMAT.0, FALLBACK_FORMAT.1));
    assert!(
        matches!(result, Err(Error::PermissionDenied { permission: flexaudio_core::types::Permission::Microphone, detail }) if detail == "injected late denial")
    );
    assert_eq!(backend.poll_event(), None);
}

/// Verify that [`fill_scratch`] does not reallocate within capacity and converts correctly,
/// ensuring no steady-state allocations in RT callbacks.
#[test]
fn fill_scratch_no_realloc_in_steady_state() {
    // Allocate for the maximum expected block size.
    let cap = 480 * 2; // equivalent to 10 ms at 48 kHz stereo
    let mut scratch: Vec<f32> = Vec::with_capacity(cap);
    let before = scratch.capacity();

    // Filling in-capacity blocks repeatedly does not change capacity (no reallocation).
    let data: Vec<i16> = (0..cap as i16).collect();
    for _ in 0..100 {
        fill_scratch(&mut scratch, &data, |s| s as f32 / -(i16::MIN as f32));
        assert_eq!(scratch.len(), data.len());
        assert_eq!(
            scratch.capacity(),
            before,
            "capacity does not grow in steady state"
        );
    }
    // Conversion is correct (i16::MIN maps to -1.0).
    let mut one = Vec::with_capacity(1);
    fill_scratch(&mut one, &[i16::MIN], |s| s as f32 / -(i16::MIN as f32));
    assert_eq!(one[0], -1.0);
}

/// `new` + `native_format` do not panic, whether or not an input device exists.
/// `new` always succeeds with either device_id = None (default) or Some (specific device).
#[test]
#[ignore = "requires native host/device queries; use the silent adapter fixture in CI"]
fn new_and_native_format_do_not_panic() {
    // Default input device (device_id = None).
    let backend = CpalMicBackend::new(None);
    let (rate, channels) = backend.native_format();
    // Format values are always positive (FALLBACK_FORMAT when no device exists).
    assert!(rate > 0);
    assert!(channels > 0);

    // Even for a nonexistent device_id, new succeeds without panicking and returns
    // FALLBACK_FORMAT (resolution failure is deferred until start/build_stream).
    let backend = CpalMicBackend::new(Some("__no_such_device__".into()));
    let (rate, channels) = backend.native_format();
    assert_eq!((rate, channels), FALLBACK_FORMAT);
}

/// `start` with a nonexistent device_id returns [`Error::DeviceNotFound`] without
/// panicking. Host microphone denial can precede device resolution on macOS/Windows
/// and skips this hardware assertion.
#[test]
#[ignore = "requires native host/device queries; use the silent adapter fixture in CI"]
fn start_with_unknown_device_id_yields_device_not_found() {
    let mut backend = CpalMicBackend::new(Some("__no_such_device__".into()));
    let (rate, channels) = backend.native_format();
    let cap = (rate as usize * channels as usize).max(1);
    let (prod, _cons) = raw_ring(cap);
    let sink = RawSink::new(prod, rate, channels);

    match backend.start(sink) {
        Err(Error::DeviceNotFound) => {}
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        Err(
            error @ Error::PermissionDenied {
                permission: flexaudio_core::types::Permission::Microphone,
                ..
            },
        ) => {
            eprintln!("Skipping start_with_unknown_device_id_yields_device_not_found: host microphone permission is denied: {error}");
        }
        other => panic!("unknown device_id should return DeviceNotFound: {other:?}"),
    }
}

/// A successful native discovery returns the complete inventory.
/// Every returned device is `Mic`, is not loopback, and has the stable key `id == name`.
#[test]
#[ignore = "requires native host/device queries; use the silent adapter fixture in CI"]
fn list_devices_never_panics_and_is_consistent() {
    let devices =
        list_devices().expect("native device discovery must succeed for this hardware test");
    for d in &devices {
        assert_eq!(d.source_kind, SourceKind::Mic);
        assert!(!d.is_loopback, "microphones are not loopback devices");
        // Stable key: cpal uses the device name as the ID.
        assert_eq!(d.id, d.name);
        assert!(!d.id.is_empty(), "id (= name) is nonempty");
        if d.format_available {
            assert!(d.sample_rate > 0);
            assert!(d.channels > 0);
        } else {
            assert_eq!((d.sample_rate, d.channels), (0, 0));
        }
    }
    // At most one default input device.
    assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
}

/// `start` may return `Err(DeviceNotFound)` where no input device exists (servers/CI).
/// Host microphone denial is also an environment outcome on macOS/Windows. Where
/// an input device is accessible, capture starts and stops on stop.
#[test]
#[ignore = "requires native host/device queries; use the silent adapter fixture in CI"]
fn start_then_stop_tolerates_missing_device() {
    let mut backend = CpalMicBackend::new(None);
    let (rate, channels) = backend.native_format();
    let cap = (rate as usize * channels as usize).max(1); // about one second
    let (prod, _cons) = raw_ring(cap);
    let sink = RawSink::new(prod, rate, channels);

    match backend.start(sink) {
        Ok(()) => {
            // In environments where start succeeds, stop must be safe.
            backend.stop();
            // Repeated stop calls are safe too.
            backend.stop();
        }
        Err(Error::DeviceNotFound) => {
            // Accept this when no input device is present (CI/server).
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        Err(
            error @ Error::PermissionDenied {
                permission: flexaudio_core::types::Permission::Microphone,
                ..
            },
        ) => {
            eprintln!("Skipping start_then_stop_tolerates_missing_device: host microphone permission is denied: {error}");
        }
        Err(other) => panic!("unexpected error from start(): {other:?}"),
    }
}

/// End-to-end test that records from a real microphone. Run with
/// `cargo test -p flexaudio-mic -- --ignored` on a laptop or other machine with an input
/// device. Ignored by default because servers/CI usually have no input device.
#[test]
#[ignore = "requires a real microphone; run `cargo test -p flexaudio-mic -- --ignored` on a laptop"]
fn end_to_end_captures_real_audio() {
    use std::time::Duration;

    let mut backend = CpalMicBackend::new(None);
    let (rate, channels) = backend.native_format();
    let cap = rate as usize * channels as usize * 2; // about two seconds
    let (prod, mut cons) = raw_ring(cap);
    let sink = RawSink::new(prod, rate, channels);

    backend
        .start(sink)
        .expect("start() should succeed with a real input device");

    // Capture for a few hundred milliseconds and verify that samples arrive.
    thread::sleep(Duration::from_millis(500));
    backend.stop();

    let mut buf = vec![0.0f32; cap];
    let got = cons.pop_slice(&mut buf);
    assert!(got > 0, "expected captured samples, got none");
    // Samples stay within [-1, 1] (conversion is valid).
    assert!(buf[..got].iter().all(|&s| (-1.5..=1.5).contains(&s)));
}

#[test]
fn infallible_constructor_retains_query_failure_before_start() {
    // Native permission/device failures are not portable to CI; inject the same
    // query result consumed by new rather than access hardware or request consent.
    let cause = Error::Backend("input configuration query failed; reconnect the device and retry".into())
        .with_context(ErrorContext::new(Operation::Enumerate));
    let mut backend = CpalMicBackend::from_query_result(None, Err(cause.clone()));
    assert_eq!(backend.terminal_error, Some(cause.clone()));
    assert_eq!(backend.poll_event(), Some(Event::TerminalError { error: cause }));
    assert_eq!(backend.poll_event(), None);
}

#[test]
fn microphone_native_query_uses_the_checked_discovery_helper() {
    // CPAL Device is concrete and cannot inject native query failures without hardware.
    // No portable behavioral wiring test is possible; input_config discovery tests
    // inject query failures through the shared helper, and this guards adapter wiring.
    let source = include_str!("lib.rs");
    let query = source.split("fn query_native_format(").nth(1).unwrap().split("pub fn list_devices()").next().unwrap();
    assert!(query.contains("input_config::discover"));
    assert!(!query.contains(".ok()"));
    assert!(!query.contains("unwrap_or"));
}

#[test]
fn callback_panic_closes_pcm_and_publishes_one_typed_terminal() {
    let mut backend = CpalMicBackend::with_format(None, FALLBACK_FORMAT);
    backend.callback_errors.guard(|| panic!("injected callback panic"));
    assert!(backend.stop_flag.load(Ordering::SeqCst));
    let event = backend.poll_event().expect("callback panic must be observable");
    assert!(matches!(event, Event::TerminalError { error: Error::Backend(ref message) } if message.contains("panicked")));
    assert!(backend.terminal_error.is_some());
    assert!(backend.poll_event().is_none());
    backend.callback_errors.guard(|| panic!("later callback panic"));
    assert!(backend.poll_event().is_none());
}

#[test]
fn microphone_inventory_uses_validated_native_formats() {
    // CPAL's concrete Device cannot inject formats without native hardware.
    // input_config discovery tests exercise the validation; this checks adapter wiring.
    let source = include_str!("lib.rs");
    let inventory = source.split("pub fn list_devices()").nth(1).unwrap().split("impl CaptureBackend").next().unwrap();
    assert!(inventory.contains("input_config::discover("));
}
