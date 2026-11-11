"""Input monitoring through the real binding with device-free capture."""
import time
import unittest

import flexaudio as fa
from mock_capture import skip_unless_mock_capture


class LevelContracts(unittest.TestCase):
    def wait_level(self, stream):
        deadline = time.monotonic() + 3
        while True:
            value = stream.poll_level()
            if value is not None:
                return value
            self.assertLess(time.monotonic(), deadline, "flowing input must refresh the level")
            time.sleep(0.005)

    @skip_unless_mock_capture
    def test_paused_levels_persist_without_pcm_or_vad_output(self):
        with fa.open("mock", gain=0.5, denoise=True, vad=fa.StreamVadOptions()) as stream:
            self.wait_level(stream)
            stream.pause()
            while stream.poll_chunk() is not None:
                pass  # The existing contract keeps pre-pause output drainable.
            level = self.wait_level(stream)
            self.assertIsInstance(level, fa.Level)
            # The binding's mock source intentionally sends digital silence.
            # Valid zero samples must still publish a level, rather than None.
            self.assertEqual((level.peak, level.rms), (0, 0))
            stream.set_gain(0)
            stream.pause()  # Require a fresh measurement at mute, without polling PCM.
            muted = self.wait_level(stream)
            self.assertEqual((muted.peak, muted.rms), (0, 0))
            self.assertIsInstance(level.age_ns, int)
            with self.assertRaises(AttributeError):
                level.peak = 1
            for _ in range(100):
                self.assertIsNotNone(stream.poll_level())
                self.assertIsNone(stream.poll_chunk())
            time.sleep(0.1)
            self.assertIsNotNone(stream.poll_level())
            self.assertIsNone(stream.poll_chunk())
        self.assertIsNone(stream.poll_level())

    @skip_unless_mock_capture
    def test_zero_gain_and_source_switch_keep_monitoring(self):
        with fa.open("mock", gain=0) as stream:
            stream.pause()
            first = self.wait_level(stream)
            self.assertEqual((first.peak, first.rms), (0, 0))
            stream.switch_source("mock")
            next_level = self.wait_level(stream)
            self.assertEqual((next_level.peak, next_level.rms), (0, 0))


if __name__ == "__main__":
    unittest.main()
