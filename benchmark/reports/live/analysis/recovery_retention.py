"""Recovery, retention and failure evidence for the live run (report Sections 4.4 and 4.5).

    benchmark/.venv/bin/python benchmark/reports/live/analysis/recovery_retention.py

A task is *changed* at a stage when a tool its test case lists differs in name, description or
input schema between the original KB and that stage's KB; otherwise it is *unchanged* (should
remain solvable). Reads the live run evidence and writes recovery_retention.json next to this file.
"""
from __future__ import annotations

import json
from pathlib import Path
import random
import statistics

LIVE = Path(__file__).resolve().parents[1]
ROOT = LIVE.parents[2]
STRATEGIES = ("no_update", "selective_patch", "full_rewrite")
FIELDS = ("name", "description", "input_schema")


def kb(path):
    return json.loads(Path(path).read_text(encoding="utf-8"))["cards"]


def resolve(name, servers):
    hits = [s for s in servers if s == name or s == f"@{name}" or s.endswith(f"/{name}")]
    return hits[0] if hits else None


def changed_tasks(cases, early, target):
    src = {(c["server_id"], c["tool_id"]): c for c in early}
    tgt = {(c["server_id"], c["tool_id"]): c for c in target}
    servers = {c["server_id"] for c in early} | {c["server_id"] for c in target}
    out = set()
    for case in cases:
        for server, tools in case["servers"].items():
            sid = resolve(server, servers)
            for tool in tools:
                a, b = src.get((sid, tool)), tgt.get((sid, tool))
                if a is None and b is None:
                    continue
                if a is None or b is None or any(a[f] != b[f] for f in FIELDS):
                    out.add(str(case["idx"]))
    return out


def paired(a, b, samples=10_000, seed=0):
    diffs = [x - y for x, y in zip(a, b)]
    rng = random.Random(seed)
    boots = sorted(statistics.mean(rng.choice(diffs) for _ in diffs) for _ in range(samples))
    return {"mean": statistics.mean(diffs), "wins": sum(d > 0 for d in diffs),
            "losses": sum(d < 0 for d in diffs), "ties": sum(d == 0 for d in diffs),
            "ci95": [boots[int(0.025 * samples)], boots[int(0.975 * samples) - 1]]}


def card_audit(early, target, adapted):
    """Under-update: changed descriptions left at the source text. Over-update: unchanged cards
    whose description or usage rules differ from the source card."""
    e = {c["lineage_id"]: c for c in early}
    t = {c["lineage_id"]: c for c in target}
    a = {c["lineage_id"]: c for c in adapted}
    changed = [l for l in t if l in e and any(e[l][f] != t[l][f] for f in FIELDS)]
    unchanged = [l for l in t if l in e and l not in changed]
    missed = [l for l in changed if e[l]["description"] != t[l]["description"]
              and a.get(l, {}).get("description") == e[l]["description"]]
    rewritten = [l for l in unchanged if l in a and (a[l]["description"] != e[l]["description"]
                                                     or a[l]["usage_rules"] != e[l]["usage_rules"])]
    return {"changed_cards": len(changed), "missed_description_changes": len(missed),
            "unchanged_cards": len(unchanged), "unchanged_cards_rewritten": len(rewritten)}


def main():
    summary = json.loads((LIVE / "summary.json").read_text(encoding="utf-8"))["stages"]
    cases = json.loads((ROOT / "benchmark/src/custom/data/test_cases/pilot_20_clean.json").read_text())
    kbs = {s: kb(LIVE / f"tool-listings/kb-{s}.json") for s in ("early", "middle", "late")}
    tasks = sorted(summary["early"]["scores"]["no_update"]["tf"], key=int)
    result = {"groups": {}, "card_audit": {}, "failed_calls": []}
    for stage in ("middle", "late"):
        changed = changed_tasks(cases, kbs["early"], kbs[stage])
        for group, members in (("changed", sorted(changed, key=int)),
                               ("unchanged", [t for t in tasks if t not in changed])):
            row = {"tasks": members}
            for at in ("early", stage):
                row[at] = {s: {m: statistics.mean(summary[at]["scores"][s][m][t] for t in members)
                               for m in ("tf", "pe")} for s in STRATEGIES}
            row["paired"] = {f"{a} - {b}": {m: paired([summary[stage]["scores"][a][m][t] for t in members],
                                                      [summary[stage]["scores"][b][m][t] for t in members])
                                            for m in ("tf", "pe")}
                             for a, b in (("selective_patch", "no_update"),
                                          ("selective_patch", "full_rewrite"))}
            result["groups"][f"{stage}/{group}"] = row
        for strategy in ("selective_patch", "full_rewrite"):
            result["card_audit"][f"{stage}/{strategy}"] = card_audit(
                kbs["early"], kbs[stage], kb(LIVE / f"runs/{stage}/kb/{strategy}-rep1.json"))
    for stage in ("early", "middle", "late"):
        for line in (LIVE / f"runs/{stage}/events.jsonl").read_text(encoding="utf-8").splitlines():
            e = json.loads(line)
            if e["event_type"] == "mcp_action" and e.get("is_error"):
                result["failed_calls"].append({"stage": stage, "strategy": e["strategy"],
                                               "task": e["task_id"], "step": e["step"],
                                               "tool": e["tool_id"].split("_1mcp_")[-1],
                                               "result": str(e["result_summary"])[:200]})
    out = Path(__file__).with_name("recovery_retention.json")
    out.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    for name, row in result["groups"].items():
        stage = name.split("/")[0]
        print(name, len(row["tasks"]), {s: {m: round(v, 2) for m, v in row[stage][s].items()} for s in STRATEGIES})
        for comp, p in row["paired"].items():
            print("   ", comp, {m: (round(q["mean"], 3), q["wins"], q["losses"], q["ties"],
                                   [round(x, 3) for x in q["ci95"]]) for m, q in p.items()})
    print(json.dumps(result["card_audit"]))


if __name__ == "__main__":
    main()
