use super::*;
use pyo3::ffi::c_str;
use pyo3::types::PyModule;

fn run(source: &std::ffi::CStr) {
    Python::initialize();
    Python::attach(|py| {
        let m = PyModule::new(py, "flexaudio").unwrap();
        register(&m).unwrap();
        let locals = PyDict::new(py);
        locals.set_item("fa", m).unwrap();
        py.run(source, None, Some(&locals)).unwrap();
    });
}

#[test]
fn strict_parameters_and_unknown_keys() {
    run(c_str!(
        r#"
for kwargs in [dict(threshold=1.0000000001), dict(threshold=float('nan')),
               dict(max_speech_ms=float('inf')), dict(min_speech_ms=-1),
               dict(min_speech_ms=134218), dict(min_speech_ms=True),
               dict(min_speech_ms=1.5), dict(provisional=1), dict(neg_threshold=0.3)]:
    try:
        fa.Vad(**kwargs)
        assert False, kwargs
    except (ValueError, TypeError):
        pass
params = fa.VadOptions(threshold=0, min_speech_ms=0, speech_pad_ms=0)
assert params.threshold == 0 and params.min_speech_ms == 0
try:
    params.threshold = 0.5
    assert False
except AttributeError:
    pass
"#
    ));
}
#[test]
fn float32_buffers_sequences_and_rejected_layouts() {
    run(c_str!(
        r#"
import array
vad = fa.Vad()
assert vad.process([0.0]*17) == []
assert vad.process(memoryview(array.array('f', [0.0]*495))) == []
assert len(vad.last_frame_probabilities().values) == 1
for data in [array.array('d', [0.0]), array.array('h', [0]),
             memoryview(array.array('f', [0.0]*4))[::2],
             memoryview(array.array('f', [0.0]*4)).cast('B').cast('f', shape=[2,2])]:
    try:
        vad.process(data)
        assert False
    except fa.VadValidationError as error:
        assert error.code == 'InvalidPcm' and error.terminal_events == []
        assert error.audio_error.kind == 'invalidArg'
        assert 'contiguous one-dimensional native float32' in error.audio_error.message
        assert 'contiguous one-dimensional native float32' in str(error)
assert vad.last_frame_probabilities().first_frame_index == 0
"#
    ));
}

#[test]
fn float32_buffer_copy_preserves_native_values_and_empty_input() {
    Python::initialize();
    Python::attach(|py| {
        let array = py.import("array").unwrap();
        let expected = [0.25_f32, -0.5, -0.0, f32::INFINITY, f32::NAN];
        let samples = array
            .call_method1("array", ("f", expected.to_vec()))
            .unwrap();
        let actual = crate::pcm_buffer::with_samples(&samples, |values| values.to_vec()).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
        let empty = array.call_method1("array", ("f",)).unwrap();
        assert!(crate::pcm_buffer::with_samples(&empty, |values| values.is_empty()).unwrap());
    });
}
#[test]
fn synthetic_tail_events_probabilities_owned_and_readonly() {
    run(c_str!(
        r#"
vad = fa.Vad(threshold=0, min_speech_ms=0, speech_pad_ms=0, provisional=True)
start = vad.process([0.0]*513)
assert len(start) == 1 and start[0].type == 'provisional_speech_start'
assert (start[0].epoch, start[0].seq, start[0].at_ms) == (0,0,0)
probs = vad.last_frame_probabilities()
assert probs.first_frame_index == 0 and len(probs.values) == 1
assert probs.values.readonly and probs.values.format == 'f'
end = vad.finish()
assert end[-1].type == 'epoch_end' and end[-1].reason == 'finish'
assert [e.seq for e in start+end] == list(range(len(start+end)))
assert [(e.start_ms,e.end_ms) for e in end if e.type == 'segment'] == [(0,60)]
assert any(e.type == 'provisional_speech_end' and e.at_ms == 32 for e in end)
assert vad.last_frame_probabilities().first_frame_index == 1
assert vad.finish() == []
try:
    vad.process([])
    assert False
except fa.VadRuntimeError as error:
    assert error.code == 'SessionFinished' and error.terminal_events == []
assert vad.reset() == []
assert len(probs.values) == 1
try:
    vad.process([float('nan')])
    assert False
except fa.VadValidationError as error:
    assert error.code == 'InvalidPcm' and error.terminal_events == []
"#
    ));
}
#[test]
fn processor_segment_marshalling_and_reset() {
    run(c_str!(
        r#"
p = fa.VadPostProcessor()
assert p.process([0.9]*10+[0.0]*5) == []
segments=p.finish()
assert len(segments)==1 and (segments[0].start_ms,segments[0].end_ms)==(0,350)
assert p.finish() == []
p.reset()
assert p.process([]) == [] and p.finish() == []
assert fa.speech_segments([0.0]*513)==[]
"#
    ));
}
#[test]
fn fatal_exception_carries_typed_closure() {
    Python::initialize();
    Python::attach(|py| {
        let failure = VadFailure {
            error: VadError::Inference,
            terminal_events: vec![flexaudio_vad::VadEvent {
                epoch: 7,
                seq: 99,
                kind: flexaudio_vad::VadEventKind::EpochEnd {
                    reason: flexaudio_vad::EpochEndReason::Error,
                },
            }],
        };
        let error = failure_to_py(py, failure);
        assert!(error.is_instance_of::<VadRuntimeError>(py));
        let events = error.value(py).getattr("terminal_events").unwrap();
        let e = events.get_item(0).unwrap();
        assert_eq!(
            e.getattr("type").unwrap().extract::<String>().unwrap(),
            "epoch_end"
        );
        assert_eq!(e.getattr("seq").unwrap().extract::<u64>().unwrap(), 99);
    });
}

#[test]
fn attached_epoch_origin_is_readonly_and_preserves_u64() {
    Python::initialize();
    Python::attach(|py| {
        let event = marshal::attached_to_py(
            py,
            flexaudio_vad::AttachedVadEvent::EpochStart {
                epoch: 3,
                seq: 0,
                capture_sample: 9_007_199_254_740_993,
                pts_ns: 123,
            },
        )
        .unwrap();
        let event = event.bind(py);
        assert_eq!(
            event.getattr("type").unwrap().extract::<String>().unwrap(),
            "epoch_start"
        );
        assert_eq!(
            event
                .getattr("capture_sample")
                .unwrap()
                .extract::<u64>()
                .unwrap(),
            9_007_199_254_740_993
        );
        assert!(event.setattr("capture_sample", 0).is_err());
        StreamVadOptions::new(py, None, false, "primary").unwrap();
        assert!(StreamVadOptions::new(py, None, false, "secondary").is_err());
    });
}

#[test]
fn standalone_accepts_arbitrary_input_format_and_integer_limits() {
    run(c_str!(
        r#"
vad = fa.Vad(max_speech_ms=900)
assert vad.process([0.0] * 3072, input_sample_rate=48000, input_channels=2) == []
assert vad.finish()[-1].type == 'epoch_end'
assert fa.VadOptions(max_speech_ms=4294967295).max_speech_ms == 4294967295
for invalid in [0.9, True, -1, 4294967296]:
    try:
        fa.Vad(max_speech_ms=invalid)
        assert False, invalid
    except (ValueError, TypeError):
        pass
"#
    ));
}
