"""Exercise the abi3 float32 input boundary through the installed Python wheel."""

import array
import sys
import unittest

import flexaudio

try:
    import numpy as np
except ImportError:
    np = None


class WhisperBufferTests(unittest.TestCase):
    def test_sequences_and_native_float32_buffers(self):
        pcm = array.array("f", [0.0] * 512)
        for samples in (list(pcm), tuple(pcm), pcm, memoryview(pcm),
                        memoryview(pcm).toreadonly()):
            with self.subTest(input_type=type(samples).__name__):
                vad = flexaudio.WhisperVad()
                self.assertEqual(vad.process(samples), [])
                self.assertEqual(len(vad.last_frame_probabilities().values), 1)
                self.assertEqual(vad.process(array.array("f")), [])

    def test_invalid_dtypes_and_layouts_are_typed_errors(self):
        pcm = array.array("f", [0.0] * 4)
        vad = flexaudio.WhisperVad()
        for samples in (array.array("d", [0.0]), array.array("h", [0]), b"\0" * 4,
                        memoryview(pcm)[::2],
                        memoryview(pcm).cast("B").cast("f", shape=[2, 2])):
            with self.subTest(samples=repr(samples)):
                with self.assertRaises(flexaudio.WhisperVadValidationError) as raised:
                    vad.process(samples)
                self.assertEqual(raised.exception.code, "InvalidPcm")
                self.assertEqual(raised.exception.terminal_events, [])
                self.assertIn("contiguous one-dimensional native float32", str(raised.exception))
        # A boundary rejection must not consume input or finish the session.
        self.assertEqual(vad.process(pcm), [])

    def test_nonfinite_float32_is_rejected(self):
        vad = flexaudio.WhisperVad()
        for value in (float("nan"), float("inf"), -float("inf")):
            with self.subTest(value=value):
                with self.assertRaises(flexaudio.WhisperVadValidationError) as raised:
                    vad.process(array.array("f", [value]))
                self.assertEqual(raised.exception.code, "InvalidPcm")
                self.assertEqual(raised.exception.terminal_events, [])

    def test_other_buffer_consumers(self):
        probabilities = array.array("f", [0.9] * 10 + [0.0] * 5)
        processor = flexaudio.WhisperVadPostProcessor()
        self.assertEqual(processor.process(memoryview(probabilities)), [])
        segments = processor.finish()
        self.assertEqual([(s.start_ms, s.end_ms) for s in segments], [(0, 350)])
        self.assertEqual(flexaudio.whisper_speech_segments(array.array("f", [0.0] * 513)), [])

    @unittest.skipIf(np is None, "NumPy is optional")
    def test_numpy_float32_and_rejected_layouts(self):
        vad = flexaudio.WhisperVad()
        pcm = np.zeros(512, dtype=np.float32)
        pcm.flags.writeable = False
        self.assertEqual(vad.process(pcm), [])
        self.assertEqual(len(vad.last_frame_probabilities().values), 1)
        opposite_endian = ">f4" if sys.byteorder == "little" else "<f4"
        for samples in (np.zeros(4, dtype=np.float64), pcm[::2], pcm.reshape(2, 256),
                        np.zeros(4, dtype=opposite_endian)):
            with self.subTest(dtype=str(samples.dtype), shape=samples.shape):
                with self.assertRaises(flexaudio.WhisperVadValidationError) as raised:
                    vad.process(samples)
                self.assertEqual(raised.exception.code, "InvalidPcm")


if __name__ == "__main__":
    unittest.main()
