"""Hardware-free settings boundaries and additive capture declarations."""
import inspect
import unittest

import flexaudio as fa

from mock_capture import skip_unless_mock_capture


SETTING_KEYS = {
    "kind", "deviceId", "targetPid", "mode", "chunkMs", "ringCapacityChunks",
    "excludeSelf", "excludePids", "output", "secondaryOutput", "secondaryEncoding",
    "gain", "mixMicDeviceId", "mixSystemDeviceId", "mixMicGain", "mixSystemGain",
    "silenceProbe", "windowsClassicLoopbackKeepalive", "denoise", "vad",
}


class SettingsLifecycleContracts(unittest.TestCase):
    # Successful capture fixtures require a wheel built with --features test-faults.
    @skip_unless_mock_capture
    def test_denoise_opens_at_noncanonical_output_rates(self):
        for rate in (16000, 44100, 48000):
            with self.subTest(rate=rate), fa.open("mock", output_rate=rate,
                                                output_channels=1, denoise=True) as stream:
                entry = self.denoise_entry(stream)
                self.assertIs(entry["requested"], True)
                self.assertIs(entry["effective"], True)
                self.assertEqual(entry["outcome"], {"status": "applied", "reason": None})
                output = next(row for row in stream.effective_settings()["entries"]
                              if row["setting"] == "output")
                self.assertEqual(output["effective"], (rate, 1))
                self.assertEqual(output["outcome"], {"status": "applied", "reason": None})

    @skip_unless_mock_capture
    def test_switch_enables_denoise_at_noncanonical_output_rates(self):
        for rate in (16000, 44100):
            with self.subTest(rate=rate), fa.open("mock", output_rate=rate) as stream:
                stream.switch_source("mock", output_rate=rate, denoise=True)
                entry = self.denoise_entry(stream)
                self.assertIs(entry["requested"], True)
                self.assertIs(entry["effective"], True)
                self.assertEqual(entry["outcome"], {"status": "applied", "reason": None})
                stream.switch_source("mock", output_rate=rate)
                self.assertIs(self.denoise_entry(stream)["effective"], True)

    @skip_unless_mock_capture
    def test_switch_omitted_denoise_retains_enabled_setting(self):
        with fa.open("mock", denoise=True) as stream:
            self.assertIs(self.denoise_entry(stream)["effective"], True)
            for options in ({}, {"denoise": None}):
                stream.switch_source("mock", **options)
                entry = self.denoise_entry(stream)
                self.assertIs(entry["requested"], False)
                self.assertIs(entry["effective"], True)
                self.assertEqual(entry["outcome"], {"status": "notRequested", "reason": None})

    @skip_unless_mock_capture
    def test_switch_explicit_false_disables_denoise(self):
        with fa.open("mock", denoise=True) as stream:
            self.assertIs(self.denoise_entry(stream)["effective"], True)
            stream.switch_source("mock", denoise=False)
            entry = self.denoise_entry(stream)
            self.assertIs(entry["requested"], False)
            self.assertIs(entry["effective"], False)
            self.assertEqual(entry["outcome"], {"status": "notRequested", "reason": None})
            stream.switch_source("mock", denoise=None)
            self.assertIs(self.denoise_entry(stream)["effective"], False)
            stream.switch_source("mock", denoise=True)
            self.assertIs(self.denoise_entry(stream)["effective"], True)

    @skip_unless_mock_capture
    def test_switch_omitted_denoise_retains_disabled_setting(self):
        with fa.open("mock", denoise=False) as stream:
            self.assertIs(self.denoise_entry(stream)["effective"], False)
            for options in ({}, {"denoise": None}):
                stream.switch_source("mock", **options)
                entry = self.denoise_entry(stream)
                self.assertIs(entry["requested"], False)
                self.assertIs(entry["effective"], False)
                self.assertEqual(entry["outcome"], {"status": "notRequested", "reason": None})

    @staticmethod
    def denoise_entry(stream: fa.Stream):
        report = stream.effective_settings()
        return next(row for row in report["entries"] if row["setting"] == "denoise")

    def test_open_defaults_to_strict(self):
        for strict in ({}, {"strict": None}):
            with self.subTest(strict=strict), self.assertRaises(fa.IgnoredSettingsError) as raised:
                fa.open("mic", exclude_self=True, **strict)
            rows = {row["setting"]: row for row in raised.exception.report["entries"]}
            self.assertEqual(rows["excludeSelf"]["outcome"]["status"], "ignored")

    def test_explicit_false_reaches_backend_with_ignored_settings(self):
        # An impossible selector makes this hardware-independent: permissive mode
        # reaches native selection, which must fail with a different typed error.
        options = {"device_id": "nonexistent-input-for-settings-test", "exclude_self": True}
        with self.assertRaises(fa.IgnoredSettingsError):
            fa.open("mic", **options)
        with self.assertRaises((fa.DeviceNotFoundError, fa.RecordingPermissionError,
                                fa.BackendError)) as raised:
            fa.open("mic", strict=False, **options)
        self.assertNotIsInstance(raised.exception, fa.IgnoredSettingsError)

    def test_parameters_are_appended_defaulted_and_keyword_only(self):
        for function in (fa.open, fa.Stream.switch_source):
            parameters = inspect.signature(function).parameters
            self.assertEqual(list(parameters)[-2:], ["strict", "ring_capacity_chunks"])
            for name in ("strict", "ring_capacity_chunks"):
                self.assertIs(parameters[name].default, None)
                self.assertEqual(parameters[name].kind, inspect.Parameter.KEYWORD_ONLY)

    def test_ring_capacity_rejects_invalid_values_before_capture(self):
        for value in (True, False, 1.5, "50", object()):
            with self.subTest(value=value), self.assertRaises(TypeError):
                fa.open("mic", ring_capacity_chunks=value)
        for value in (0, -1, 2 ** 256):
            with self.subTest(value=value), self.assertRaises(ValueError):
                fa.open("mic", ring_capacity_chunks=value)

    def test_strict_reports_all_settings_before_device_or_model_access(self):
        with self.assertRaises(fa.IgnoredSettingsError) as raised:
            fa.open("mic", strict=True, silence_probe=False, exclude_self=True,
                    vad=fa.StreamVadOptions(),
                    secondary_output=fa.SecondaryOutputOptions(rate=16000, channels=1, encoding="s16"))
        error = raised.exception
        self.assertIsInstance(error, ValueError)
        self.assertEqual(set(error.report), {"revision", "entries"})
        self.assertEqual(error.report["revision"], 0)
        rows = {row["setting"]: row for row in error.report["entries"]}
        self.assertEqual(set(rows), SETTING_KEYS)
        self.assertEqual(len(error.report["entries"]), len(SETTING_KEYS))
        for row in rows.values():
            self.assertEqual(set(row), {"setting", "requested", "effective", "outcome"})
            self.assertEqual(set(row["outcome"]), {"status", "reason"})
        for key in ("silenceProbe", "excludeSelf"):
            self.assertEqual(rows[key]["outcome"], {"status": "ignored", "reason": "wrongSourceKind"})
            self.assertIsNone(rows[key]["effective"])
        self.assertEqual(rows["output"]["effective"], (48000, 2))
        self.assertEqual(rows["secondaryOutput"]["effective"], (16000, 1))
        self.assertEqual(rows["secondaryEncoding"]["effective"], "s16")
        self.assertIs(rows["vad"]["effective"], True)
        with self.assertRaises(AttributeError):
            error.report = {}

    def test_explicit_defaults_are_not_requested(self):
        # One ignored request forces a device-free report of all other default values.
        with self.assertRaises(fa.IgnoredSettingsError) as raised:
            fa.open("mic", strict=True, mic_gain=2.0, gain=1.0,
                    silence_probe=True, windows_classic_loopback_keepalive=True,
                    ring_capacity_chunks=50, exclude_self=False, exclude_pids=[])
        rows = {row["setting"]: row for row in raised.exception.report["entries"]}
        for key in SETTING_KEYS - {"mixMicGain"}:
            self.assertEqual(rows[key]["outcome"], {"status": "notRequested", "reason": None})

    def test_invalid_arguments_precede_strict_rejection(self):
        # A missing process ID supplies a typed error before any device is acquired.
        for strict in ({}, {"strict": None}, {"strict": False}, {"strict": True}):
            with self.subTest(strict=strict), self.assertRaises(fa.InvalidArgumentError):
                fa.open("process", device_id="unused", **strict)

    def test_additive_exports(self):
        for name in ("effective_settings", "poll_lifecycle_event"):
            self.assertTrue(callable(getattr(fa.Stream, name)))
        for cls in (fa.AudioChunk, fa.SecondaryChunk):
            self.assertTrue(hasattr(cls, "arrival_ns"))
        self.assertTrue(callable(fa.devices_with_namespaces))
        for name in ("type", "reason", "message"):
            self.assertTrue(hasattr(fa.LifecycleEvent, name))
        for name in ("device", "id_namespace"):
            self.assertTrue(hasattr(fa.NamespacedDevice, name))


if __name__ == "__main__":
    unittest.main()
