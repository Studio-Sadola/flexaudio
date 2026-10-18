"""Hardware-free capability and secondary option boundary contracts."""
import inspect
import unittest

import flexaudio as fa


class CapabilitiesAndSecondary(unittest.TestCase):
    def test_capabilities_are_snake_case_closed_records(self):
        value = fa.capabilities()
        self.assertEqual(set(value), {"os_version", "sources", "system_exclusions", "process", "device_watcher", "side_effects"})
        self.assertEqual(set(value["sources"]), {"mic", "system", "process", "mix"})
        for source in value["sources"].values():
            self.assertEqual(set(source), {"backend", "availability"})
            self.assertIn(source["backend"], {"cpal", "pipeWire", "wasapiLoopback", "wasapiProcessLoopback", "coreAudioProcessTap", "composite", "unsupported", "other"})
            availability = source["availability"]
            status = availability["status"]
            self.assertIn(status, {"supported", "unavailable", "unknown"})
            if status == "unknown":
                self.assertIn("kind", availability["error"])
                self.assertIn("contexts", availability["error"])
            elif status == "unavailable":
                self.assertIn(availability["reason"]["kind"], {"unsupportedOs", "osVersionTooOld", "pipeWireUnavailable", "notApplicable", "other"})
        self.assertEqual(set(value["system_exclusions"]), {"availability", "backend", "scope", "max_distinct_pids", "device_selection"})
        self.assertEqual(set(value["process"]), {"include_scope", "exclude_scope"})
        self.assertEqual(set(value["device_watcher"]), {"availability", "mode", "scopes"})
        for effect in value["side_effects"].values():
            self.assertEqual(set(effect), {"availability", "enabled_by_default", "sources", "condition"})

    def test_keepalive_defaults_and_keyword_only_contract(self):
        for entry, default in [(fa.open, True), (fa.Stream.switch_source, None)]:
            parameter = inspect.signature(entry).parameters["windows_classic_loopback_keepalive"]
            self.assertIs(parameter.default, default)
            self.assertIs(parameter.kind, inspect.Parameter.KEYWORD_ONLY)

    def test_secondary_options_validate_before_device_access(self):
        options = fa.SecondaryOutputOptions(rate=16000, channels=1)
        self.assertEqual((options.rate, options.channels, options.encoding), (16000, 1, "f32"))
        self.assertEqual(fa.SecondaryOutputOptions(rate=48000, channels=2, encoding="s16").encoding, "s16")
        for kwargs in [dict(rate=0, channels=1), dict(rate=16000, channels=3), dict(rate=16000, channels=1, encoding="invalid")]:
            with self.assertRaises(ValueError):
                fa.SecondaryOutputOptions(**kwargs)
        with self.assertRaises(AttributeError):
            options.rate = 48000
        with self.assertRaises(fa.VadValidationError) as raised:
            fa.open("mic", vad=fa.StreamVadOptions(tap="secondary"))
        self.assertEqual(raised.exception.code, "UnsupportedTap")
        with self.assertRaises(ValueError):
            fa.StreamVadOptions(tap="invalid")


if __name__ == "__main__":
    unittest.main()
