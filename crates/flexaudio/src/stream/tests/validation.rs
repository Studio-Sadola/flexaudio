//! Offline validation regressions.
use super::*;

// --- Input validation (Stream::open error paths) ---

#[test]
fn open_validates_exclusion_pids_for_system_capture() {
    for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
        let config = StreamConfig {
            kind,
            exclude_pids: vec![0],
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        assert!(matches!(
            Stream::open(config, backend),
            Err(Error::InvalidArg(message))
                if message == "exclude_pids: pid 0 is not a valid process id"
        ));

        for exclude_pids in [vec![], vec![42]] {
            let config = StreamConfig {
                kind,
                exclude_pids,
                ..Default::default()
            };
            let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
            assert!(Stream::open(config, backend).is_ok());
        }
    }
    for kind in [SourceKind::Mic, SourceKind::ProcessLoopback] {
        let config = StreamConfig {
            kind,
            exclude_pids: vec![0],
            ..Default::default()
        };
        let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
        assert!(Stream::open(config, backend).is_ok());
    }
}

#[test]
fn switch_source_rejects_zero_exclusion_pid_before_replacing_backend() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(StreamConfig::default(), backend).expect("open");
    stream.start().expect("start mock capture");

    for kind in [SourceKind::SystemLoopback, SourceKind::Mix] {
        let new_config = StreamConfig {
            kind,
            exclude_pids: vec![42, 0],
            ..Default::default()
        };
        assert!(matches!(
            stream.switch_source(new_config),
            Err(Error::InvalidArg(message))
                if message == "exclude_pids: pid 0 is not a valid process id"
        ));
        assert_eq!(stream.config.kind, SourceKind::Mic);
        assert!(stream.config.exclude_pids.is_empty());
    }
    stream.stop();
}

/// `ring_capacity_chunks == 0` is rejected with InvalidArg (a ring capacity of 0 is invalid).
#[test]
fn open_rejects_zero_ring_capacity() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        ring_capacity_chunks: 0,
        ..Default::default()
    };
    let err = open_err(Stream::open(config, backend), "capacity 0");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "expected InvalidArg: {err:?}"
    );
}

/// An unsupported output format (channels=3) fails validation with UnsupportedFormat.
#[test]
fn open_rejects_invalid_output_channels() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 48_000,
            channels: 3,
        },
        ..Default::default()
    };
    let err = open_err(Stream::open(config, backend), "ch=3");
    assert!(
        matches!(err, Error::UnsupportedFormat(_)),
        "expected UnsupportedFormat: {err:?}"
    );
}

/// An extreme, out-of-range output rate is also rejected with UnsupportedFormat.
#[test]
fn open_rejects_out_of_range_output_rate() {
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        output: OutputFormat {
            sample_rate: 1_000_000,
            channels: 2,
        },
        ..Default::default()
    };
    let err = open_err(Stream::open(config, backend), "extreme rate");
    assert!(
        matches!(err, Error::UnsupportedFormat(_)),
        "expected UnsupportedFormat: {err:?}"
    );
}

/// If the backend's native_format is 0 (rate=0 / ch=0), it is rejected with InvalidArg.
#[test]
fn open_rejects_zero_native_format() {
    // MockBackend::new applies max(1) internally, so it cannot produce 0. Define a test-only
    // backend with a zero native_format to verify this.
    struct ZeroFormatBackend;
    impl CaptureBackend for ZeroFormatBackend {
        fn native_format(&self) -> (u32, u16) {
            (0, 0)
        }
        fn start(&mut self, _sink: RawSink) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) {}
    }
    let backend = Box::new(ZeroFormatBackend);
    let err = open_err(
        Stream::open(StreamConfig::default(), backend),
        "native_format 0",
    );
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "expected InvalidArg: {err:?}"
    );
}

/// Invalid gains (negative and NaN) are rejected with InvalidArg by both open and set_gain.
#[test]
fn invalid_gain_rejected() {
    // open: config.gain is negative.
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        gain: -1.0,
        ..Default::default()
    };
    let err = open_err(Stream::open(config, backend), "gain=-1.0");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "expected InvalidArg: {err:?}"
    );

    // open: config.gain is NaN.
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let config = StreamConfig {
        gain: f32::NAN,
        ..Default::default()
    };
    let err = open_err(Stream::open(config, backend), "gain=NaN");
    assert!(
        matches!(err, Error::InvalidArg(_)),
        "expected InvalidArg: {err:?}"
    );

    // set_gain: negative and NaN values return InvalidArg and leave the current value unchanged.
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let stream = Stream::open(StreamConfig::default(), backend).expect("open");
    assert!(matches!(stream.set_gain(-1.0), Err(Error::InvalidArg(_))));
    assert!(matches!(
        stream.set_gain(f32::NAN),
        Err(Error::InvalidArg(_))
    ));
    assert_eq!(
        stream.gain(),
        1.0,
        "failed set_gain should not change the current value"
    );
}

/// secondary_output cannot be changed with switch_source (it is fixed at open).
#[test]
fn secondary_output_cannot_change_on_switch() {
    let config = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 16_000,
            channels: 1,
        }),
        ..Default::default()
    };
    let backend = Box::new(MockBackend::new(48_000, 2, 440.0));
    let mut stream = Stream::open(config, backend).expect("open");
    stream.start().expect("start");

    // A switch request that changes the secondary format is rejected with InvalidArg before backend construction.
    let new_config = StreamConfig {
        secondary_output: Some(OutputFormat {
            sample_rate: 8_000,
            channels: 1,
        }),
        ..Default::default()
    };
    let err = stream.switch_source(new_config);
    stream.stop();
    assert!(
        matches!(err, Err(Error::InvalidArg(_))),
        "expected InvalidArg when changing the secondary format: {err:?}"
    );
}
