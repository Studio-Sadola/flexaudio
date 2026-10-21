//! Commit, recovery, rollback, and side-effect-free source-switch preflight.

use super::*;
use crate::{IgnoreReason, SettingKey, SettingOutcome, SettingValue};
use std::sync::atomic::AtomicUsize;

struct Backend {
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    fail: bool,
    sink: Option<RawSink>,
}
impl Backend {
    fn new(fail: bool) -> Self {
        Self {
            starts: Arc::new(AtomicUsize::new(0)),
            stops: Arc::new(AtomicUsize::new(0)),
            fail,
            sink: None,
        }
    }
}
impl CaptureBackend for Backend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: RawSink) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.sink = Some(sink);
        if self.fail {
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

fn open(config: StreamConfig, strict: bool, backend: Backend) -> Stream {
    crate::open_using(config, OpenOptions::new().strict(strict), |_, _| {
        Ok(Box::new(backend))
    })
    .unwrap()
}

#[test]
fn successful_identical_switches_increment_recovery_does_not() {
    let mut config = StreamConfig::default();
    config.gain = 2.0;
    config.ring_capacity_chunks = 7;
    let mut stream = open(config, false, Backend::new(false));
    stream.start().unwrap();
    let initial = stream.effective_settings().unwrap().clone();
    assert_eq!(initial.revision, 0);
    Stream::open_backend_once(&stream.shared, GenerationChange::Recovery).unwrap();
    assert_eq!(stream.effective_settings(), Some(&initial));
    stream.set_gain(3.0).unwrap();
    stream.set_denoise(true);
    for revision in [1, 2] {
        stream
            .switch_source_using(StreamConfig::default(), None, None, |_, _| {
                Ok(Box::new(Backend::new(false)))
            })
            .unwrap();
        let report = stream.effective_settings().unwrap();
        assert_eq!(report.revision, revision);
        let gain = report.get(SettingKey::Gain).unwrap();
        assert_eq!(gain.requested, SettingValue::Float(1.0));
        assert_eq!(gain.effective, SettingValue::Float(3.0));
        assert_eq!(gain.outcome, SettingOutcome::NotRequested);
        assert_eq!(
            report
                .get(SettingKey::RingCapacityChunks)
                .unwrap()
                .effective,
            SettingValue::Unsigned(7)
        );
        assert_eq!(
            report.get(SettingKey::Denoise).unwrap().requested,
            SettingValue::Bool(false)
        );
        assert_eq!(
            report.get(SettingKey::Denoise).unwrap().effective,
            SettingValue::Bool(true)
        );
    }
    stream.stop_checked().unwrap();
}

#[test]
fn construction_and_start_failures_keep_previous_report_options_and_config() {
    let backend = Backend::new(false);
    let starts = backend.starts.clone();
    let mut stream = open(StreamConfig::default(), false, backend);
    stream.start().unwrap();
    let report = stream.effective_settings().unwrap().clone();
    let config = stream.config().clone();
    for fail_start in [false, true] {
        let replacement = StreamConfig::new(crate::SourceKind::SystemLoopback);
        let error = stream
            .switch_source_using(
                replacement,
                Some(true),
                Some(OpenOptions::new().strict(true)),
                |_, _| {
                    if fail_start {
                        Ok(Box::new(Backend::new(true)))
                    } else {
                        Err(Error::DeviceNotFound)
                    }
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DeviceNotFound);
        assert_eq!(stream.effective_settings(), Some(&report));
        assert_eq!(stream.config(), &config);
        assert_eq!(stream.open_options(), &OpenOptions::default());
        assert!(!stream.shared.denoise_enabled.load(Ordering::SeqCst));
    }
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    stream.stop_checked().unwrap();
}

#[test]
fn strict_switch_counts_zero_builds_and_old_source_stops_and_keeps_snapshot() {
    let backend = Backend::new(false);
    let stops = backend.stops.clone();
    let starts = backend.starts.clone();
    let mut stream = open(StreamConfig::default(), true, backend);
    stream.start().unwrap();
    let initial = stream.effective_settings().unwrap().clone();
    let builds = AtomicUsize::new(0);
    let mut requested = StreamConfig::default();
    requested.gain = 2.0;
    requested.ring_capacity_chunks = 7;
    requested.target_pid = Some(42);
    let error = stream
        .switch_source_using(requested, None, None, |_, _| {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Backend::new(false)))
        })
        .unwrap_err();
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    assert_eq!(starts.load(Ordering::SeqCst), 1);
    assert_eq!(stream.effective_settings(), Some(&initial));
    assert!(stream.open_options().strict);
    let candidate = error.ignored_settings().unwrap();
    assert_eq!(candidate.revision, 1);
    for key in [SettingKey::Gain, SettingKey::RingCapacityChunks] {
        assert_eq!(
            candidate.get(key).unwrap().outcome,
            SettingOutcome::Ignored(IgnoreReason::FixedAtOpen)
        );
    }
    // Explicit permissive replacement is committed together with a successful source.
    stream
        .switch_source_using(
            {
                let mut config_value = StreamConfig::default();
                config_value.gain = 2.0;
                config_value
            },
            Some(true),
            Some(OpenOptions::new().strict(false)),
            |_, _| Ok(Box::new(Backend::new(false))),
        )
        .unwrap();
    assert!(!stream.open_options().strict);
    assert_eq!(stream.effective_settings().unwrap().revision, 1);
    assert_eq!(
        stream
            .effective_settings()
            .unwrap()
            .get(SettingKey::Denoise)
            .unwrap()
            .effective,
        SettingValue::Bool(true)
    );
    stream.stop_checked().unwrap();
}

#[test]
fn omitted_options_report_defaults_while_retaining_backend_options() {
    let mut stream = crate::open_using(
        StreamConfig::default(),
        OpenOptions::new().windows_classic_loopback_keepalive(false),
        |_, _| Ok(Box::new(Backend::new(false))),
    )
    .unwrap();
    stream.start().unwrap();
    stream
        .switch_source_using(StreamConfig::default(), None, None, |_, options| {
            assert!(!options.windows_classic_loopback_keepalive);
            Ok(Box::new(Backend::new(false)))
        })
        .unwrap();
    let entry = stream
        .effective_settings()
        .unwrap()
        .get(SettingKey::WindowsClassicLoopbackKeepalive)
        .unwrap();
    assert_eq!(entry.requested, SettingValue::Bool(true));
    assert_eq!(entry.effective, SettingValue::Absent);
    assert_eq!(entry.outcome, SettingOutcome::NotRequested);
    stream.stop_checked().unwrap();
}

#[test]
fn omitted_options_keep_controls_and_attachment_reports_across_source_round_trip() {
    let config = |kind| {
        let mut config_value = StreamConfig::new(kind);
        config_value.target_pid = (kind == crate::SourceKind::ProcessLoopback).then_some(42);
        config_value
    };
    for initial_kind in [
        crate::SourceKind::Mic,
        crate::SourceKind::ProcessLoopback,
        crate::SourceKind::SystemLoopback,
    ] {
        for enabled in [true, false] {
            let options = OpenOptions::new()
                .windows_classic_loopback_keepalive(enabled)
                .settings_attachments(crate::SettingsAttachments::new().secondary_encoding("s16"));
            let mut stream = crate::open_using(config(initial_kind), options, |_, _| {
                Ok(Box::new(Backend::new(false)))
            })
            .unwrap();
            stream.start().unwrap();
            for kind in [
                crate::SourceKind::SystemLoopback,
                crate::SourceKind::Mic,
                crate::SourceKind::SystemLoopback,
            ] {
                stream
                    .switch_source_using(config(kind), None, None, |_, options| {
                        assert_eq!(options.windows_classic_loopback_keepalive, enabled);
                        Ok(Box::new(Backend::new(false)))
                    })
                    .unwrap();
                let report = stream.effective_settings().unwrap();
                let keepalive = report
                    .get(SettingKey::WindowsClassicLoopbackKeepalive)
                    .unwrap();
                assert_eq!(keepalive.requested, SettingValue::Bool(true));
                assert_eq!(keepalive.outcome, SettingOutcome::NotRequested);
                assert_eq!(
                    keepalive.effective,
                    if cfg!(target_os = "windows") && kind == crate::SourceKind::SystemLoopback {
                        SettingValue::Bool(enabled)
                    } else {
                        SettingValue::Absent
                    }
                );
                let encoding = report.get(SettingKey::SecondaryEncoding).unwrap();
                assert_eq!(encoding.requested, SettingValue::Text("f32".into()));
                assert_eq!(encoding.effective, SettingValue::Text("s16".into()));
                assert_eq!(encoding.outcome, SettingOutcome::NotRequested);
            }
            stream.stop_checked().unwrap();
        }
    }
}

#[test]
fn attachment_switch_preflight_matches_public_report_and_commits_only_on_success() {
    let mut config = StreamConfig::default();
    config.secondary_output = Some(crate::OutputFormat {
        sample_rate: 16_000,
        channels: 1,
    });
    let attachments = crate::SettingsAttachments::new()
        .denoise(true)
        .vad(true)
        .secondary_encoding("s16")
        .fixed_at_open([SettingKey::Denoise]);
    let mut stream = crate::open_using(
        config.clone(),
        OpenOptions::new().settings_attachments(attachments.clone()),
        |_, _| Ok(Box::new(Backend::new(false))),
    )
    .unwrap();
    stream.set_denoise(true);
    stream.start().unwrap();
    let previous = stream.effective_settings().unwrap().clone();
    let options = OpenOptions::new()
        .strict(true)
        .settings_attachments(attachments);
    let expected = crate::resolve_settings(
        &crate::SettingsRequest::new(config.clone())
            .open_options(&options)
            .previous(&previous),
    )
    .unwrap();
    let error = stream
        .switch_source_using(config.clone(), None, Some(options), |_, _| {
            panic!("strict attachment rejection must precede native construction")
        })
        .unwrap_err();
    assert_eq!(error.ignored_settings(), Some(&expected));
    assert_eq!(stream.effective_settings(), Some(&previous));

    let options = OpenOptions::new().settings_attachments(
        crate::SettingsAttachments::new()
            .denoise(false)
            .omitted_on_switch([
                SettingKey::Vad,
                SettingKey::SecondaryOutput,
                SettingKey::SecondaryEncoding,
            ]),
    );
    let expected = crate::resolve_settings(
        &crate::SettingsRequest::new(config.clone())
            .open_options(&options)
            .previous(&previous),
    )
    .unwrap();
    stream
        .switch_source_using(config, Some(false), Some(options), |_, _| {
            Ok(Box::new(Backend::new(false)))
        })
        .unwrap();
    assert_eq!(stream.effective_settings(), Some(&expected));
    assert_eq!(
        expected.get(SettingKey::Vad).unwrap().effective,
        SettingValue::Bool(true)
    );
    assert_eq!(
        expected
            .get(SettingKey::SecondaryEncoding)
            .unwrap()
            .effective,
        SettingValue::Text("s16".into())
    );
    stream.stop_checked().unwrap();
}
