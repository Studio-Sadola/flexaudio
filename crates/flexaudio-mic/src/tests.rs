use super::*;
use flexaudio_core::raw_ring;

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
    assert_eq!(
        mailbox.take().unwrap().to_string(),
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
        assert!(d.sample_rate > 0);
        assert!(d.channels > 0);
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
