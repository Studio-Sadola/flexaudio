//! Legacy opens and both primary poll APIs share the facade denoise path.
use super::*;
use std::{
    mem, ptr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Sink = Arc<Mutex<Option<flexaudio::core::backend::RawSink>>>;
struct Input(Sink);
impl flexaudio::CaptureBackend for Input {
    fn native_format(&self) -> (u32, u16) {
        (48_000, 2)
    }
    fn start(&mut self, sink: flexaudio::core::backend::RawSink) -> flexaudio::Result<()> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

unsafe fn poll(stream: *mut FlexStream, v2: bool, samples: &mut Vec<f32>) -> bool {
    if v2 {
        let mut chunk = mem::zeroed();
        let status = vad_integration::flexaudio_poll_chunk_v2(stream, &mut chunk);
        assert!(status >= 0);
        if status == 0 {
            return false;
        }
        samples.extend_from_slice(std::slice::from_raw_parts(
            chunk.chunk.data,
            chunk.chunk.len,
        ));
        vad_integration::flexaudio_chunk_free_v2(&mut chunk);
    } else {
        let mut chunk = mem::zeroed();
        let status = flexaudio_poll_chunk(stream, &mut chunk);
        assert!(status >= 0);
        if status == 0 {
            return false;
        }
        samples.extend_from_slice(std::slice::from_raw_parts(chunk.data, chunk.len));
        flexaudio_chunk_free(&mut chunk);
    }
    true
}

#[test]
fn legacy_stream_denoise_matches_facade_before_downmix_and_gain() {
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
            for v2 in [false, true] {
                unsafe {
                    let config = FlexConfig {
                        output_rate: 48_000,
                        output_channels: channels,
                        gain,
                        denoise: 1,
                        ..mem::zeroed()
                    };
                    let sink = Sink::default();
                    let handle = open_with_exclude_pids_using(&config, ptr::null(), 0, |config| {
                        flexaudio::Stream::open(config, Box::new(Input(sink.clone())))
                    });
                    assert!(!handle.is_null());
                    assert_eq!(flexaudio_start(handle), 0);
                    let facade_sink = Sink::default();
                    let mut facade = flexaudio::Stream::open(
                        convert::build_config(&config, Vec::new()).unwrap(),
                        Box::new(Input(facade_sink.clone())),
                    )
                    .unwrap();
                    facade.set_denoise(true);
                    facade.start().unwrap();
                    for sink in [&sink, &facade_sink] {
                        assert_eq!(
                            sink.lock().unwrap().as_mut().unwrap().push(&input, 0),
                            input.len()
                        );
                    }
                    let mut expected = Vec::new();
                    let mut actual = Vec::new();
                    let samples = 9600 * usize::from(channels);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while expected.len() < samples || actual.len() < samples {
                        if let Some(chunk) = facade.poll_chunk() {
                            expected.extend(chunk.data);
                        }
                        poll(handle, v2, &mut actual);
                        assert!(Instant::now() < deadline, "denoise output timed out");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    facade.stop_checked().unwrap();
                    assert_eq!(flexaudio_stop(handle), 0);
                    while let Some(chunk) = facade.poll_chunk() {
                        expected.extend(chunk.data);
                    }
                    while poll(handle, v2, &mut actual) {}
                    flexaudio_free(handle);
                    assert_eq!(
                        actual.len(),
                        (9600_usize + 480).div_ceil(960) * 960 * usize::from(channels)
                    );
                    assert!(actual.iter().any(|s| s.abs() > 1e-6));
                    assert_eq!(
                        actual, expected,
                        "channels={channels}, gain={gain}, v2={v2}"
                    );
                }
            }
        }
    }
}

#[test]
fn legacy_v1_denoise_accepts_noncanonical_output_rates() {
    legacy_denoise_accepts_noncanonical_output_rates(false);
}

#[test]
fn legacy_v2_denoise_accepts_noncanonical_output_rates() {
    legacy_denoise_accepts_noncanonical_output_rates(true);
}

fn legacy_denoise_accepts_noncanonical_output_rates(v2: bool) {
    for rate in [16_000, 44_100] {
        unsafe {
            let config = FlexConfig {
                output_rate: rate,
                output_channels: 1,
                denoise: 1,
                ..mem::zeroed()
            };
            let open = |decoded: flexaudio::StreamConfig, options: flexaudio::OpenOptions| {
                let report = flexaudio::resolve_settings(
                    &flexaudio::SettingsRequest::new(decoded.clone()).open_options(&options),
                )?;
                let denoise = report.get(flexaudio::SettingKey::Denoise).unwrap();
                assert_eq!(denoise.requested, flexaudio::SettingValue::Bool(true));
                assert_eq!(denoise.effective, flexaudio::SettingValue::Bool(true));
                assert_eq!(denoise.outcome, flexaudio::SettingOutcome::Applied);
                let output = report.get(flexaudio::SettingKey::Output).unwrap();
                assert_eq!(
                    output.effective,
                    flexaudio::SettingValue::Format(decoded.output)
                );
                assert_eq!(output.outcome, flexaudio::SettingOutcome::Applied);
                flexaudio::Stream::open(decoded, Box::new(Input(Sink::default())))
            };
            let stream = if v2 {
                let envelope = vad_integration::FlexStreamConfigV2 {
                    size: u32::try_from(mem::size_of::<vad_integration::FlexStreamConfigV2>())
                        .unwrap(),
                    version: 2,
                    config: &config,
                    vad: ptr::null(),
                };
                let mut out = ptr::null_mut();
                assert_eq!(
                    vad_integration::open_v2_with(&envelope, &mut out, open),
                    code::FLEX_OK
                );
                out
            } else {
                open_with_exclude_pids_options(&config, ptr::null(), 0, open)
            };
            assert!(!stream.is_null());
            assert_eq!((*stream).inner.config().output.sample_rate, rate);
            assert_eq!(flexaudio_start(stream), code::FLEX_OK);
            assert_eq!(flexaudio_stop(stream), code::FLEX_OK);
            flexaudio_free(stream);
        }
    }
}
