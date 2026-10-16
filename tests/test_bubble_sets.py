# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy>=2,<3", "contourpy>=1.3,<2", "shapely>=2,<3", "cairosvg>=2.7,<3"]
# ///
"""Run geometry and CLI checks with `uv run tests/test_bubble_sets.py`."""

import importlib.util
import json
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path
from unittest.mock import patch

REPO = Path(__file__).resolve().parents[1]
SCRIPT = REPO / "scripts" / "bubble_sets.py"
HAS_DEPS = all(importlib.util.find_spec(name) for name in ("numpy", "contourpy", "shapely", "cairosvg"))
if HAS_DEPS:
    sys.path.insert(0, str(REPO / "scripts"))
    import bubble_sets as bs


@unittest.skipUnless(HAS_DEPS, "run with uv run tests/test_bubble_sets.py for optional diagram dependencies")
class BubbleSetsTests(unittest.TestCase):
    def faithful(self, data, root=None):
        data = bs.normalize(data)
        nodes, bubbles = bs.build(data, root)
        self.assertEqual(len(nodes), len(data["agents"]))
        self.assertEqual({b.name for b in bubbles}, {g for g, members in data["groups"].items() if members})
        for bubble in bubbles:
            self.assertTrue(bubble.shape.is_valid)
            self.assertEqual(bubble.shape.geom_type, "Polygon")
            for node in nodes:
                if node.name in data["groups"][bubble.name]:
                    self.assertTrue(bubble.shape.covers(node.shape()), (bubble.name, node.name))
                else:
                    self.assertFalse(bubble.shape.intersects(node.shape()), (bubble.name, node.name))
        svg, _, _ = bs.render(data, nodes, bubbles)
        ET.fromstring(svg)
        return nodes, bubbles, svg

    def test_routes_around_nonmember_between_members(self):
        _, bubbles, _ = self.faithful({
            "groups": {"pair": ["left", "right"], "obstacle": ["middle"]},
            "positions": {"left": [0, 0], "middle": [200, 0], "right": [400, 0]},
        })
        self.assertEqual(len(bubbles), 2)

    def test_crossing_memberships(self):
        self.faithful({
            "groups": {"horizontal": ["left", "right"], "vertical": ["top", "bottom"],
                       "diagonal": ["top", "right"], "center": ["center"]},
            "positions": {"left": [-240, 0], "right": [240, 0], "top": [0, -200],
                          "bottom": [0, 200], "center": [0, 0]},
        })

    def test_empty_singleton_duplicate_groups_and_ungrouped_agent(self):
        _, bubbles, svg = self.faithful({
            "groups": {"empty": [], "one": ["member"], "same": ["member"]},
            "agents": ["outsider"], "positions": {"member": [0, 0], "outsider": [220, 0]},
        })
        self.assertEqual(bubbles[0].path, bubbles[1].path)
        self.assertIn("no agents", svg)
        self.faithful({"groups": {}})

    def test_escaping_and_long_names(self):
        _, _, svg = self.faithful({"groups": {'<group & "friends">': ['A<&"', 'A very long agent name']},
                                   "positions": {'A<&"': [0, 0], 'A very long agent name': [400, 0]}})
        root = ET.fromstring(svg)
        names = [node.text for node in root.findall(".//{*}text") if node.get("class") == "name"]
        self.assertEqual(set(names), {'A<&"', 'A very long agent name'})

    @unittest.skipUnless(shutil.which("dot"), "Graphviz required")
    def test_arbitrary_layout_and_requested_hierarchy(self):
        data = {"groups": {"council": ["lead", "east", "west"], "east-team": ["east", "a", "b"],
                           "west-team": ["west", "c", "d"], "empty": []}, "agents": ["solo"]}
        self.faithful(data)
        nodes, _, svg = self.faithful(data, "council")
        _, _, again = self.faithful(data, "council")
        self.assertEqual(svg, again)
        by_name = {n.name: n for n in nodes}
        self.assertLess(by_name["east"].y, by_name["a"].y)
        self.assertLess(by_name["lead"].y, by_name["east"].y)

    @unittest.skipUnless(shutil.which("dot"), "Graphviz required")
    def test_overlapping_generated_mappings(self):
        for seed in range(6):
            rng = random.Random(seed)
            agents = [f"agent-{i}" for i in range(12)]
            groups = {f"team-{j}": rng.sample(agents, rng.randint(2, 6)) for j in range(6)}
            with self.subTest(seed=seed):
                self.faithful({"agents": agents, "groups": groups})

    def test_bbs_input_ignores_humans_and_retired_includes_ungrouped(self):
        with patch.object(bs, "command", side_effect=[
            ('{"name":"team","members":["Alice","Bob"],"description":"Ships the: thing, with Carol",'
             '"writers":["Alice"]}\n{"name":"empty","members":[],"description":null,"writers":[]}\n'),
            ("NAME KIND GROUPS ACTIVE_AT WHERE STATUS\nWill human * now\nAlice agent team now\n"
            "Bob agent team now\nSolo agent - now\nOld retired - now\n"),
        ]) as run:
            data = bs.read_bbs("custom-bbs")
        self.assertEqual(data["agents"], ["Alice", "Bob", "Solo"])
        self.assertEqual(data["groups"], {"empty": [], "team": ["Alice", "Bob"]})
        self.assertEqual(run.call_args_list[0].args[0], ["custom-bbs", "group", "list", "--json"])
        for output in ("unexpected output", '{"members": []}\n'):
            with (patch.object(bs, "command", return_value=output),
                  self.assertRaisesRegex(ValueError, "unrecognized")):
                bs.read_bbs()

    def test_rejects_invalid_input_and_impossible_fixed_positions(self):
        for data in ({}, {"groups": []}, {"groups": {"g": "Alice"}},
                     {"groups": {"g": [""]}}, {"groups": {"g": ["A"]}, "positions": {}},
                     {"groups": {"g": ["A"]}, "positions": {"A": [float("nan"), 0]}}):
            with self.subTest(data=data), self.assertRaises((TypeError, ValueError)):
                bs.normalize(data)
        with self.assertRaisesRegex(ValueError, "overlap"):
            bs.build(bs.normalize({"groups": {"g": ["A", "B"]},
                                   "positions": {"A": [0, 0], "B": [0, 0]}}))

    def test_cli_snapshot_png_dimensions_and_error_without_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, svg, png, snapshot = [root / name for name in ("in.json", "out.svg", "out.png", "snapshot.json")]
            data = {"groups": {"pair": ["A", "B"]}, "positions": {"A": [0, 0], "B": [220, 0]}}
            source.write_text(json.dumps(data))
            command = [sys.executable, str(SCRIPT), "--input", str(source), "--output", str(svg),
                       "--png", str(png), "--snapshot", str(snapshot), "--longest-edge", "4096"]
            result = subprocess.run(command, check=False, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(snapshot.read_text()), bs.normalize(data))
            ET.parse(svg)
            image = png.read_bytes()
            self.assertEqual(image[:8], b"\x89PNG\r\n\x1a\n")
            self.assertEqual(max(struct.unpack(">II", image[16:24])), 4096)
            before = svg.read_bytes()
            failed = subprocess.run(command + ["--root-group", "missing"], check=False, capture_output=True, text=True)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("unknown root group", failed.stderr)
            self.assertEqual(svg.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
