//! Device-free real queue routing and transactional settings tests.
use super::*;
use pyo3::types::PyDict;

fn fixture(config: fa::StreamConfig, strict: bool) -> Stream {
    let report = crate::settings::Request {
        config: &config,
        keepalive: Some(true),
        denoise: false,
        vad: false,
        secondary: None,
    }
    .resolve(None, strict)
    .unwrap();
    let mut inner =
        fa::Stream::open(config, Box::new(fa::MockBackend::new(48_000, 2, 0.0))).unwrap();
    inner.start().unwrap();
    let mut stream = denoise_tests::binding(inner);
    stream.settings = crate::settings::SettingsState {
        report: Some(report),
        strict,
    };
    stream
}

#[test]
fn switch_commits_revision_retains_fixed_values_and_rolls_back_failures() {
    Python::initialize();
    Python::attach(|py| {
        let config = fa::StreamConfig {
            gain: 2.0,
            ring_capacity_chunks: 7,
            ..Default::default()
        };
        let mut stream = fixture(config, true);
        let snapshot = stream.effective_settings(py).unwrap();
        stream.inner.set_gain(3.0).unwrap();
        for revision in 1..=2 {
            stream
                .switch_with(
                    fa::StreamConfig::default(),
                    false,
                    None,
                    None,
                    |inner, _| {
                        inner.switch_backend_with_denoise(
                            Box::new(fa::MockBackend::new(48_000, 2, 0.0)),
                            false,
                        )
                    },
                )
                .unwrap();
            let report = stream.settings.report.as_ref().unwrap();
            assert_eq!(report.revision, revision);
            assert_eq!(
                report.get(fa::SettingKey::Gain).unwrap().effective,
                fa::SettingValue::Float(3.0)
            );
            assert_eq!(
                report
                    .get(fa::SettingKey::RingCapacityChunks)
                    .unwrap()
                    .effective,
                fa::SettingValue::Unsigned(7)
            );
        }
        assert_eq!(
            snapshot
                .get_item("revision")
                .unwrap()
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            0
        );
        let before = stream.settings.report.clone().unwrap();
        let ignored = fa::StreamConfig {
            gain: 3.0,
            ring_capacity_chunks: 7,
            ..Default::default()
        };
        let error = stream
            .switch_with(ignored, false, None, None, |_, _| {
                panic!("strict rejection reached backend")
            })
            .unwrap_err();
        let report = error.value(py).getattr("report").unwrap();
        assert_eq!(
            report
                .get_item("revision")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            3
        );
        assert_eq!(stream.settings.report.as_ref().unwrap(), &before);
        assert!(stream.settings.strict);
        assert!(stream
            .switch_with(
                fa::StreamConfig::default(),
                false,
                None,
                Some(false),
                |_, _| Err(fa::Error::DeviceNotFound)
            )
            .is_err());
        assert_eq!(stream.settings.report.as_ref().unwrap(), &before);
        assert!(stream.settings.strict);
        stream
            .switch_with(
                fa::StreamConfig {
                    gain: 4.0,
                    ring_capacity_chunks: 9,
                    ..Default::default()
                },
                true,
                None,
                Some(false),
                |inner, _| {
                    inner.switch_backend_with_denoise(
                        Box::new(fa::MockBackend::new(48_000, 2, 0.0)),
                        true,
                    )
                },
            )
            .unwrap();
        assert!(!stream.settings.strict);
        let report = stream.settings.report.as_ref().unwrap();
        assert_eq!(report.revision, 3);
        assert_eq!(
            report.get(fa::SettingKey::Denoise).unwrap().outcome,
            fa::SettingOutcome::Applied
        );
        assert_eq!(
            report.get(fa::SettingKey::Gain).unwrap().outcome,
            fa::SettingOutcome::Ignored(fa::IgnoreReason::FixedAtOpen)
        );
        stream.stop().unwrap();
    });
}

struct LifecycleBackend(std::collections::VecDeque<fa::Event>);
impl fa::CaptureBackend for LifecycleBackend {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, _: fa::core::backend::RawSink) -> fa::Result<()> {
        Ok(())
    }
    fn stop(&mut self) {
        self.0.push_back(fa::Event::SilenceProbeStopped {
            reason: fa::HelperStopReason::StreamStopped,
        });
        self.0
            .push_back(fa::Event::ClassicLoopbackKeepaliveStopped {
                reason: fa::HelperStopReason::StreamStopped,
            });
    }
    fn poll_event(&mut self) -> Option<fa::Event> {
        self.0.pop_front()
    }
}

#[test]
fn lifecycle_fifo_is_independent_and_drainable_after_stop() {
    Python::initialize();
    Python::attach(|py| {
        for ordinary_first in [false, true] {
            let backend = LifecycleBackend(std::collections::VecDeque::from([
                fa::Event::SilenceProbeStarted,
                fa::Event::StreamRecovered,
                fa::Event::ClassicLoopbackKeepaliveStarted,
            ]));
            let mut inner =
                fa::Stream::open(fa::StreamConfig::default(), Box::new(backend)).unwrap();
            inner.start().unwrap();
            let mut stream = denoise_tests::binding(inner);
            stream.local_events.push_back(fa::Event::Clipped);
            // Stopping synchronizes backend event delivery; both queues must remain drainable.
            stream.stop().unwrap();
            let locals = PyDict::new(py);
            locals
                .set_item("stream", Py::new(py, stream).unwrap())
                .unwrap();
            locals.set_item("ordinary_first", ordinary_first).unwrap();
            py.run(
                pyo3::ffi::c_str!(
                    r#"
ordinary = []
lifecycle = []
def drain_ordinary():
    while (event := stream.poll_event()) is not None:
        ordinary.append(event.type)
def drain_lifecycle():
    while (event := stream.poll_lifecycle_event()) is not None:
        lifecycle.append((event.type, event.reason, event.message))
if ordinary_first:
    drain_ordinary()
    drain_lifecycle()
else:
    drain_lifecycle()
    drain_ordinary()
assert ordinary == ['recovered', 'clipped'], ordinary
assert lifecycle == [
    ('silenceProbeStarted', None, None),
    ('classicLoopbackKeepaliveStarted', None, None),
    ('silenceProbeStopped', 'streamStopped', None),
    ('classicLoopbackKeepaliveStopped', 'streamStopped', None),
], lifecycle
assert stream.poll_event() is None
assert stream.poll_lifecycle_event() is None
"#
                ),
                Some(&locals),
                Some(&locals),
            )
            .unwrap();
        }
    });
}

#[test]
fn switch_boundary_validates_capacity_and_retains_strictness() {
    Python::initialize();
    Python::attach(|py| {
        let locals = PyDict::new(py);
        locals
            .set_item(
                "stream",
                Py::new(py, fixture(fa::StreamConfig::default(), true)).unwrap(),
            )
            .unwrap();
        py.run(
            pyo3::ffi::c_str!(
                r#"
before = stream.effective_settings()
for value in (True, False, 0, -1, 1.5, '50', 2 ** 256):
    try:
        stream.switch_source('mic', ring_capacity_chunks=value)
    except (TypeError, ValueError):
        pass
    else:
        raise AssertionError('invalid ring capacity accepted')
for strict in ({}, {'strict': None}, {'strict': True}):
    try:
        stream.switch_source('mic', gain=2.0, **strict)
    except ValueError as error:
        assert type(error).__name__ == 'IgnoredSettingsError'
        rows = {row['setting']: row for row in error.report['entries']}
        assert rows['gain']['outcome'] == {'status': 'ignored', 'reason': 'fixedAtOpen'}
    else:
        raise AssertionError('strictness was not retained')
assert stream.effective_settings() == before
stream.stop()
"#
            ),
            Some(&locals),
            Some(&locals),
        )
        .unwrap();
    });
}

#[test]
fn defaults_never_trigger_strict_rejection_and_nondefaults_are_permissive() {
    let config = fa::StreamConfig::default();
    let request = crate::settings::Request {
        config: &config,
        keepalive: Some(true),
        denoise: false,
        vad: false,
        secondary: None,
    };
    let report = request.resolve(None, true).unwrap();
    assert!(report
        .entries
        .iter()
        .all(|entry| entry.outcome == fa::SettingOutcome::NotRequested));
    let config = fa::StreamConfig {
        silence_probe: false,
        exclude_self: true,
        ..Default::default()
    };
    let request = crate::settings::Request {
        config: &config,
        ..request
    };
    let permissive = request.resolve(None, false).unwrap();
    assert!(permissive.has_ignored());
    let rejection = request.resolve(None, true).unwrap_err();
    assert_eq!(rejection.ignored_settings(), Some(&permissive));
}

#[test]
fn both_chunk_arrival_properties_alias_pts_exactly() {
    Python::initialize();
    Python::attach(|py| {
        for pts_ns in [i64::MIN, -1, 0, 123_456_789, i64::MAX] {
            let audio = chunk_to_py(fa::AudioChunk {
                data: Vec::new(),
                frames: 0,
                frame_index: 0,
                pts_ns,
                seq: 0,
                flags: fa::ChunkFlags::empty(),
                dropped_before: 0,
                peak: 0.0,
                rms: 0.0,
            });
            let secondary = crate::secondary::PySecondaryChunk::new(
                fa::SecondaryChunk {
                    samples: Vec::new(),
                    frames: 0,
                    frame_index: 0,
                    pts_ns,
                    seq: 0,
                    flags: fa::ChunkFlags::empty(),
                    dropped_before: 0,
                    peak: 0.0,
                    rms: 0.0,
                },
                crate::secondary::Encoding::S16,
            );
            let objects = [
                Py::new(py, audio).unwrap().into_any(),
                Py::new(py, secondary).unwrap().into_any(),
            ];
            for object in objects {
                let object = object.bind(py);
                assert_eq!(
                    object
                        .getattr("arrival_ns")
                        .unwrap()
                        .extract::<i64>()
                        .unwrap(),
                    pts_ns
                );
                assert_eq!(
                    object.getattr("pts_ns").unwrap().extract::<i64>().unwrap(),
                    pts_ns
                );
                assert!(object.setattr("arrival_ns", 1).is_err());
            }
        }
    });
}
