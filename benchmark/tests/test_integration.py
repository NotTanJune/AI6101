from __future__ import annotations

import copy
import importlib
import json
from pathlib import Path
import socket
import statistics
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "benchmark"), str(ROOT / "benchmark/src")]

from integration.audit import aligned_files, audit, ecs, validate_judgments
from integration.tool_kb import from_snapshot, to_snapshot, validate_kb
from run import resolve_paths


class ToolKBTests(unittest.TestCase):
    def setUp(self):
        self.kb = json.loads((ROOT / "fixtures/contracts/tool-kb.valid.json").read_text())

    def test_round_trip_preserves_all_contract_fields(self):
        self.kb["cards"][0]["usage_rules"] = ["Use Celsius"]
        self.kb["cards"][0]["output_schema"] = {"type": "object"}
        converted = from_snapshot(to_snapshot(self.kb), release_id=self.kb["release_id"],
                                  server_version=self.kb["server_version"], source_file="fixture")
        self.assertEqual(converted, self.kb)

    def test_stale_embedded_metadata_is_rejected(self):
        snapshot = to_snapshot(self.kb)
        snapshot["tools"][0]["description"] = "Changed"
        with self.assertRaisesRegex(ValueError, "stale"):
            from_snapshot(snapshot, release_id=self.kb["release_id"],
                          server_version=self.kb["server_version"], source_file="fixture")

    def test_duplicate_route_rejected(self):
        card = copy.deepcopy(self.kb["cards"][0])
        card["lineage_id"] = "different-lineage"
        self.kb["cards"].append(card)
        with self.assertRaisesRegex(ValueError, "Duplicate"):
            validate_kb(self.kb)

    def test_ambiguous_route_rejected(self):
        self.kb["cards"][0]["tool_id"] = "a_1mcp_b"
        with self.assertRaisesRegex(ValueError, "Ambiguous"):
            to_snapshot(self.kb)

    def test_invalid_contract_rejected(self):
        del self.kb["cards"][0]["provenance"]
        with self.assertRaises(Exception):
            validate_kb(self.kb)

    def test_legacy_schema_is_preserved_as_contract_object(self):
        schema = {"type": "number", "minimum": 0, "exclusiveMinimum": True}
        snapshot = {"format_version": 1, "tools": [
            {"name": "server_1mcp_tool", "description": "Exact text", "inputSchema": schema}]}
        kb = from_snapshot(snapshot, release_id="r", server_version="original", source_file="snapshot.json")
        self.assertEqual(kb["cards"][0]["input_schema"], schema)
        self.assertEqual(kb["cards"][0]["provenance"]["source_pointer"], "/tools/0")
        self.assertEqual(to_snapshot(kb)["tools"], snapshot["tools"])


class AuditTests(unittest.TestCase):
    def test_all_committed_results_match_report(self):
        result = audit()
        self.assertEqual(len(result["conditions"]), 9)
        self.assertEqual(result["selective_adaptation"]["iter5"]["model_calls"], 19)
        self.assertIsNone(result["selective_adaptation"]["iter5"]["estimated_usd"])

    def test_ecs_uses_per_task_population_sd(self):
        values = [[1, 5, 9], [9, 5, 1]]
        self.assertAlmostEqual(ecs(values), 5 - statistics.pstdev([1, 5, 9]))
        self.assertNotEqual(ecs(values), 5)

    def test_missing_task_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            (Path(temp) / "1.json").write_text("{}")
            with self.assertRaisesRegex(ValueError, "frozen pilot"):
                aligned_files(Path(temp), {1, 2})

    def test_invalid_score_and_corrupt_mean_rejected(self):
        path = ROOT / "benchmark/src/custom/data/benchmark_clean_r2/selective_iter3/gpt-6-luna/EVAL/161.json"
        original = json.loads(path.read_text())
        for bad in [True, 11, 0, 6.5, "7", None]:
            with self.subTest(value=bad):
                data = copy.deepcopy(original)
                data["eval_result"]["runs"][0]["result"]["grounding"] = bad
                with self.assertRaises(ValueError):
                    validate_judgments(data, "fixture")
        original["eval_result"]["aggregate"]["grounding_mean"] = float("nan")
        with self.assertRaises(ValueError):
            validate_judgments(original, "fixture")


class RuntimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with patch.object(socket.socket, "connect", side_effect=AssertionError("Unexpected network")):
            cls.judge = importlib.import_module("custom.analysis.answer.evaluation")
            cls.agent = importlib.import_module("custom.agents.SimpleOpenAIAgent")
            cls.selective = importlib.import_module("custom.real_mcp.selective_patch_runner")

    def test_imports_do_not_load_fastchat_or_torch(self):
        self.assertNotIn("custom.utils.fastchat_utils", sys.modules)
        self.assertNotIn("torch", sys.modules)

    def test_judge_uses_saved_tools_without_network(self):
        case = {"question": "Find it", "answer": "Found", "trajectory_messages": [],
                "involved_tools": [{"name": "saved_unique_tool", "description": "historic"}]}
        with patch.object(socket.socket, "connect", side_effect=AssertionError("Unexpected network")):
            self.assertIn("saved_unique_tool", self.judge.build_judge_user_content(case))
        del case["involved_tools"]
        with self.assertRaisesRegex(ValueError, "--tool_snapshot"):
            self.judge.build_judge_user_content(case)

    def test_judge_retries_invalid_scores_and_records_provenance(self):
        good = {"task_fulfillment": 8, "grounding": 9, "planning_and_efficiency": 7}
        bad = good | {"grounding": True}
        with patch.object(self.judge, "evaluate_one", side_effect=[bad, *[good] * 5]) as fake:
            result = self.judge.evaluate_one_five_times({}, model="offline-test")
        self.assertEqual(fake.call_count, 6)
        self.assertEqual(result["aggregate"]["successful_runs"], 5)
        self.assertEqual(result["judge_metadata"]["model"], "offline-test")
        self.assertEqual(len(result["judge_metadata"]["rubric_sha256"]), 64)

    def test_partial_or_invalid_judgments_do_not_count_as_success(self):
        results = [{"result": {"task_fulfillment": 10}}, {"result": {"error": "bad"}}]
        self.assertEqual(self.judge.aggregate_eval_runs(results)["successful_runs"], 0)

    def test_annotation_directory_resolves_from_root(self):
        from custom.utils.test_utils import ANNOTATION_ROOT, generate_tree_string
        self.assertEqual(ANNOTATION_ROOT, ROOT / "benchmark/anotation_path")
        self.assertNotIn("Error: Invalid directory", generate_tree_string("./anotation_path"))

    def test_frozen_chart_schema_is_not_rewritten(self):
        from types import SimpleNamespace
        from custom.real_mcp.tool_level_test import return_involved_tool_status
        schema = {"type": "object", "properties": {"kind": {"const": "chart"}}}
        status = SimpleNamespace(tools=[SimpleNamespace(
            name="s_1mcp_generate_chart", description="Exact", inputSchema=schema)])
        cards = return_involved_tool_status({"s": ["generate_chart"]}, status, preserve_schema=True)
        self.assertEqual(cards[0]["inputSchema"], schema)

    def test_path_arguments_resolve_from_callers_directory(self):
        self.assertEqual(resolve_paths(["--input=a.json", "--out", "b.json"], Path("/work")),
                         ["--input=/work/a.json", "--out", "/work/b.json"])

    def test_selective_patch_preserves_unaffected_card(self):
        old = {"server": "s", "name": "a", "description": "old", "inputSchema": {"type": "object"}}
        unchanged = old | {"name": "b"}
        original = {("s", "a"): old, ("s", "b"): unchanged}
        live = copy.deepcopy(original)
        live[("s", "a")]["description"] = "new"
        cards, routing, records, errors, warnings = self.selective.build_minimal_patch(
            {"s": ["a", "b"]}, {"s": ["a", "b"]}, original, live)
        self.assertEqual(errors, [])
        self.assertEqual(warnings, [])
        self.assertEqual(cards[1], unchanged)
        self.assertEqual(old["description"], "old")
        self.assertEqual(records[0]["change_classes"], ["DESC"])
        self.assertEqual(routing, {"a": "s", "b": "s"})

    def test_ecs_missing_metadata_is_correct(self):
        from custom.analysis.answer.evolutionary_volatility import compare_three_dirs
        with tempfile.TemporaryDirectory() as temp:
            dirs = [Path(temp) / name for name in ("a", "b", "c")]
            for directory in dirs:
                directory.mkdir()
                (directory / "1.json").write_text(json.dumps({"eval_result": {"task_fulfillment": 8}}))
            _, meta = compare_three_dirs(*(str(directory) for directory in dirs))
            self.assertEqual(meta["missing_in"], {"a": [], "b": [], "c": []})
            (dirs[1] / "2.json").write_text(json.dumps({"eval_result": {"task_fulfillment": 4}}))
            _, meta = compare_three_dirs(*(str(directory) for directory in dirs))
            self.assertEqual(meta["missing_in"], {"a": ["2.json"], "b": [], "c": ["2.json"]})


if __name__ == "__main__":
    unittest.main()
