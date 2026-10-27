"""Summarize a completed live Early/Middle/Late run into report tables (JSON and Markdown).

    benchmark/.venv/bin/python benchmark/integration/live_report.py \
        --runs target/live/runs/full --out benchmark/reports/live

Reads each stage's Rust `events.jsonl` (which carries the Python runner's events). Writes
`summary.json` and `summary.md`. Never overwrites existing outputs.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
import json
from pathlib import Path
import random
import statistics

STAGES = ("early", "middle", "late")
STRATEGIES = ("no_update", "selective_patch", "full_rewrite")
METRICS = ("tf", "grounding", "pe")


def load(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def stage_summary(events: list[dict]) -> dict:
    scores = defaultdict(lambda: defaultdict(dict))      # strategy -> metric -> task -> value
    cost = defaultdict(lambda: defaultdict(lambda: {"calls": 0, "input_tokens": 0,
                                                    "output_tokens": 0, "usd": 0.0,
                                                    "unknown_calls": 0}))
    outcome, canary, completions, errors = {}, defaultdict(lambda: defaultdict(int)), [], []
    actions = defaultdict(lambda: {"calls": 0, "errors": 0})
    for e in events:
        kind, strategy = e["event_type"], e.get("strategy")
        if kind == "metric_score" and e.get("task_id") is not None:
            scores[strategy][e["metric"]][e["task_id"]] = e["value"]
        elif kind == "model_call":
            phase = "adaptation" if e["phase"] == "adaptation" else "task_and_judge"
            row = cost[strategy][phase]
            row["calls"] += 1
            if e.get("input_tokens") is None or e.get("estimated_usd") is None:
                row["unknown_calls"] += 1
            row["input_tokens"] += e.get("input_tokens") or 0
            row["output_tokens"] += e.get("output_tokens") or 0
            row["usd"] += e.get("estimated_usd") or 0.0
        elif kind == "adaptation_outcome":
            outcome[strategy] = {"status": e["status"], "reason": e["reason"],
                                 "changed_lineages": len(e.get("changed_lineage_ids") or [])}
        elif kind == "canary_result":
            canary[strategy][e["status"]] += 1
        elif kind == "mcp_action":
            actions[strategy]["calls"] += 1
            actions[strategy]["errors"] += bool(e.get("is_error"))
        elif kind == "run_complete":
            completions.append({"source": e["source"], "strategy": strategy, "status": e["status"]})
        elif kind == "error":
            errors.append({"strategy": strategy, "message": e["message"][:300]})
    means = {s: {m: statistics.mean(v.values()) if v else None
                 for m, v in ((m, scores[s][m]) for m in METRICS)} for s in STRATEGIES}
    return {"scores": {s: {m: dict(scores[s][m]) for m in METRICS} for s in STRATEGIES},
            "means": means, "cost": {s: dict(cost[s]) for s in STRATEGIES},
            "adaptation": outcome, "canary": {s: dict(canary[s]) for s in STRATEGIES},
            "mcp_actions": {s: actions[s] for s in STRATEGIES},
            "completions": completions, "errors": errors}


def ecs(per_stage: dict) -> dict:
    result = {}
    for strategy in STRATEGIES:
        tf = [per_stage[stage]["scores"][strategy]["tf"] for stage in STAGES]
        tasks = sorted(set(tf[0]) & set(tf[1]) & set(tf[2]), key=lambda t: (not t.isdigit(), int(t) if t.isdigit() else t))
        rows = {t: statistics.mean(v[t] for v in tf) - statistics.pstdev([v[t] for v in tf]) for t in tasks}
        result[strategy] = {"ecs": statistics.mean(rows.values()) if rows else None,
                            "tasks": len(rows), "per_task": rows}
    return result


def paired(ecs_rows: dict, a: str, b: str, samples: int = 10_000, seed: int = 0) -> dict:
    common = sorted(set(ecs_rows[a]["per_task"]) & set(ecs_rows[b]["per_task"]))
    diffs = [ecs_rows[a]["per_task"][t] - ecs_rows[b]["per_task"][t] for t in common]
    rng = random.Random(seed)
    boots = sorted(statistics.mean(rng.choice(diffs) for _ in diffs) for _ in range(samples))
    return {"mean": statistics.mean(diffs), "wins": sum(d > 0 for d in diffs),
            "losses": sum(d < 0 for d in diffs), "ties": sum(d == 0 for d in diffs),
            "ci95": [boots[int(0.025 * samples)], boots[int(0.975 * samples) - 1]],
            "tasks": len(diffs), "bootstrap": {"samples": samples, "seed": seed}}


def markdown(summary: dict) -> str:
    lines = ["# Live run summary", "", "| Strategy | Stage | TF | Grounding | PE |", "|---|---|---:|---:|---:|"]
    for s in STRATEGIES:
        for stage in STAGES:
            m = summary["stages"][stage]["means"][s]
            lines.append(f"| {s} | {stage} | " + " | ".join(
                "n/a" if m[k] is None else f"{m[k]:.3f}" for k in METRICS) + " |")
    lines += ["", "| Strategy | ECS | tasks |", "|---|---:|---:|"]
    for s in STRATEGIES:
        e = summary["ecs"][s]
        value = "n/a" if e["ecs"] is None else f"{e['ecs']:.4f}"
        lines.append(f"| {s} | {value} | {e['tasks']} |")
    lines += ["", "| Comparison | mean diff | W/L/T | 95% CI |", "|---|---:|---|---|"]
    for name, p in summary["paired"].items():
        lines.append(f"| {name} | {p['mean']:+.3f} | {p['wins']}/{p['losses']}/{p['ties']} | "
                     f"[{p['ci95'][0]:.3f}, {p['ci95'][1]:.3f}] |")
    lines += ["", "| Strategy | Stage | Adaptation | Canary | Adapt calls | In tok | Out tok | Adapt USD | Task+judge USD |",
              "|---|---|---|---|---:|---:|---:|---:|---:|"]
    for s in STRATEGIES:
        for stage in STAGES:
            st = summary["stages"][stage]
            a = st["cost"][s].get("adaptation", {"calls": 0, "input_tokens": 0, "output_tokens": 0, "usd": 0.0})
            t = st["cost"][s].get("task_and_judge", {"usd": 0.0})
            lines.append(f"| {s} | {stage} | {st['adaptation'].get(s, {}).get('status', 'n/a')} | "
                         f"{st['canary'][s] or 'none'} | {a['calls']} | {a['input_tokens']} | "
                         f"{a['output_tokens']} | {a['usd']:.4f} | {t['usd']:.4f} |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--runs", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    stages = {stage: stage_summary(load(args.runs / stage / "events.jsonl")) for stage in STAGES}
    rows = ecs(stages)
    summary = {"runs": str(args.runs), "stages": stages, "ecs": rows,
               "paired": {"selective_patch - no_update": paired(rows, "selective_patch", "no_update"),
                          "selective_patch - full_rewrite": paired(rows, "selective_patch", "full_rewrite"),
                          "no_update - full_rewrite": paired(rows, "no_update", "full_rewrite")}}
    args.out.mkdir(parents=True, exist_ok=True)
    with (args.out / "summary.json").open("x", encoding="utf-8") as stream:
        json.dump(summary, stream, indent=2)
    with (args.out / "summary.md").open("x", encoding="utf-8") as stream:
        stream.write(markdown(summary))
    print(markdown(summary))


if __name__ == "__main__":
    main()
