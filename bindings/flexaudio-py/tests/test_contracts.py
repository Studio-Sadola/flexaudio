"""Declaration regressions for closed public binding contracts."""
import ast
from pathlib import Path
import re
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
            "SilenceWhileSourceActiveEventDict": ("silenceWhileSourceActive", "message"),
            "DeviceLostEventDict": ("deviceLost", None),
            "LegacyErrorEventDict": ("error", "message"),
            "TerminalErrorEventDict": ("terminalError", "error"),
            "RecoverableErrorEventDict": ("recoverableError", "error"),
            "ShutdownErrorEventDict": ("shutdownError", "error"),
            "AudioLossEventDict": ("audioLoss", "loss"),
            "ClippedEventDict": ("clipped", None),
            "StartupSamplesDroppedEventDict": ("startupSamplesDropped", "frames"),
            "UnknownEventDict": ("unknown", "message"),
        }
        self.assertEqual(self.members("StreamEventDict"), set(expected))
        for name, (tag, payload) in expected.items():
            fields = self.fields(name)
            self.assertEqual(fields["type"], f"Literal['{tag}']")
            if payload is not None:
                self.assertIn(payload, fields)
            self.assertFalse(any(field.startswith("Optional[") for field in fields.values()))

    def test_startup_drop_payload_is_typed(self):
        self.assertEqual(self.fields("StartupSamplesDroppedEventDict")["frames"], "int")
        self.assertEqual(self.fields("StartupSamplesDroppedEventDict")["reason"], "Literal['outOfRange']")

    def test_device_events_cannot_fabricate_default_id_or_source(self):
        self.assertEqual(self.members("DeviceEventDict"), {
            "DeviceAddedEventDict", "DeviceRemovedEventDict", "DefaultChangedEventDict",
            "DefaultClearedEventDict", "RescanRequiredEventDict", "DeviceTerminalErrorDict", "UnknownEventDict",
        })
        self.assertEqual(set(self.fields("DefaultClearedEventDict")), {"type", "source_kind"})
        self.assertEqual(self.fields("DefaultClearedEventDict")["source_kind"], "Literal['mic', 'system']")
        self.assertEqual(set(self.fields("RescanRequiredEventDict")), {"type", "dropped_events"})
        self.assertEqual(self.fields("DeviceTerminalErrorDict"), {
            "type": "Literal['terminalError']", "error": "AudioErrorDict",
        })

    def test_error_union_has_one_arm_per_root_and_specific_payloads(self):
        expected = {
            "InvalidArgument", "InvalidState", "DeviceNotFound", "RecordingPermission",
            "UnsupportedOsVersion", "DeviceLost", "Backend", "UnsupportedFormat",
            "NativeFormatChanged", "Unsupported", "AmbiguousDeviceName",
        }
        self.assertEqual(self.members("AudioErrorDict"), {name + "ErrorDict" for name in expected})
        for name in expected:
            fields = self.fields(name + "ErrorDict")
            common = {"kind", "message", "contexts", "secondary", "diagnostic_details", "addon_kind", "addon_message"}
            specific = {
                "RecordingPermission": {"permission"}, "NativeFormatChanged": {"advertised", "actual"},
                "Backend": {"vad_code"},
            }.get(name, set())
            self.assertEqual(set(fields), common | specific)
            self.assertEqual(fields["diagnostic_details"], "List[str]")
            self.assertEqual(fields["addon_kind"], "Optional[AddonErrorKind]")
            self.assertEqual(fields["addon_message"], "Optional[str]")
            self.assertEqual("permission" in fields, name == "RecordingPermission")
            self.assertEqual("advertised" in fields, name == "NativeFormatChanged")
            self.assertEqual("actual" in fields, name == "NativeFormatChanged")
        self.assertEqual(set(self.fields("ErrorContextDict")), {"operation", "lane", "native_status"})
        self.assertEqual(set(self.fields("AudioLossDict")), {"path", "reason", "samples", "sample_rate", "channels"})
        self.assertEqual(set(self.fields("ShutdownReportDict")), {"primary", "cleanup_errors"})

    def test_backend_vad_code_is_optional_and_exclusive(self):
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
            self.assertEqual("vad_code" in fields, name == "BackendErrorDict")
        backend = namespace["BackendErrorDict"]
        self.assertEqual(self.fields("BackendErrorDict")["vad_code"], "VadErrorCode")
        self.assertEqual(backend.__optional_keys__, {"vad_code"})
        self.assertEqual(backend.__required_keys__, {"kind", "message", "contexts", "secondary", "diagnostic_details", "addon_kind", "addon_message"})

    def test_vad_error_codes_match_rust_typescript_and_c(self):
        root = Path(__file__).resolve().parents[3]
        rust = (root / "crates/flexaudio-core/src/vad_error.rs").read_text()
        variants = rust.split("pub enum VadErrorCode {", 1)[1].split("}", 1)[0]
        expected = set(re.findall(r"^    (\w+),$", variants, re.MULTILINE))
        spellings = dict(re.findall(r'Self::(\w+) => "(\w+)"', rust))
        self.assertEqual(spellings, {name: name for name in expected})
        python_codes = self.aliases["VadErrorCode"].slice.elts
        self.assertEqual({node.value for node in python_codes}, expected)
        typescript = (root / "crates/flexaudio-napi/index.d.ts").read_text()
        declaration = typescript.split("export type VadErrorCode =", 1)[1].split("/**", 1)[0]
        self.assertEqual(set(re.findall(r"'(\w+)'", declaration)), expected)
        header = (root / "crates/flexaudio-ffi/include/flexaudio.h").read_text()
        codes = header.split("// Codes: ", 1)[1].split("// # Safety", 1)[0]
        self.assertEqual(set(re.findall(r"\b[A-Z]\w+\b", codes)), expected)

    def test_switch_documents_persistent_vad_and_per_switch_denoise(self):
        method = next(node for node in self.classes["Stream"].body
                      if isinstance(node, ast.FunctionDef) and node.name == "switch_source")
        self.assertNotIn("vad", {argument.arg for argument in method.args.kwonlyargs})
        doc = ast.get_docstring(method)
        self.assertIn("persists across switches", doc)
        self.assertIn("Omitting denoise or passing None retains", doc)
        self.assertIn("explicit True/False enables/disables noise suppression", doc)


if __name__ == "__main__":
    unittest.main()
