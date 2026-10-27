"""ProcessRunner entry point for live MCPEvol tool execution and judging."""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import random
import sys
import threading

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(ROOT / "src")]

from jsonschema import Draft202012Validator
from openai import OpenAI

from integration.tool_kb import validate_kb
from custom.agents.SimpleOpenAIAgent import ChatModel
from custom.utils.budget_guard import BudgetExceededError, BudgetGuard
from custom.utils.judge_scores import METRICS, valid_scores
from custom.utils.test_utils import ANNOTATION_ROOT, generate_tree_string


class EventStream:
    def __init__(self, path: Path, run_id: str, strategy: str):
        path.parent.mkdir(parents=True, exist_ok=True)
        self.stream = path.open("w", encoding="utf-8", buffering=1)
        schema = json.loads((ROOT.parent / "schemas/run-event.schema.json").read_text())
        self.validate = Draft202012Validator(schema)
        self.run_id, self.strategy, self.seq = run_id, strategy, 0
        self.lock = threading.Lock()

    def emit(self, body: dict):
        with self.lock:
            self._emit(body)

    def _emit(self, body: dict):
        event = {
            "schema_version": "run-event/1", "run_id": self.run_id, "seq": self.seq,
            "ts": datetime.now(timezone.utc).isoformat(), "source": "python",
            "strategy": self.strategy, **body,
        }
        self.validate.validate(event)
        self.stream.write(json.dumps(event, ensure_ascii=False, separators=(",", ":")) + "\n")
        self.stream.flush()
        self.seq += 1

    def close(self):
        self.stream.close()


class RunnerArgumentParser(argparse.ArgumentParser):
    """Usage errors must not exit 2: the Rust process runner reads exit 2 as budget exhaustion."""

    def error(self, message):
        self.print_usage(sys.stderr)
        self.exit(64, f"{self.prog}: error: {message}\n")


def parse_args(argv=None):
    parser = RunnerArgumentParser(description=__doc__)
    # Rust prepends its configured fixed arguments, so options may precede the subcommand.
    parser.add_argument("command", nargs="?", choices=["run"], default="run")
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--strategy", required=True,
                        choices=["no_update", "full_rewrite", "selective_patch"])
    parser.add_argument("--tool-kb", type=Path, required=True)
    parser.add_argument("--source-version", required=True)
    parser.add_argument("--server-version", required=True)
    parser.add_argument("--task-id", action="append", required=True)
    parser.add_argument("--provider", required=True, choices=["openai", "local", "mock", "gemini"])
    parser.add_argument("--api-key-env", default="OPENAI_API_KEY")
    parser.add_argument("--eval-data-env", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--temperature", type=float)
    parser.add_argument("--top-p", type=float)
    parser.add_argument("--max-output-tokens", type=int)
    parser.add_argument("--seed", type=int, required=True)
    parser.add_argument("--judge-version", required=True)
    parser.add_argument("--output-jsonl", type=Path, required=True)
    parser.add_argument("--remaining-budget-usd", type=float, required=True)
    parser.add_argument("--rate-card-json")
    parser.add_argument("--mcp-server-url", default=os.environ.get("MCP_SERVER_URL", "http://127.0.0.1:3050/mcp"))
    parser.add_argument("--mock", action="store_true")
    parser.add_argument("--workers", type=int, default=1,
                        help="tasks run concurrently against the same MCP endpoint")
    return parser.parse_args(sys.argv[1:] if argv is None else argv)


def load_cases(data_root: Path, wanted: set[str]) -> dict[str, dict]:
    candidates = [data_root / "test_cases/pilot_20_clean.json",
                  data_root / "test_cases/test_cases.json"]
    found = {}
    for path in candidates:
        if not path.is_file():
            continue
        payload = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(payload, dict):
            payload = payload.get("samples", payload.get("data", []))
        for case in payload:
            key = str(case.get("idx", ""))
            if key in wanted:
                found.setdefault(key, case)
    missing = sorted(wanted - set(found))
    if missing:
        raise ValueError(f"requested task IDs are absent from {data_root}/test_cases: {missing}")
    return found


def resolve_server(name: str, server_ids: set[str]) -> str | None:
    """The KB server for a task's server name, or None when the KB has no such server (its
    tools are then unavailable to the agent, as for a server absent from the 1MCP config)."""
    if name in server_ids:
        return name
    candidates = [server for server in server_ids
                  if server == f"@{name}" or server.endswith(f"/{name}")]
    if len(candidates) > 1:
        raise ValueError(f"task server {name!r} is ambiguous in the supplied Tool KB: {candidates}")
    return candidates[0] if candidates else None


def tools_for_case(case: dict, kb: dict) -> tuple[list[dict], dict[str, str]]:
    by_route = {(card["server_id"], card["tool_id"]): card for card in kb["cards"]}
    server_ids = {card["server_id"] for card in kb["cards"]}
    selected, routing = [], {}
    for task_server, names in case["servers"].items():
        canonical = resolve_server(task_server, server_ids)
        if canonical is None:
            continue
        for tool_id in names:
            card = by_route.get((canonical, tool_id))
            if card is None:
                continue  # Preserve task definition; agent sees only cards in active KB.
            function_name = card["tool_id"]
            if function_name in routing and routing[function_name] != canonical:
                raise ValueError(f"ambiguous MCP function name {function_name!r} across task servers")
            description = card["description"]
            if card["usage_rules"]:
                description += "\n\nUsage rules:\n" + "\n".join(f"- {rule}" for rule in card["usage_rules"])
            selected.append({"name": function_name, "description": description,
                             "inputSchema": card["input_schema"]})
            routing[function_name] = canonical
    if not selected:
        raise ValueError(f"task {case.get('idx')} has no tools in the active Tool KB")
    return selected, routing


def stable_task_seed(seed: int, task_id: str) -> int:
    digest = hashlib.sha256(f"{seed}:{task_id}".encode()).digest()
    return int.from_bytes(digest[:8], "big")


def main() -> int:
    args = parse_args()
    if not args.max_output_tokens:
        args.max_output_tokens = 4096
    stream = EventStream(args.output_jsonl, args.run_id, args.strategy)
    status, detail, exit_code = "ok", None, 0
    try:
        raw_kb = json.loads(args.tool_kb.read_text(encoding="utf-8"))
        validate_kb(raw_kb, uniform_versions=False)
        if raw_kb["server_version"] != args.server_version:
            raise ValueError("Tool KB server_version does not match requested server version")
        if not args.remaining_budget_usd >= 0:
            raise ValueError("remaining budget must be non-negative")
        if args.provider == "mock" or args.mock:
            raise ValueError("mock mode has no deterministic agent/tool fixture; refusing to fabricate scores")
        if args.provider == "gemini":
            raise ValueError("the imported agent uses the OpenAI chat-completions client; configure an OpenAI-compatible provider")
        if not os.environ.get(args.eval_data_env):
            raise ValueError(f"evaluation data environment variable {args.eval_data_env} is not set")
        rate_card = json.loads(args.rate_card_json) if args.rate_card_json else None
        if args.provider == "openai" and rate_card is None:
            raise ValueError("paid process runs require the Rust rate card")
        data_root = Path(os.environ[args.eval_data_env]).expanduser().resolve()
        cases = load_cases(data_root, set(args.task_id))
        if len(set(args.task_id)) != len(args.task_id):
            raise ValueError("duplicate --task-id values are not allowed")

        key = os.environ.get(args.api_key_env)
        if args.provider == "openai" and not key:
            raise ValueError(f"provider key environment variable {args.api_key_env} is empty")
        if args.provider == "local" and not os.environ.get("MODEL_URL"):
            raise ValueError("local OpenAI-compatible runs need provider.base_url")

        guard = BudgetGuard(provider=args.provider, model=args.model,
                            remaining_usd=args.remaining_budget_usd,
                            max_output_tokens=args.max_output_tokens,
                            rate_card=rate_card, emit=stream.emit,
                            run_id=args.run_id, strategy=args.strategy)
        from custom.analysis.answer import evaluation
        client = OpenAI(api_key=key or "local-no-key", base_url=os.environ.get("MODEL_URL"))

        def judge_call(conversation, model, temperature=0.0, max_tokens=4096):
            request = {"model": model, "messages": conversation,
                       "max_completion_tokens": min(max_tokens, args.max_output_tokens)}
            if not model.startswith("gpt-6"):
                request["temperature"] = temperature
            response = guard.dispatch(client.chat.completions.create, request, "other")
            content = response.choices[0].message.content
            return content.strip() if content else ""

        evaluation.chat_with_openai = judge_call
        artifact_root = args.output_jsonl.parent / f"{args.output_jsonl.stem}-artifacts" / args.server_version
        answer_dir, eval_dir = artifact_root / "ANSWER", artifact_root / "EVAL"
        answer_dir.mkdir(parents=True, exist_ok=True)
        eval_dir.mkdir(parents=True, exist_ok=True)

        def run_task(task_id):
            case = dict(cases[task_id])
            tools, routing = tools_for_case(case, raw_kb)
            action_step = 0

            def on_action(action):
                nonlocal action_step
                stream.emit({"event_type": "mcp_action", "task_id": task_id,
                             "step": action_step, "tool_id": action["tool_id"],
                             "arguments": action["arguments"],
                             "result_summary": action["result_summary"],
                             "is_error": action["is_error"]})
                action_step += 1

            agent = ChatModel(
                model_name=args.model, api_key=key or "local-no-key",
                model_url=os.environ.get("MODEL_URL"), tools=tools,
                max_round=max(10, 2 * len(tools)), tool_server_map=routing,
                file_exists=generate_tree_string(str(ANNOTATION_ROOT)),
                mcp_server_url=args.mcp_server_url,
                temperature=0.2 if args.temperature is None else args.temperature,
                top_p=args.top_p, max_new_tokens=args.max_output_tokens,
                request_dispatch=lambda create, request, phase: guard.dispatch(create, request, phase),
                on_tool_action=on_action,
            )
            answer, trajectory, stopped, exceeded = agent.run_with_server(case["question"])
            saved = dict(case)
            saved.update({"answer": answer, "trajectory_messages": trajectory,
                          "stop": stopped, "exceeded": exceeded,
                          "forced_finalization": agent.forced_finalization,
                          "involved_tools": tools, "harness_strategy": args.strategy,
                          "source_version": args.source_version,
                          "stage": args.server_version, "run_id": args.run_id,
                          "judge_version": args.judge_version})
            (answer_dir / f"{task_id}.json").write_text(
                json.dumps(saved, ensure_ascii=False, indent=2), encoding="utf-8")

            rng = random.Random(stable_task_seed(args.seed, task_id))
            scores = evaluation.evaluate_one_five_times(
                saved, model=args.model, temperature=0.0,
                max_tokens=args.max_output_tokens, num_runs=5, rng=rng)
            eval_payload = dict(saved)
            eval_payload["eval_result"] = scores
            (eval_dir / f"{task_id}.json").write_text(
                json.dumps(eval_payload, ensure_ascii=False, indent=2), encoding="utf-8")
            valid = [item["result"] for item in scores["runs"]
                     if valid_scores(item.get("result"))]
            if not valid:
                raise RuntimeError(f"judge returned no valid scores for task {task_id}")
            for metric in METRICS:
                event_metric = {"task_fulfillment": "tf", "grounding": "grounding",
                                "planning_and_efficiency": "pe"}[metric]
                stream.emit({"event_type": "metric_score", "metric": event_metric,
                             "task_id": task_id,
                             "value": sum(item[metric] for item in valid) / len(valid)})

        workers = max(1, args.workers)
        if workers == 1:
            for task_id in args.task_id:
                run_task(task_id)
        else:
            # The first failure stops queued tasks; running ones finish. Budget exhaustion wins
            # over other errors so Rust sees exit 2 whenever the budget actually ran out.
            stop = threading.Event()

            def guarded(task_id):
                if not stop.is_set():
                    run_task(task_id)

            failures = []
            pool = ThreadPoolExecutor(max_workers=workers)
            try:
                for future in as_completed([pool.submit(guarded, t) for t in args.task_id]):
                    if future.exception() is not None:
                        stop.set()
                        failures.append(future.exception())
            finally:
                pool.shutdown(wait=True, cancel_futures=True)
            if failures:
                raise next((f for f in failures if isinstance(f, BudgetExceededError)), failures[0])

    except BudgetExceededError as error:
        status, detail, exit_code = "budget_exhausted", str(error), 2
    except Exception as error:
        status, detail, exit_code = "failed", f"{type(error).__name__}: {error}", 1
        stream.emit({"event_type": "error", "message": detail, "fatal": True})
    finally:
        stream.emit({"event_type": "run_complete", "status": status, "detail": detail})
        stream.close()
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
