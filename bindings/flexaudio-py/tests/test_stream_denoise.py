"""Stream denoise validation stays independent of capture hardware."""
import unittest

import flexaudio as fa


class StreamDenoiseValidation(unittest.TestCase):
    def test_open_accepts_noncanonical_output_rates(self):
        for rate in (16000, 44100):
            for channels in (1, 2):
                with self.subTest(rate=rate, channels=channels):
                    with fa.open("mock", output_rate=rate,
                                 output_channels=channels, denoise=True) as stream:
                        entry = next(row for row in stream.effective_settings()["entries"]
                                     if row["setting"] == "denoise")
                        self.assertIs(entry["effective"], True)
                        self.assertEqual(entry["outcome"],
                                         {"status": "applied", "reason": None})

    def test_invalid_channel_error_is_unchanged(self):
        for channels in (0, 3):
            with self.subTest(channels=channels):
                with self.assertRaises(ValueError) as raised:
                    fa.open("mic", output_channels=channels, denoise=True)
                self.assertEqual(str(raised.exception),
                                 f"invalid channel count {channels} (expected 1 or 2)")
                self.assertFalse(hasattr(raised.exception, "audio_error"))


if __name__ == "__main__":
    unittest.main()
