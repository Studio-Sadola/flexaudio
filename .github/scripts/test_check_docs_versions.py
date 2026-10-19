"""Offline fixture coverage for documentation release checks."""

from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import unittest

from check_docs_versions import DocumentationCheckError, check_document, check_documentation


class DocumentationVersionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True, capture_output=True)

    def write(self, path: str, content: str, *, tracked: bool = True) -> None:
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(content, encoding="utf-8")
        if tracked:
            subprocess.run(["git", "-C", str(self.root), "add", "--", path], check=True, capture_output=True)

    def test_release_080_defects_report_every_original_line(self) -> None:
        defects = {
            "README.md": [
                'Version 0.6.0 replaces the two earlier VAD APIs.',
                'flexaudio = "0.6"',
                'cargo add flexaudio@0.4',
                'Pin to `0.6` to opt into updates.',
                'flexaudio-vad = { version = "0.6", features = ["default"] }',
            ],
            "README.ja.md": ['cargo add flexaudio-vad@0.6'],
            "crates/flexaudio-napi/README.md": [
                'npm install @studio-sadola/flexaudio@0.6',
                '[settings](../../README.md#settings)',
            ],
            "bindings/flexaudio-py/README.md": ['pip install "flexaudio>=0.6,<0.7"'],
        }
        for path, lines in defects.items():
            self.write(path, "# Install\n" + "\n".join(lines) + "\n")
        with self.assertRaises(DocumentationCheckError) as failure:
            check_documentation(self.root, "0.8.0")
        for path, lines in defects.items():
            for number, text in enumerate(lines, 2):
                with self.subTest(path=path, line=number):
                    self.assertIn(f"{path}:{number}:", str(failure.exception))
                    self.assertIn(text, str(failure.exception))

    def test_corrected_fixture_passes(self) -> None:
        self.write("README.md", '\n'.join([
            'Version 0.8.1 replaces the earlier VAD APIs.',
            'flexaudio = "0.8"',
            'flexaudio-vad = { version = "0.8.1", features = [] }',
            'cargo add flexaudio@0.8.1',
            'cargo add flexaudio-denoise@0.8',
            'Pin to `0.8` to opt into updates.',
        ]))
        self.write("crates/flexaudio-napi/README.md", '\n'.join([
            'npm install @studio-sadola/flexaudio@0.8.1',
            'npm i @studio-sadola/flexaudio-linux-x64-gnu@0.8',
            '[guide](https://github.com/Studio-Sadola/flexaudio/blob/v0.8.1/README.md)',
            '[local](README.md#install)',
            '[subdirectory](examples/demo.md)',
            '[anchor](#install)',
        ]))
        self.write("bindings/flexaudio-py/README.md", '\n'.join([
            'pip install "flexaudio>=0.8,<0.9"',
            'python -m pip install flexaudio==0.8.1',
            'pip3 install "flexaudio~=0.8.1"',
        ]))
        check_documentation(self.root, "0.8.1")

    def test_pins_across_every_documentation_location(self) -> None:
        paths = [
            "README.md", "README.ja.md", "docs/guide.md", "docs/nested/guide.md",
            "crates/flexaudio/README.md", "bindings/flexaudio-py/README.md",
            "crates/flexaudio-napi/index.d.ts", "bindings/flexaudio-py/flexaudio.pyi",
            "crates/flexaudio-ffi/include/flexaudio.h",
        ]
        for path in paths:
            self.write(path, '\n// cargo add flexaudio@0.6\n')
        with self.assertRaises(DocumentationCheckError) as failure:
            check_documentation(self.root, "0.8.0")
        for path in paths:
            self.assertIn(f"{path}:2:", str(failure.exception))

    def test_historical_exemptions_are_explicit(self) -> None:
        self.write("CHANGELOG.md", 'flexaudio = "0.1"\n')
        self.write("docs/CHANGELOG.md", 'cargo add flexaudio@0.1\n')
        self.write("docs/history.md", '\n'.join([
            'History: cargo add flexaudio@0.1',
            'Since 0.1: flexaudio = "0.1"',
            'Previously: npm install @studio-sadola/flexaudio@0.1',
            'Migration from: pip install flexaudio==0.1',
            '## History',
            'flexaudio = "0.1"',
            '### Earlier releases',
            'cargo add flexaudio@0.1',
            '## Migration',
            'flexaudio-vad = { version = "0.1" }',
        ]))
        check_documentation(self.root, "0.8.1")

    def test_history_scope_ends_and_similar_words_do_not_exempt(self) -> None:
        content = '\n'.join([
            '# History', 'flexaudio = "0.1"', '# Install',
            'flexaudio = "0.1"', 'flexaudio = "0.1" # historyless',
            '```md', '# History', 'cargo add flexaudio@0.1', '```',
            '## History of usage', 'cargo add flexaudio@0.1',
        ])
        findings = check_document("README.md", content, "0.8.1")
        self.assertEqual([finding.line for finding in findings], [4, 5, 8, 11])

    def test_registry_relative_links_in_all_supported_forms(self) -> None:
        bad_links = [
            '[root](../../README.md)', '[root](../../README.md#install)',
            '[root](<../../README.md>)', '[root]: ../../README.md "Title"',
            '<a href="../../README.md">root</a>', '![root](../../logo.png)',
            '<img src="../../logo.png">', '[root](/README.md)',
            '[root](%2e%2e/%2e%2e/README.md)',
        ]
        for link in bad_links:
            for path in ("crates/flexaudio/README.md", "bindings/flexaudio-py/README.md"):
                with self.subTest(link=link, path=path):
                    findings = check_document(path, '# History\n' + link, "0.8.1")
                    self.assertEqual(len(findings), 1)
                    self.assertEqual(findings[0].line, 2)
                    self.assertEqual(findings[0].text, link)
        self.assertEqual(check_document("README.md", '[guide](docs/guide.md)', "0.8.1"), [])
        self.assertEqual(check_document("crates/flexaudio/README.md", '[local](sub/../README.md)', "0.8.1"), [])

    def test_registry_repository_links_match_exact_tag(self) -> None:
        for kind in ("blob", "tree"):
            for ref in ("v0.8.1", "main", "025e595", "v0.8.0"):
                content = f'[guide](https://github.com/Studio-Sadola/flexaudio/{kind}/{ref}/docs/guide.md)'
                for version in ("0.8.1", "0.8.2"):
                    with self.subTest(kind=kind, ref=ref, version=version):
                        findings = check_document("crates/flexaudio/README.md", '\n' + content, version)
                        self.assertEqual(bool(findings), ref != f"v{version}")
                        if findings:
                            self.assertEqual(findings[0].line, 2)
                            self.assertEqual(findings[0].text, content)
        historical = 'Previously: [guide](https://github.com/Studio-Sadola/flexaudio/blob/v0.1.0/README.md)'
        self.assertEqual(check_document("crates/flexaudio/README.md", historical, "0.8.1"), [])

    def test_multiline_cargo_dependency_reports_version_line(self) -> None:
        content = 'flexaudio-vad = {\n  version = "0.6",\n  features = []\n}\n'
        findings = check_document("docs/guide.md", content, "0.8.1")
        self.assertEqual([finding.line for finding in findings], [2])

    def test_every_pin_in_a_single_install_command_is_checked(self) -> None:
        for command in (
            'cargo add flexaudio@0.8 flexaudio-vad@0.6',
            'npm i @studio-sadola/flexaudio@0.8 @studio-sadola/flexaudio-linux-x64-gnu@0.6',
            'pip install "flexaudio>=0.8,<0.9" flexaudio==0.6',
        ):
            with self.subTest(command=command):
                findings = check_document("README.md", command, "0.8.1")
                self.assertEqual(len(findings), 1)
                self.assertEqual(findings[0].line, 1)

    def test_other_packages_and_runtime_versions_are_not_release_references(self) -> None:
        content = '\n'.join([
            'other-flexaudio = "0.1"', 'cargo add other-flexaudio@0.1',
            'pip install other-flexaudio==0.1', 'Python version 3.8.0 is supported.',
            'npm install @other/flexaudio@0.1',
        ])
        self.assertEqual(check_document("README.md", content, "0.8.1"), [])

    def test_version_constraints_cannot_mask_stale_or_unbounded_pins(self) -> None:
        for pin in ('==0.8.0', '>=0.6,<0.9', '>=0.8,<0.10', '==0.8.*', '!=0.8.1', '>=0.8,>=0.6'):
            with self.subTest(pin=pin):
                self.assertTrue(check_document("README.md", f'pip install "flexaudio{pin}"', "0.8.1"))

    def test_only_tracked_documentation_is_scanned(self) -> None:
        self.write("README.md", 'flexaudio = "0.8"')
        self.write("docs/untracked.md", 'flexaudio = "0.6"', tracked=False)
        self.write("example.py", 'flexaudio = "0.6"')
        check_documentation(self.root, "0.8.1")

    def test_missing_tracked_file_and_non_repository_fail_closed(self) -> None:
        self.write("README.md", "# Install")
        (self.root / "README.md").unlink()
        with self.assertRaisesRegex(DocumentationCheckError, 'README.md: cannot inspect documentation'):
            check_documentation(self.root, "0.8.1")
        with tempfile.TemporaryDirectory() as plain:
            with self.assertRaisesRegex(DocumentationCheckError, 'Cannot enumerate tracked documentation'):
                check_documentation(Path(plain), "0.8.1")


if __name__ == "__main__":
    unittest.main()
