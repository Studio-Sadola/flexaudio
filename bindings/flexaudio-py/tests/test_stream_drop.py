"""Stop failures first discovered by garbage collection remain observable."""
import gc
import sys
import os

import flexaudio
import pytest


@pytest.mark.skipif(not hasattr(flexaudio, "_stream_stop_failure"), reason="requires test-faults wheel")
@pytest.mark.parametrize("mode", ["drop", "stop", "context"])
def test_stream_drop_reports_only_unobserved_stop_failure(mode):
    observed = []
    previous = sys.unraisablehook
    sys.unraisablehook = observed.append
    try:
        stream = flexaudio._stream_stop_failure()
        if mode == "stop":
            with pytest.raises(RuntimeError) as stopped:
                stream.stop()
            assert stopped.value.audio_error.kind == "deviceLost"
        elif mode == "context":
            with pytest.raises(RuntimeError) as stopped:
                with stream:
                    pass
            assert stopped.value.audio_error.kind == "deviceLost"
        del stream
        gc.collect()
    finally:
        sys.unraisablehook = previous
    assert len(observed) == (1 if mode == "drop" else 0)
    if mode == "drop":
        assert observed[0].exc_value.audio_error.kind == "deviceLost"


@pytest.mark.skipif(sys.platform != "linux", reason="requires procfs pipe descriptor access")
@pytest.mark.parametrize("explicit", [False, True])
def test_encoder_drop_reports_only_unobserved_finalize_failure(explicit):
    reader, writer = os.pipe()
    observed = []
    previous = sys.unraisablehook
    sys.unraisablehook = observed.append
    try:
        encoder = flexaudio.FlacEncoder(f"/proc/self/fd/{writer}", 16000, 1, split_seconds=0)
        encoder.write_chunk([0.0])
        if explicit:
            with pytest.raises(OSError) as failure:
                encoder.finalize()
            assert failure.value.audio_error.addon_kind == "flacIo"
        del encoder
        gc.collect()
    finally:
        sys.unraisablehook = previous
        os.close(writer)
        os.close(reader)
    assert len(observed) == (0 if explicit else 1)
    if not explicit:
        assert observed[0].exc_value.audio_error.addon_kind == "flacIo"
