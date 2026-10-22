"""Hardware-free regressions for public startup and consent promises."""
from pathlib import Path
import ast
import re
import unittest


ROOT = Path(__file__).resolve().parents[3]


class StartupDocumentation(unittest.TestCase):
    def test_windows_enumeration_keeps_native_discovery_without_consent_preflight(self):
        paths = [
            "README.md", "CHANGELOG.md", "crates/flexaudio/src/lib.rs",
            "crates/flexaudio-napi/index.d.ts", "crates/flexaudio-napi/src/lib.rs",
            "bindings/flexaudio-py/flexaudio.pyi", "crates/flexaudio-ffi/src/lib.rs",
            "crates/flexaudio-ffi/include/flexaudio.h",
        ]
        for path in paths:
            with self.subTest(path=path):
                text = (ROOT / path).read_text(encoding="utf-8")
                if path == "CHANGELOG.md":
                    text = text.split("## [0.5.2]", 1)[0]
                self.assertFalse(
                    "Windows confirmed microphone denial also fails discovery" in text
                    or "On Windows, confirmed microphone denial also fails" in text,
                    "Windows enumeration must not consult capture consent",
                )
                self.assertTrue(
                    "Windows enumeration does not preflight microphone permission" in text,
                    "Native Windows enumeration policy must be explicit",
                )
                self.assertTrue("macOS" in text and "backend" in text.lower() and "HAL" in text,
                                "The macOS enumeration gate must remain documented")
        japanese = (ROOT / "README.ja.md").read_text(encoding="utf-8")
        inventory = japanese.split("- `flexaudio::devices()", 1)[1].split("- `flexaudio::processes()", 1)[0]
        self.assertTrue("AppCapability" in inventory,
                        "Japanese Windows inventory docs must explain the absent consent check")
        self.assertTrue("\u4e8b\u524d\u78ba\u8a8d\u3092\u884c\u308f\u305a" in inventory,
                        "Japanese Windows inventory must preserve native discovery")

    def test_changelog_keeps_precise_consent_correction(self):
        text = (ROOT / "CHANGELOG.md").read_bytes()
        current = text[:text.index(b"## [0.5.2]")].decode()
        for fact in ["ConsentPoll", "NotDetermined", "Ok(true)", "query_native_format", "Authorized"]:
            self.assertTrue(fact in current, f"Missing precise consent history: {fact}")

    def test_macos_enumeration_never_prompts_or_queries_undecided_hal(self):
        declarations = [
            ("crates/flexaudio-napi/index.d.ts", "export declare function devices()", "/**"),
            ("crates/flexaudio-napi/src/lib.rs", "pub fn devices(env:", "/// Enumerate the complete inventory"),
            ("crates/flexaudio/src/lib.rs", "pub fn devices()", "/// Return audio devices"),
            ("crates/flexaudio-ffi/src/lib.rs", "pub unsafe extern \"C\" fn flexaudio_devices(", "/// List available devices"),
        ]
        for path, declaration, marker in declarations:
            with self.subTest(path=path):
                text = (ROOT / path).read_text(encoding="utf-8")
                self.assertTrue(declaration in text, "Enumeration declaration is missing")
                prefix = text.split(declaration, 1)[0]
                self.assertTrue(marker in prefix, "Enumeration documentation anchor is missing")
                doc = prefix[prefix.rfind(marker):]
                for fact in ["never prompts", "Backend", "HAL", "undecided", "mandatory"]:
                    self.assertTrue(fact in doc, f"Enumeration contract omits {fact}")
        tree = ast.parse((ROOT / "bindings/flexaudio-py/flexaudio.pyi").read_text(encoding="utf-8"))
        devices = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == "devices")
        doc = ast.get_docstring(devices)
        for fact in ["never prompts", "Backend", "HAL", "undecided", "mandatory"]:
            self.assertTrue(fact in doc, f"Python enumeration contract omits {fact}")

    def test_public_open_poll_and_japanese_linux_contracts_are_explicit(self):
        facade = (ROOT / "crates/flexaudio/src/lib.rs").read_text(encoding="utf-8")
        self.assertTrue("/// High-level entry point" in facade.split("pub fn open(", 1)[0],
                        "Facade open documentation anchor is missing")
        doc = facade.split("pub fn open(", 1)[0].rsplit("/// High-level entry point", 1)[-1]
        with self.subTest(surface="facade open"):
            self.assertTrue("30 seconds" in doc and "Backend" in doc and "HAL" in doc,
                            "Facade open must describe the bounded pre-HAL consent failure")
        ffi = (ROOT / "crates/flexaudio-ffi/src/lib.rs").read_text(encoding="utf-8")
        open_doc = ffi.split("pub unsafe extern \"C\" fn flexaudio_open(", 1)[0].rsplit("/// Open a stream", 1)[-1]
        with self.subTest(surface="C v1 open"):
            self.assertTrue("30 seconds" in open_doc and "Backend" in open_doc and "HAL" in open_doc,
                            "C v1 open must describe the bounded pre-HAL consent failure")
        poll_doc = ffi.split("pub unsafe extern \"C\" fn flexaudio_poll_chunk(", 1)[0].rsplit("/// Retrieve one chunk", 1)[-1]
        with self.subTest(surface="C v1 PCM poll"):
            self.assertTrue("PCM polling does not wait for event polling" in poll_doc,
                            "C v1 PCM poll must describe independence from event polling")
            self.assertTrue("FIFO" in poll_doc and "published" in poll_doc,
                            "C v1 poll must describe FIFO notice publication ordering")
        japanese = (ROOT / "README.ja.md").read_text(encoding="utf-8")
        linux = japanese.split("- **Linux:**", 1)[1].split("\n\n", 1)[0]
        with self.subTest(surface="Japanese Linux exclusions"):
            self.assertTrue("2 \u79d2" in linux,
                            "Japanese Linux exclusion rules must describe the two-second negotiation bound")
        header = (ROOT / "crates/flexaudio-ffi/include/flexaudio.h").read_text(encoding="utf-8")
        self.assertTrue("struct FlexStream *flexaudio_open(" in header, "C open declaration is missing")
        header_open = header.split("struct FlexStream *flexaudio_open(", 1)[0].rsplit("// Open a stream", 1)[-1]
        with self.subTest(surface="generated C open brief"):
            self.assertTrue("30 seconds" in header_open and "Backend" in header_open and "HAL" in header_open,
                            "Generated C open brief must describe bounded consent")
        header_poll = header.split("int32_t flexaudio_poll_chunk(", 1)[0].rsplit("// Retrieve one chunk", 1)[-1]
        self.assertTrue("int32_t flexaudio_poll_chunk(" in header, "C poll declaration is missing")
        with self.subTest(surface="generated C poll brief"):
            self.assertTrue("PCM polling does not wait for event polling" in header_poll
                            and "FIFO" in header_poll and "published" in header_poll,
                            "Generated C poll brief must preserve the pull/publication contract")

    def test_japanese_readme_covers_startup_and_decided_consent(self):
        text = (ROOT / "README.ja.md").read_text(encoding="utf-8")
        startup = next(
            (section for section in text.split("\n## ")
             if "StartupSamplesDropped {" in section),
            None,
        )
        self.assertIsNotNone(startup, "Japanese startup documentation is missing")
        for term in ["10 ms", "2 \u79d2", "generationEnded", "DISCONTINUITY"]:
            self.assertTrue(term in startup, f"Missing Japanese startup contract: {term!a}")
        self.assertFalse("500 ms" in text, "Obsolete periodic consent monitor claim")
        self.assertFalse("Event::PermissionPending {" in text, "No built-in pending-consent event")
        self.assertTrue("30 \u79d2" in text, "Japanese consent bound is missing")
        self.assertTrue("`Backend`" in text, "Japanese typed consent failure is missing")

    def test_napi_open_rustdoc_requires_decided_consent(self):
        text = (ROOT / "crates/flexaudio-napi/src/lib.rs").read_text(encoding="utf-8")
        doc = text.split("/// Open and start a stream", 1)[1].split("#[napi", 1)[0]
        self.assertFalse("unanswered prompt proceeds with capture" in doc, "NAPI Rustdoc promises unsafe unanswered capture")
        self.assertFalse("Authorization is checked every 500 ms" in doc, "NAPI Rustdoc promises a nonexistent consent monitor")
        self.assertTrue("before any HAL" in doc, "NAPI Rustdoc omits pre-HAL consent")
        self.assertTrue("30 s" in doc, "NAPI Rustdoc omits consent bound")
        self.assertTrue("Backend" in doc, "NAPI Rustdoc omits typed consent failure")

    def test_pull_documentation_never_requires_event_polling_for_pcm(self):
        paths = [
            "README.md", "bindings/flexaudio-py/README.md",
            "bindings/flexaudio-py/flexaudio.pyi", "crates/flexaudio-ffi/README.md",
            "crates/flexaudio-ffi/include/flexaudio.h",
        ]
        stale = (r"return no chunk while a startup notice|hold[s]? back (?:their|its) generation"
                 r"|returns None until that startup notice|polling a startup notice releases"
                 r"|must drain[^.]*before retrying pending PCM")
        for path in paths:
            with self.subTest(path=path):
                text = (ROOT / path).read_text(encoding="utf-8")
                self.assertIsNone(re.search(stale, text, re.DOTALL), "PCM must not wait for event polling")
                self.assertIsNotNone(
                    re.search(r"PCM\s+(?:polling|polls)\s+(?:does|do)\s+not\s+wait for event polling", text),
                    "Independent pull contract is missing",
                )

    def test_napi_callback_order_excludes_other_advisories(self):
        text = (ROOT / "crates/flexaudio-napi/index.d.ts").read_text(encoding="utf-8")
        self.assertIsNotNone(re.search(r"[Rr]ecovered and other advisories[^.]*independent", text), "Callback ordering excludes independent advisories")
        self.assertTrue("before or after the startup notice" in text, "Independent callback order must be explicit")


if __name__ == "__main__":
    unittest.main()
