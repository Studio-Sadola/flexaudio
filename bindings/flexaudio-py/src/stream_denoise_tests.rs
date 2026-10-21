//! Stream output parity with canonical facade processing, including graceful tails.
use super::*;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Sink = Arc<Mutex<Option<fa::core::backend::RawSink>>>;
struct Input(Sink);
impl fa::CaptureBackend for Input {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: fa::core::backend::RawSink) -> fa::Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

pub(super) fn binding(inner: fa::Stream) -> Stream {
    Stream {
        secondary: inner.config().secondary_output.map(|format| {
            crate::secondary::SecondaryOutputOptions {
                format,
                encoding: crate::secondary::Encoding::F32,
            }
        }),
        inner,
        ready_secondary: Default::default(),
        vad_secondary: false,
        secondary_metadata: (0, 0, 0, 0),
        shutdown: None,
        local_events: Default::default(),
        lifecycle_events: Default::default(),
        capture_events: Default::default(),
        settings: Default::default(),
        vad_tap: None,
        vad_events: Vec::new(),
        vad_origin: (0, 0),
        vad_error: None,
        vad_cleanup: Vec::new(),
        ready_chunks: Default::default(),
    }
}

#[test]
fn stream_denoise_matches_facade_before_downmix_and_gain_on_both_taps() {
    let input: Vec<f32> = (0..9600)
        .flat_map(|i| {
            let t = i as f32 / 48_000.0;
            [
                0.3 * (std::f32::consts::TAU * 440.0 * t).sin(),
                0.15 * (std::f32::consts::TAU * 750.0 * t).sin(),
            ]
        })
        .collect();
    for channels in [1, 2] {
        for gain in [1.0, 0.5] {
            let output = fa::OutputFormat {
                sample_rate: 48_000,
                channels,
            };
            let mut config = fa::StreamConfig::default();
            config.output = output;
            config.secondary_output = Some(output);
            config.gain = gain;
            let facade_sink = Sink::default();
            let mut facade =
                fa::Stream::open(config.clone(), Box::new(Input(facade_sink.clone()))).unwrap();
            facade.set_denoise(true);
            facade.start().unwrap();
            let binding_sink = Sink::default();
            let inner = fa::Stream::open(config, Box::new(Input(binding_sink.clone()))).unwrap();
            // Exercise the same pre-start setup as Python open(denoise=True).
            let mut stream = binding(start_capture(inner, true, false).unwrap());
            for sink in [&facade_sink, &binding_sink] {
                assert_eq!(
                    sink.lock().unwrap().as_mut().unwrap().push(&input, 0),
                    input.len()
                );
            }
            let mut expected = Vec::new();
            let mut actual = Vec::new();
            let mut secondary = Vec::new();
            let samples = 9600 * usize::from(channels);
            let deadline = Instant::now() + Duration::from_secs(5);
            while expected.len() < samples || actual.len() < samples || secondary.len() < samples {
                if let Some(chunk) = facade.poll_chunk() {
                    expected.extend(chunk.data);
                }
                if let Some(chunk) = stream.poll_chunk().unwrap() {
                    actual.extend_from_slice(chunk.samples());
                }
                if let Some(chunk) = stream.poll_secondary().unwrap() {
                    secondary.extend(chunk.chunk.samples);
                }
                assert!(Instant::now() < deadline, "denoise output timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
            facade.stop_checked().unwrap();
            stream.stop().unwrap();
            while let Some(chunk) = facade.poll_chunk() {
                expected.extend(chunk.data);
            }
            while let Some(chunk) = stream.poll_chunk().unwrap() {
                actual.extend_from_slice(chunk.samples());
            }
            while let Some(chunk) = stream.poll_secondary().unwrap() {
                secondary.extend(chunk.chunk.samples);
            }
            assert_eq!(
                actual.len(),
                (9600_usize + 480).div_ceil(960) * 960 * usize::from(channels)
            );
            assert!(
                actual.iter().any(|s| s.abs() > 1e-6),
                "fixture must contain a nonzero denoised signal"
            );
            assert_eq!(actual, expected, "channels={channels}, gain={gain}");
            assert_eq!(secondary, actual, "both taps must share canonical denoise");
        }
    }
}

#[test]
fn switch_denoise_rejects_non_48khz_output_as_value_error() {
    Python::initialize();
    Python::attach(|py| {
        let sink = Sink::default();
        let mut config = fa::StreamConfig::default();
        config.output.sample_rate = 16_000;
        let inner = fa::Stream::open(config, Box::new(Input(sink))).unwrap();
        let object = Py::new(py, binding(start_capture(inner, false, false).unwrap())).unwrap();
        let locals = pyo3::types::PyDict::new(py);
        locals.set_item("stream", object).unwrap();
        py.run(
            pyo3::ffi::c_str!(
                r#"
try:
    stream.switch_source('mic', output_rate=16000, denoise=True)
except ValueError as error:
    assert 'denoise requires 48000 Hz output (output_rate=16000)' in str(error)
else:
    raise AssertionError('non-48 kHz denoise was accepted')
stream.stop()
"#
            ),
            None,
            Some(&locals),
        )
        .unwrap();
    });
}
