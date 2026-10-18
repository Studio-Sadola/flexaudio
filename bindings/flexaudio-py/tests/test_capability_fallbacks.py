"""Future capability tags must remain explicit at the Python boundary."""
import ast
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[3]


class CapabilityFallbacks(unittest.TestCase):
    def test_unknown_rust_variants_are_never_known_tags_or_dropped(self):
        # Future non-exhaustive variants cannot be constructed safely today.
        # Guard their actual match arms until the upstream enums gain variants.
        source = (ROOT / "bindings/flexaudio-py/src/capabilities.rs").read_text()
        for enum in ["PidScope", "DeviceSelectionBehavior", "WatcherMode", "WatchScope"]:
            with self.subTest(enum=enum):
                match = re.search(rf"fa::{enum}::\w+\s*=>.*?\n\s*_\s*=>\s*([^,\n]+)", source, re.S)
                self.assertIsNotNone(match)
                self.assertEqual(match.group(1), '"other"')
        scopes = source.split('"scopes",', 1)[1].split('.collect::<Vec<_>>()', 1)[0]
        self.assertNotIn("filter_map", scopes)
        self.assertIn(".map(", scopes)
        self.assertIn("other => return other_unavailable(py, other)", source)
        helper = (ROOT / "bindings/flexaudio-py/src/lib.rs").read_text().split("fn source_kind_str", 1)[1].split("\n}", 1)[0]
        self.assertIn('_ => "other"', helper)

    def test_capability_literals_match_node_tags_including_other(self):
        tree = ast.parse((ROOT / "bindings/flexaudio-py/flexaudio.pyi").read_text())
        declarations = (ROOT / "crates/flexaudio-napi/index.d.ts").read_text()
        aliases = {node.targets[0].id: node.value for node in tree.body if isinstance(node, ast.Assign)}
        classes = {node.name: node for node in tree.body if isinstance(node, ast.ClassDef)}

        def field(class_name, field_name):
            return next(node.annotation for node in classes[class_name].body
                        if isinstance(node, ast.AnnAssign) and node.target.id == field_name)

        def literals(node):
            return {item.value for item in ast.walk(node) if isinstance(item, ast.Constant) and isinstance(item.value, str)}

        for node_name, annotation in [
            ("BackendKind", aliases["CapabilityBackend"]),
            ("PidScope", aliases["PidScope"]),
            ("DeviceSelectionBehavior", field("SystemExclusionCapabilitiesDict", "device_selection")),
            ("WatcherMode", field("DeviceWatcherCapabilitiesDict", "mode")),
            ("WatchScope", field("DeviceWatcherCapabilitiesDict", "scopes")),
            ("SideEffectCondition", field("SideEffectCapabilityDict", "condition")),
        ]:
            with self.subTest(type=node_name):
                expected = set(re.findall(r"'([^']+)'", re.search(rf"export type {node_name} = ([^\n]+)", declarations).group(1)))
                self.assertIn("other", expected)
                self.assertEqual(literals(annotation), expected)
        sources = re.search(r"sources: Array<([^>]+)>", declarations).group(1)
        self.assertEqual(literals(field("SideEffectCapabilityDict", "sources")), set(re.findall(r"'([^']+)'", sources)))

    def test_poll_chunk_documentation_describes_polling(self):
        tree = ast.parse((ROOT / "bindings/flexaudio-py/flexaudio.pyi").read_text())
        stream = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == "Stream")
        poll = next(node for node in stream.body if isinstance(node, ast.FunctionDef) and node.name == "poll_chunk")
        doc = ast.get_docstring(poll)
        self.assertIn("primary audio chunk", doc)
        self.assertIn("None", doc)
        self.assertNotIn("Switch sources", doc)


if __name__ == "__main__":
    unittest.main()
