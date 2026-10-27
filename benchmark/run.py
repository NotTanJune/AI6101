"""Root-runnable commands. Explicit path arguments resolve from the caller's cwd."""
from __future__ import annotations

import os
from pathlib import Path
import runpy
import sys

ROOT = Path(__file__).resolve().parent
COMMANDS = {
    "audit": "integration.audit",
    "tool-kb": "integration.tool_kb",
    "baseline": "custom.real_mcp.tool_level_test",
    "selective": "custom.real_mcp.selective_patch_runner",
    "judge": "custom.analysis.answer.evaluation",
    "ecs": "custom.analysis.answer.evolutionary_volatility",
    "rust-runner": "integration.rust_runner",
    "rust-ecs": "integration.rust_ecs",
}
PATH_FLAGS = {
    "--test_case_file", "--mutation_path", "--source_snapshot", "--target_snapshot",
    "--harness_snapshot", "--save_harness_snapshot", "--output_path",
    "--answer_path", "--mutation_answer_path", "--tool_snapshot",
    "--dir_a", "--dir_b", "--dir_c", "--out", "--data-root",
    "--input", "--output",
    "--tool-kb", "--output-jsonl",
    "--early-run", "--middle-run", "--late-run",
    "--original-run", "--iter3-run", "--iter5-run",
}


def resolve_paths(args: list[str], cwd: Path) -> list[str]:
    resolved = list(args)
    for i, arg in enumerate(args):
        flag, sep, value = arg.partition("=")
        if flag in PATH_FLAGS:
            if not sep:
                if i + 1 == len(args) or args[i + 1].startswith("--"):
                    continue  # Let argparse explain a missing argument.
                value = args[i + 1]
            if value:
                path = str((cwd / Path(value).expanduser()).resolve())
                resolved[i if sep else i + 1] = f"{flag}={path}" if sep else path
    return resolved


def main() -> None:
    if len(sys.argv) < 2 or sys.argv[1] in {"-h", "--help"}:
        print("Usage: python benchmark/run.py COMMAND [arguments]")
        print("Commands: " + ", ".join(COMMANDS))
        print("audit and tool-kb are offline. baseline, selective and judge can spend API credits.")
        return
    command = sys.argv[1]
    if command not in COMMANDS:
        raise SystemExit(f"Unknown command: {command}")
    args = resolve_paths(sys.argv[2:], Path.cwd())
    sys.path.insert(0, str(ROOT))
    sys.path.insert(0, str(ROOT / "src"))
    # Legacy defaults use custom/data paths relative to src.
    os.chdir(ROOT / "src")
    sys.argv = [f"benchmark/run.py {command}", *args]
    runpy.run_module(COMMANDS[command], run_name="__main__")


if __name__ == "__main__":
    main()
