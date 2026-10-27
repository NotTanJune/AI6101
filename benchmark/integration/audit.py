"""Reproduce the reported pilot scores from committed artifacts, without APIs."""
from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
from pathlib import Path
import statistics

from custom.utils.judge_scores import METRICS, valid_scores

BENCHMARK_ROOT = Path(__file__).resolve().parents[1]
DATA_ROOT = BENCHMARK_ROOT / "src/custom/data"
VERSIONS = ("original", "iter3", "iter5")
CONDITIONS = {
    "no_update": ("benchmark_clean_r1/no_update_",) * 3,
    "selective_patch": ("benchmark_clean_r2/selective_",) * 3,
    "full_rewrite": ("benchmark_clean_r2/full_rewrite_",
                     "benchmark_clean_r1/full_rewrite_", "benchmark_clean_r1/full_rewrite_"),
}
REPORTED = {
    "no_update": ((6.120, 8.990, 8.420), (7.090, 8.540, 8.320), (6.620, 9.110, 8.350)),
    "selective_patch": ((6.400, 8.530, 9.000), (6.480, 8.370, 8.380), (6.710, 8.770, 8.390)),
    "full_rewrite": ((6.490, 8.910, 8.390), (5.650, 8.470, 9.020), (6.230, 8.660, 8.390)),
}
ECS_FILES = {
    "no_update": "benchmark_clean_r1/ECS_NO_UPDATE.json",
    "selective_patch": "benchmark_clean_r2/ECS_SELECTIVE.json",
    "full_rewrite": "benchmark_clean_r2/ECS_FULL_REWRITE.json",
}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def close(actual, expected, label):
    require(type(actual) in (int, float) and math.isfinite(actual)
            and math.isclose(actual, expected, abs_tol=1e-9, rel_tol=0),
            f"{label}: stored {actual!r}, recomputed {expected!r}")


def validate_judgments(payload: dict, label: str) -> dict:
    packed = payload["eval_result"]
    runs = packed["runs"]
    valid = [run["result"] for run in runs if valid_scores(run.get("result"))]
    # Parse/API errors may be retained, but malformed numeric scores are never ignored.
    require(all(valid_scores(run.get("result")) or
                (isinstance(run.get("result"), dict) and "error" in run["result"])
                for run in runs), f"{label}: invalid judge score")
    require(len(valid) == 5, f"{label}: expected five valid judgments, got {len(valid)}")
    aggregate = packed["aggregate"]
    require(aggregate["successful_runs"] == 5 and aggregate["total_runs"] == len(runs),
            f"{label}: judgment count mismatch")
    means = {metric: statistics.mean(run[metric] for run in valid) for metric in METRICS}
    for metric, mean in means.items():
        close(aggregate[f"{metric}_mean"], mean, f"{label}/{metric}")
    return means


def aligned_files(directory: Path, ids: set[int]) -> dict[int, Path]:
    require(directory.is_dir(), f"Missing artifact directory: {directory}")
    files = list(directory.glob("*.json"))
    require({file.name for file in files} == {f"{idx}.json" for idx in ids},
            f"{directory}: task files do not match the frozen pilot")
    return {idx: directory / f"{idx}.json" for idx in sorted(ids)}


def ecs(task_scores: list[list[float]]) -> float:
    require(bool(task_scores) and all(len(scores) == 3 for scores in task_scores),
            "ECS needs three aligned version scores per task")
    return statistics.mean(statistics.mean(scores) - statistics.pstdev(scores)
                           for scores in task_scores)


def audit(data_root: Path = DATA_ROOT) -> dict:
    pilot = read_json(data_root / "test_cases/pilot_20_clean.json")
    ids = {case["idx"] for case in pilot}
    require(len(pilot) == len(ids) == 20, "Pilot must contain 20 distinct task IDs")
    result = {
        "source_commit": read_json(BENCHMARK_ROOT / "UPSTREAM_IMPORT.json")["source_commit"],
        "artifact_kind": "historical_python_benchmark_audit",
        "task_ids": sorted(ids), "conditions": [], "ecs": {}, "selective_adaptation": {},
        "judgments_per_trajectory": 5, "saved_trajectories_per_task_condition": 1,
        "reported_score_source": "user-provided AI Ethics rerun results, 2026-10-26",
        "evidence_sha256": {},
    }

    def evidence(path):
        result["evidence_sha256"][path.relative_to(data_root).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
        return read_json(path)

    evidence(data_root / "test_cases/pilot_20_clean.json")
    for strategy, prefixes in CONDITIONS.items():
        by_version = []
        for version_index, (version, prefix) in enumerate(zip(VERSIONS, prefixes)):
            root = data_root / (prefix + version) / "gpt-6-luna"
            answers = aligned_files(root / "ANSWER", ids)
            evaluations = aligned_files(root / "EVAL", ids)
            task_scores = {}
            forced = []
            for idx in sorted(ids):
                answer, evaluation = evidence(answers[idx]), evidence(evaluations[idx])
                require(answer["idx"] == evaluation["idx"] == idx, f"{root}/{idx}: ID mismatch")
                for field in ("question", "answer", "trajectory_messages"):
                    require(answer[field] == evaluation[field], f"{root}/{idx}: {field} differs between ANSWER and EVAL")
                task_scores[str(idx)] = validate_judgments(evaluation, f"{strategy}/{version}/{idx}")
                if answer.get("forced_finalization"):
                    forced.append(idx)
            means = {metric: statistics.mean(scores[metric] for scores in task_scores.values())
                     for metric in METRICS}
            for metric, reported in zip(METRICS, REPORTED[strategy][version_index]):
                close(means[metric], reported, f"Reported {strategy}/{version}/{metric}")
            result["conditions"].append({"strategy": strategy, "version": version,
                "artifact_path": root.relative_to(data_root).as_posix(), "task_count": len(ids),
                "scores": means, "task_scores": task_scores, "forced_finalization_task_ids": forced})
            by_version.append(task_scores)
            if strategy == "selective_patch":
                audits = [evidence(path) for path in aligned_files(root / "PATCH_AUDIT", ids).values()]
                for idx, item in zip(sorted(ids), audits):
                    require(item["task_id"] == idx and item["unchanged_hash_check"] is True,
                            f"Selective audit {version}/{idx}: ID or unchanged-card hash check failed")
                    ledger, summary = item["cost_ledger"], item["cost_summary"]
                    require(len(ledger) == summary["model_calls"], f"Audit {version}/{idx}: call count mismatch")
                    for field in ("input_tokens", "output_tokens", "latency_ms"):
                        close(summary[field], sum(call[field] for call in ledger), f"Audit {version}/{idx}/{field}")
                calls = [call for item in audits for call in item["cost_ledger"]]
                result["selective_adaptation"][version] = {
                    "tasks_with_changes": sum(item["changed_card_count"] > 0 for item in audits),
                    "model_calls": len(calls),
                    **{field: sum(call[field] for call in calls)
                       for field in ("input_tokens", "output_tokens", "latency_ms")},
                    "adaptation_time_ms": sum(item["adaptation_time_ms"] for item in audits),
                    "estimated_usd": (sum(call["estimated_usd"] for call in calls)
                                      if all(call["estimated_usd"] is not None for call in calls) else None),
                    "warnings": {str(item["task_id"]): item["warnings"] for item in audits if item["warnings"]},
                }
        value = ecs([[scores[str(idx)]["task_fulfillment"] for scores in by_version] for idx in sorted(ids)])
        stored = evidence(data_root / ECS_FILES[strategy])
        close(stored["summary"]["ecs"], value, f"Stored {strategy} ECS")
        require(stored["summary"]["tasks_count"] == len(ids), f"Stored {strategy} ECS task count differs")
        result["ecs"][strategy] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data-root", type=Path, default=DATA_ROOT)
    parser.add_argument("--out", type=Path, help="Write the verified JSON summary and a sibling CSV")
    args = parser.parse_args()
    result = audit(args.data_root)
    for condition in result["conditions"]:
        scores = " ".join(f"{key}={value:.3f}" for key, value in condition["scores"].items())
        print(f"{condition['strategy']:16} {condition['version']:8} {scores}")
    print("ECS: " + ", ".join(f"{key}={value:.6f}" for key, value in result["ecs"].items()))
    print("PASS: 180 trajectories, 900 valid judgments, 27 reported means, three ECS values, 60 patch audits")
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        with args.out.with_suffix(".csv").open("w", newline="", encoding="utf-8") as stream:
            writer = csv.DictWriter(stream, fieldnames=["strategy", "version", "task_count", *METRICS])
            writer.writeheader()
            for condition in result["conditions"]:
                writer.writerow({key: condition[key] for key in ("strategy", "version", "task_count")} | condition["scores"])


if __name__ == "__main__":
    main()
