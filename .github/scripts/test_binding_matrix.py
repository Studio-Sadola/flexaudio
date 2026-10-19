"""Offline matrix validation, declaration indexing and documentation regression tests."""

from __future__ import annotations

import copy
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import binding_matrix as matrix
from binding_matrix_surfaces import rust_declarations, rust_surface, rust_text, surfaces


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / ".github/scripts/binding_matrix.py"


class BindingMatrixTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.indexes = surfaces(ROOT)

    def sample(self) -> dict:
        return {"features": [{"feature": "Device listing", "bindings": {
            "Rust": {"status": "supported", "symbols": ["flexaudio::devices"]},
            "Node": {"status": "supported", "symbols": ["devices"]},
            "Python": {"status": "supported", "symbols": ["devices"]},
            "C": {"status": "supported", "symbols": ["flexaudio_devices"]},
            "CLI": {"status": "supported", "symbols": ["--list-devices"]},
        }}]}

    def test_nonexistent_symbol_fails_in_each_frontend(self) -> None:
        for frontend in matrix.FRONTENDS:
            with self.subTest(frontend=frontend):
                data = self.sample()
                data["features"][0]["bindings"][frontend]["symbols"].append("nonexistent_symbol")
                with self.assertRaisesRegex(matrix.MatrixError, "unknown symbol nonexistent_symbol"):
                    matrix.validate(data, self.indexes)

    def test_unavailable_cells_require_nonempty_single_line_reasons(self) -> None:
        for status in ("missing", "not-applicable"):
            for reason in (None, "", "  ", 1, "first\nsecond", "first\rsecond"):
                with self.subTest(status=status, reason=reason):
                    data = self.sample()
                    cell = {"status": status}
                    if reason is not None:
                        cell["reason"] = reason
                    data["features"][0]["bindings"]["Node"] = cell
                    with self.assertRaises(matrix.MatrixError):
                        matrix.validate(data, self.indexes)

    def test_explicit_unavailable_cells_pass(self) -> None:
        for status in ("missing", "not-applicable"):
            data = self.sample()
            data["features"][0]["bindings"]["Node"] = {"status": status, "reason": "No option exists."}
            self.assertIn(status + ": No option exists.", matrix.render(matrix.validate(data, self.indexes)))

    def test_invalid_structure_fails_closed(self) -> None:
        cases = [[], {}, {"features": []}, {"features": ["invalid"]}]
        data = self.sample()
        del data["features"][0]["bindings"]["CLI"]
        cases.append(data)
        data = self.sample()
        data["features"].append(copy.deepcopy(data["features"][0]))
        cases.append(data)
        for cell in ({"status": "unknown"}, {"status": "supported", "symbols": []},
                     {"status": "supported", "symbols": ["devices"], "typo": True},
                     {"status": "missing", "reason": "No option.", "symbols": ["devices"]}):
            data = self.sample()
            data["features"][0]["bindings"]["Node"] = cell
            cases.append(data)
        for data in cases:
            with self.subTest(data=data), self.assertRaises(matrix.MatrixError):
                matrix.validate(data, self.indexes)

    def test_qualified_options_and_fields_are_checked(self) -> None:
        expected = {
            "Rust": "flexaudio::StreamConfig::secondary_output",
            "Node": "OpenOptions.secondaryOutput",
            "Python": "open.secondary_output",
            "C": "FlexStreamConfigV3.secondary_output",
            "CLI": "--no-classic-loopback-keepalive",
        }
        for frontend, symbol in expected.items():
            with self.subTest(frontend=frontend):
                self.assertIn(symbol, self.indexes[frontend])
                data = self.sample()
                data["features"][0]["bindings"][frontend]["symbols"] = [symbol + "_typo"]
                with self.assertRaisesRegex(matrix.MatrixError, "unknown symbol"):
                    matrix.validate(data, self.indexes)

    def test_rust_index_excludes_private_and_wrong_scopes(self) -> None:
        text = '''
pub struct Options { pub capacity: usize, hidden: usize }
pub enum Source { Mic, System }
const EXAMPLE: &str = "pub fn invented() {}";
impl Options {
    pub fn open() { let text = "already cleaned"; }
    fn hidden() {}
}
mod private { pub fn nested() {} }
impl SomeTrait for Options { fn trait_method() {} }
'''
        names = rust_declarations(text)
        self.assertTrue({"Options::capacity", "Options::open", "Source::Mic"} <= names)
        self.assertTrue({"Options::hidden", "Options::trait_method", "nested", "invented"}.isdisjoint(names))

    def test_rust_lifetimes_and_literals_do_not_preserve_fake_comment_symbols(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            source = Path(temporary) / "lib.rs"
            source.write_text('''
pub fn real(value: &'a str) {}
// pub fn invented() {}
pub fn other(value: &'b str) { let brace = '}'; }
const TEXT: &str = r#"pub fn fake() { }"#;
''', encoding="utf-8")
            self.assertEqual(rust_declarations(rust_text(source)), {"real", "other"})

    def test_rust_reexport_removal_invalidates_claim(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            facade = root / "crates/flexaudio/src/lib.rs"
            facade.write_text(facade.read_text(encoding="utf-8").replace("pub use stream::Stream;", ""), encoding="utf-8")
            self.assertNotIn("flexaudio::Stream::switch_source", rust_surface(root))

    def run_gate(self, root: Path, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([sys.executable, str(SCRIPT), "--repo-root", str(root), *args],
                              capture_output=True, text=True, check=False)

    def fixture(self, root: Path) -> None:
        # Only declaration inputs, not binaries, caches or dependency directories.
        for path in (ROOT / "crates").rglob("*.rs"):
            target = root / path.relative_to(ROOT)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
        for relative in ("crates/flexaudio-napi/index.d.ts", "bindings/flexaudio-py/flexaudio.pyi",
                         "crates/flexaudio-ffi/include/flexaudio.h", matrix.DATA):
            target = root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, target)

    def test_generation_stale_and_missing_document(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.fixture(root)
            missing = self.run_gate(root, "--check")
            self.assertEqual(missing.returncode, 1)
            self.assertIn("Stale docs/binding-matrix.md", missing.stderr)
            generated = self.run_gate(root)
            self.assertEqual(generated.returncode, 0, generated.stderr)
            doc = root / matrix.DOC
            expected = doc.read_bytes()
            self.assertEqual(self.run_gate(root, "--check").returncode, 0)
            doc.write_bytes(expected + b"stale\n")
            stale = self.run_gate(root, "--check")
            self.assertEqual(stale.returncode, 1)
            self.assertIn("Stale docs/binding-matrix.md", stale.stderr)
            self.assertEqual(doc.read_bytes(), expected + b"stale\n")
            self.assertEqual(self.run_gate(root).returncode, 0)
            self.assertEqual(doc.read_bytes(), expected)
            data = json.loads((root / matrix.DATA).read_text(encoding="utf-8"))
            data["features"][0]["feature"] += " updated"
            (root / matrix.DATA).write_text(json.dumps(data), encoding="utf-8")
            self.assertEqual(self.run_gate(root, "--check").returncode, 1)

    def test_current_repository_passes(self) -> None:
        result = self.run_gate(ROOT, "--check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Binding matrix check passed:", result.stdout)


if __name__ == "__main__":
    unittest.main()
