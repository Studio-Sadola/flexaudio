"""Declaration regressions for the approved closed 0.5 binding contracts."""
import ast
from pathlib import Path
import unittest


class DeclarationContracts(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tree = ast.parse((Path(__file__).parent.parent / "flexaudio.pyi").read_text())
        cls.classes = {node.name: node for node in cls.tree.body if isinstance(node, ast.ClassDef)}
        cls.aliases = {
            node.targets[0].id: node.value
            for node in cls.tree.body
            if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name)
        }

    def members(self, alias):
        value = self.aliases[alias]
        self.assertIsInstance(value, ast.Subscript)
        self.assertEqual(value.value.id, "Union")
        return {node.id for node in value.slice.elts}

    def fields(self, name):
        fields = {}
        for base in self.classes[name].bases:
            if isinstance(base, ast.Name) and base.id in self.classes:
                fields.update(self.fields(base.id))
        fields.update({
            node.target.id: ast.unparse(node.annotation)
            for node in self.classes[name].body
            if isinstance(node, ast.AnnAssign)
        })
        return fields

    def test_stream_events_are_closed_and_complete(self):
        expected = {
            "ChunkDroppedEventDict": ("chunkDropped", "count"),
            "StalledEventDict": ("stalled", None),
            "RecoveredEventDict": ("recovered", None),
            "PermissionDeniedEventDict": ("permissionDenied", "permission"),
            "PermissionPendingEventDict": ("permissionPending", "permission"),
            "PermissionGrantedEventDict": ("permissionGranted", "permission"),
            "SilenceWhileSourceActiveEventDict": ("silenceWhileSourceActive", "message"),
            "DeviceLostEventDict": ("deviceLost", None),
            "LegacyErrorEventDict": ("error", "message"),
            "TerminalErrorEventDict": ("terminalError", "error"),
            "RecoverableErrorEventDict": ("recoverableError", "error"),
            "ShutdownErrorEventDict": ("shutdownError", "error"),
            "AudioLossEventDict": ("audioLoss", "loss"),
            "ClippedEventDict": ("clipped", None),
            "UnknownEventDict": ("unknown", "message"),
        }
        self.assertEqual(self.members("StreamEventDict"), set(expected))
        for name, (tag, payload) in expected.items():
            fields = self.fields(name)
            self.assertEqual(fields["type"], f"Literal['{tag}']")
            if payload is not None:
                self.assertIn(payload, fields)
            self.assertFalse(any(field.startswith("Optional[") for field in fields.values()))
        self.assertEqual(self.fields("PermissionGrantedEventDict")["permission"], "Literal['microphone']")

    def test_device_events_cannot_fabricate_default_id_or_source(self):
        self.assertEqual(self.members("DeviceEventDict"), {
            "DeviceAddedEventDict", "DeviceRemovedEventDict", "DefaultChangedEventDict",
            "DefaultClearedEventDict", "RescanRequiredEventDict", "UnknownEventDict",
        })
        self.assertEqual(set(self.fields("DefaultClearedEventDict")), {"type", "source_kind"})
        self.assertEqual(self.fields("DefaultClearedEventDict")["source_kind"], "Literal['mic', 'system']")
        self.assertEqual(set(self.fields("RescanRequiredEventDict")), {"type", "dropped_events"})

    def test_error_union_has_one_arm_per_root_and_specific_payloads(self):
        expected = {
            "InvalidArgument", "InvalidState", "DeviceNotFound", "RecordingPermission",
            "UnsupportedOsVersion", "DeviceLost", "Backend", "UnsupportedFormat",
            "NativeFormatChanged", "Unsupported", "AmbiguousDeviceName",
        }
        self.assertEqual(self.members("AudioErrorDict"), {name + "ErrorDict" for name in expected})
        for name in expected:
            fields = self.fields(name + "ErrorDict")
            self.assertIn("kind", fields)
            self.assertEqual("permission" in fields, name == "RecordingPermission")
            self.assertEqual("advertised" in fields, name == "NativeFormatChanged")
            self.assertEqual("actual" in fields, name == "NativeFormatChanged")
        self.assertEqual(set(self.fields("ErrorContextDict")), {"operation", "lane", "native_status"})
        self.assertEqual(set(self.fields("AudioLossDict")), {"path", "reason", "samples", "sample_rate", "channels"})
        self.assertEqual(set(self.fields("ShutdownReportDict")), {"primary", "cleanup_errors"})

    def test_backend_whisper_code_is_optional_and_exclusive(self):
        # Execute only the declarations, so this test needs no native extension.
        end = next(
            index for index, node in enumerate(self.tree.body)
            if isinstance(node, ast.Assign)
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id == "AudioErrorDict"
        )
        declarations = ast.Module(body=self.tree.body[:end + 1], type_ignores=[])
        namespace = {}
        exec(compile(declarations, "flexaudio.pyi", "exec"), namespace)
        for name in self.members("AudioErrorDict"):
            fields = self.fields(name)
            self.assertEqual("whisper_code" in fields, name == "BackendErrorDict")
        backend = namespace["BackendErrorDict"]
        self.assertEqual(self.fields("BackendErrorDict")["whisper_code"], "WhisperCode")
        self.assertEqual(backend.__optional_keys__, {"whisper_code"})
        self.assertEqual(backend.__required_keys__, {"kind", "message", "contexts", "secondary"})


if __name__ == "__main__":
    unittest.main()
