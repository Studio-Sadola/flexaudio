"""Offline declaration compatibility tests; no registry, compiler, or audio required."""

from __future__ import annotations

from pathlib import Path
import runpy
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-api-compatibility.py")
FILES = (
    "crates/flexaudio-napi/index.d.ts",
    "bindings/flexaudio-py/flexaudio.pyi",
    "crates/flexaudio-ffi/include/flexaudio.h",
)
DECLARATIONS = (
    '''export declare function devices(): Array<Device>
export interface Device {
  id: string
  kind: 'mic'
    | 'system'
}
export declare class Stream {
  constructor(rate?: number)
  stop(): Promise<void>
  convert(
    samples: Float32Array,
    rate: number
  ): Float32Array
}
export type Kind = 'mic' | 'system'
''',
    '''from typing import Optional
Kind = str
def devices(kind: Optional[str] = None) -> list[str]: ...
class Stream:
    """A stream."""
    def __init__(self, rate: int = 48000) -> None: ...
    @property
    def rate(self) -> int: ...
    def stop(self) -> None: ...
''',
    '''#ifndef API_H
#define API_H
#include <stdint.h>
#define API_OK 0
typedef struct Stream Stream;
typedef struct Config { uint32_t rate; uint16_t channels; } Config;
typedef enum Kind { MIC = 0, SYSTEM, MIX = 7 } Kind;
int32_t devices(const char *kind);
void stop(struct Stream *stream);
#endif
''',
)


class ApiCompatibilityTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.write("Cargo.toml", '[workspace.package]\nversion = "0.6.2"\n')
        for path, text in zip(FILES, DECLARATIONS):
            self.write(path, text)
        self.git("init", "--quiet")
        self.commit_baseline()

    def commit_baseline(self) -> None:
        self.git("add", ".")
        # Fixture identity is independent of the contributor's global Git config.
        self.git("-c", "user.name=API tests", "-c", "user.email=tests@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "--quiet", "-m", "Test baseline")
        self.git("tag", "-f", "v0.6.2")

    def git(self, *args: str) -> str:
        return subprocess.run(["git", "-C", str(self.root), *args], check=True,
                              capture_output=True, text=True).stdout

    def write(self, relative: str, text: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def run_gate(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([sys.executable, str(SCRIPT), "--repo-root", str(self.root), *args],
                              capture_output=True, text=True, check=False)

    def assert_pass(self) -> None:
        result = self.run_gate()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("API compatibility check passed: v0.6.2", result.stdout)

    def assert_failure(self, expected: str) -> None:
        result = self.run_gate()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(expected, result.stderr)

    def test_unchanged_and_patch_versions_pass(self) -> None:
        self.assert_pass()
        self.write("Cargo.toml", '[workspace.package]\nversion = "0.6.3"\n')
        self.assert_pass()

    def test_additions_pass(self) -> None:
        self.write(FILES[0], DECLARATIONS[0].replace("  id: string", "  label?: string\n  id: string")
                   + "export declare function newExport(): void\n")
        self.write(FILES[1], DECLARATIONS[1].replace("    def stop", "    def added(self) -> str: ...\n    def stop")
                   + "def added() -> str: ...\n")
        self.write(FILES[2], DECLARATIONS[2].replace("MIX = 7", "MIX = 7, NEW = 8")
                   + "void added(void);\n")
        self.assert_pass()

    def test_comments_and_formatting_are_ignored(self) -> None:
        self.write(FILES[0], DECLARATIONS[0].replace("devices()", "devices ( )").replace(
            "  id: string", "  /** Identifier. */ id : string;"))
        self.write(FILES[1], DECLARATIONS[1].replace('"""A stream."""', '"""Updated documentation."""'))
        self.write(FILES[2], DECLARATIONS[2].replace("int32_t devices", "/* Enumerate. */\nint32_t    devices"))
        self.assert_pass()

    def test_typescript_removed_exports_and_members(self) -> None:
        for removed, name in (("export declare function devices(): Array<Device>\n", "devices"),
                              ("  stop(): Promise<void>\n", "Stream.stop"),
                              ("  id: string\n", "Device.id")):
            with self.subTest(name=name):
                self.write(FILES[0], DECLARATIONS[0].replace(removed, ""))
                self.assert_failure("removed or changed " + name)

    def test_typescript_changed_signatures(self) -> None:
        for old, new, name in (("rate?: number", "rate: number", "Stream.constructor"),
                               ("Promise<void>", "Promise<boolean>", "Stream.stop"),
                               ("id: string", "id: number", "Device.id"),
                               ("rate: number", "rate: string", "Stream.convert"),
                               ("'mic' | 'system'", "'mic'", "Kind"),
                               ("interface Device {", "interface Device extends Other {", "Device")):
            with self.subTest(name=name):
                self.write(FILES[0], DECLARATIONS[0].replace(old, new))
                self.assert_failure("removed or changed " + name)

    def test_typescript_overload_removal(self) -> None:
        original = DECLARATIONS[0] + "export declare function devices(kind: string): Array<Device>\n"
        self.write(FILES[0], original)
        self.commit_baseline()
        self.write(FILES[0], DECLARATIONS[0])
        self.assert_failure("removed or changed devices")

    def test_python_removed_exports_and_members(self) -> None:
        for removed, name in (("def devices(kind: Optional[str] = None) -> list[str]: ...\n", "devices"),
                              ("    def stop(self) -> None: ...\n", "Stream.stop")):
            with self.subTest(name=name):
                self.write(FILES[1], DECLARATIONS[1].replace(removed, ""))
                self.assert_failure("removed or changed " + name)

    def test_python_changed_signatures(self) -> None:
        for old, new, name in (("rate: int = 48000", "rate: int = 16000", "Stream.__init__"),
                               ("@property", "@classmethod", "Stream.rate"),
                               ("list[str]", "list[int]", "devices"),
                               ("class Stream:", "class Stream(Base):", "Stream"),
                               ("Kind = str", "Kind = int", "Kind")):
            with self.subTest(name=name):
                self.write(FILES[1], DECLARATIONS[1].replace(old, new))
                self.assert_failure("removed or changed " + name)

    def test_c_removals_and_prototype_changes(self) -> None:
        for old, new, name in (("void stop(struct Stream *stream);", "", "stop"),
                               ("int32_t devices", "int64_t devices", "devices"),
                               ("const char *kind", "char *kind", "devices"),
                               ("typedef struct Stream Stream;", "", "Stream"),
                               ("uint32_t rate", "uint64_t rate", "Config"),
                               ("uint16_t channels;", "", "Config"),
                               (" MIC = 0,", "", "Kind.MIC"),
                               ("MIX = 7", "MIX = 9", "Kind.MIX"),
                               ("MIC = 0", "MIC = 1", "Kind.SYSTEM"),
                               ("#define API_OK 0", "#define API_OK 1", "API_OK")):
            with self.subTest(name=name):
                self.write(FILES[2], DECLARATIONS[2].replace(old, new))
                self.assert_failure("removed or changed " + name)

    def test_minor_and_major_allow_breaking_declarations(self) -> None:
        self.write(FILES[0], DECLARATIONS[0].replace("  stop(): Promise<void>\n", ""))
        self.write(FILES[1], DECLARATIONS[1].replace("    def stop(self) -> None: ...\n", ""))
        self.write(FILES[2], DECLARATIONS[2].replace("void stop(struct Stream *stream);", ""))
        for value in ("0.7.0", "1.0.0"):
            with self.subTest(version=value):
                self.write("Cargo.toml", f'[workspace.package]\nversion = "{value}"\n')
                self.assert_pass()

    def test_rollback_and_invalid_versions_fail(self) -> None:
        for value in ("0.6.1", "0.5.9", "0.6.3-rc.1", "0.6", "00.6.3"):
            with self.subTest(version=value):
                self.write("Cargo.toml", f'[workspace.package]\nversion = "{value}"\n')
                result = self.run_gate()
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_latest_tag_is_numeric_and_ignores_prereleases(self) -> None:
        self.git("tag", "v0.6.10-rc.1")
        self.git("tag", "unrelated")
        self.assert_pass()
        self.git("tag", "v0.10.0")
        self.git("tag", "v0.9.0")
        gate = runpy.run_path(str(SCRIPT))
        self.assertEqual(gate["latest_tag"](self.root), "v0.10.0")
        self.assert_failure("Release tag and baseline workspace version differ")

    def test_missing_tags_fail(self) -> None:
        self.git("tag", "-d", "v0.6.2")
        self.assert_failure("No release tags found")

    def test_missing_baseline_file_fails(self) -> None:
        self.git("rm", FILES[2])
        self.commit_baseline()
        self.write(FILES[2], DECLARATIONS[2])
        self.assert_failure("Cannot read release baseline")

    def test_missing_current_file_fails(self) -> None:
        (self.root / FILES[1]).unlink()
        self.assert_failure("API compatibility check failed")

    def test_unknown_syntax_fails_closed(self) -> None:
        for path, text in zip(FILES, ("export namespace Unknown {}", "if True:\n    x: int", "extern int unknown;")):
            with self.subTest(path=path):
                original = (self.root / path).read_text()
                self.write(path, text)
                self.assert_failure("Unsupported")
                self.write(path, original)

    def test_invalid_explicit_tag_fails(self) -> None:
        for tag in ("HEAD", "--help", "v0.6.2:Cargo.toml", "v0.6.2\n"):
            with self.subTest(tag=tag):
                result = self.run_gate("--baseline-tag", tag)
                self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_actual_checked_in_declarations_parse(self) -> None:
        root = SCRIPT.resolve().parents[2]
        for path in FILES:
            self.write(path, (root / path).read_text(encoding="utf-8"))
        self.commit_baseline()
        self.assert_pass()


if __name__ == "__main__":
    unittest.main()
