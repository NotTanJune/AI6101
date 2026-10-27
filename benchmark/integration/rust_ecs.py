"""Compute per-task ECS from three completed Rust harness event logs."""
from __future__ import annotations

import argparse
import json
import math
from pathlib import Path
import statistics


def scores(path: Path) -> dict[str, dict[str, list[float]]]:
    result: dict[str, dict[str, list[float]]] = {}
    completions = []
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not line.strip():
            continue
        event = json.loads(line)
        if event.get("event_type") == "run_complete":
            completions.append(event.get("status"))
        if event.get("event_type") != "metric_score" or event.get("metric") != "tf":
            continue
        task = event.get("task_id")
        strategy = event.get("strategy")
        value = event.get("value")
        if not isinstance(task, str) or strategy not in {
            "no_update", "full_rewrite", "selective_patch"
        } or type(value) not in (int, float) or not math.isfinite(value):
            raise ValueError(f"{path}:{number}: malformed task-level task-fulfillment event")
        result.setdefault(strategy, {}).setdefault(task, []).append(float(value))
    if not completions or any(status != "ok" for status in completions):
        raise ValueError(f"{path}: requires successful Python stages and Rust run completion")
    return result


def summarize(early: Path, middle: Path, late: Path) -> dict:
    inputs = {"early": scores(early), "middle": scores(middle), "late": scores(late)}
    summary = {"formula": "mean_task(mean(version TF) - population_sd(version TF))",
               "versions": {key: str(value) for key, value in zip(inputs, (early, middle, late))},
               "strategies": {}}
    for strategy in ("selective_patch", "no_update", "full_rewrite"):
        by_version = {version: inputs[version].get(strategy, {}) for version in inputs}
        id_sets = [set(by_version[version]) for version in inputs]
        if not id_sets[0] or not all(ids == id_sets[0] for ids in id_sets[1:]):
            raise ValueError(f"{strategy}: task IDs differ or are empty across the three runs")
        rows = []
        for task in sorted(id_sets[0], key=lambda value: (not value.isdigit(), value)):
            values = [statistics.mean(by_version[version][task]) for version in inputs]
            rows.append({"task_id": task, "task_fulfillment": dict(zip(inputs, values)),
                         "mean": statistics.mean(values), "population_sd": statistics.pstdev(values),
                         "ecs": statistics.mean(values) - statistics.pstdev(values)})
        summary["strategies"][strategy] = {
            "tasks_count": len(rows), "ecs": statistics.mean(row["ecs"] for row in rows),
            "per_task": rows,
        }
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--early-run", "--original-run", dest="early_run", required=True, type=Path)
    parser.add_argument("--middle-run", "--iter3-run", dest="middle_run", required=True, type=Path)
    parser.add_argument("--late-run", "--iter5-run", dest="late_run", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if args.out.exists():
        parser.error(f"refusing to overwrite {args.out}")
    result = summarize(args.early_run, args.middle_run, args.late_run)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("x", encoding="utf-8") as stream:
        json.dump(result, stream, indent=2)
        stream.write("\n")
    for strategy, data in result["strategies"].items():
        print(f"{strategy}: ECS={data['ecs']:.6f} tasks={data['tasks_count']}")


if __name__ == "__main__":
    main()
