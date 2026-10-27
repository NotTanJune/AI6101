"""Prepare a live Rust/Python run against real 1MCP server versions.

Subcommands:
  mcp-config  Write a 1MCP config limited to the pilot's servers, with machine paths filled in.
  capture     Save the tool listing a running 1MCP endpoint exposes (format_version 1 snapshot).
  fixture     Build a Rust fixture-adapter root (kb/<label>.json + tasks.json) from live Tool KBs.
"""
from __future__ import annotations

import argparse
import asyncio
from datetime import datetime, timezone
import json
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path[:0] = [str(ROOT), str(ROOT / "src")]

from integration.tool_kb import validate_kb  # noqa: E402

PILOT = ROOT / "src/custom/data/test_cases/pilot_20_clean.json"


def pilot_cases(path: Path) -> list[dict]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    return payload.get("samples", payload.get("data", [])) if isinstance(payload, dict) else payload


def matches(config_key: str, task_server: str) -> bool:
    return (config_key == task_server or config_key == f"@{task_server}"
            or config_key.endswith(f"/{task_server}"))


def mcp_config(args) -> None:
    source = ROOT / "servers/1mcp_config" / args.version / "mcp.json"
    servers = json.loads(source.read_text(encoding="utf-8"))["mcpServers"]
    wanted = {name for case in pilot_cases(args.tasks) for name in case["servers"]}
    selected, missing = {}, []
    for name in sorted(wanted):
        keys = [key for key in servers if matches(key, name)]
        if not keys:
            missing.append(name)
        for key in keys:
            selected[key] = servers[key]
    for override in args.node_override:
        name, _, script = override.partition("=")
        if name not in selected or not Path(script).is_file():
            raise SystemExit(f"--node-override {override!r}: unknown server or missing script")
        selected[name] = {"command": "node", "args": [str(Path(script).resolve())]}
    text = json.dumps({"mcpServers": selected}, ensure_ascii=False, indent=2)
    replacements = {"/path/to/workspace/": f"{ROOT}/"}
    if args.servers_dir:
        replacements[f"/path/to/{args.version}-servers"] = str(args.servers_dir.resolve())
    for old, new in replacements.items():
        text = text.replace(old, new)
    if "/path/to/" in text:
        raise SystemExit(f"unresolved /path/to/ placeholder remains in {args.version} config")
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(text + "\n", encoding="utf-8")
    print(f"{args.version}: {len(selected)} servers -> {args.out}; absent from config: {missing}")


async def list_tools(url: str):
    import mcp
    from mcp.client.streamable_http import streamablehttp_client
    async with streamablehttp_client(url) as (read_stream, write_stream, _):
        async with mcp.ClientSession(read_stream, write_stream) as session:
            await session.initialize()
            tools, cursor = [], None
            while True:
                page = await session.list_tools(cursor=cursor)
                tools.extend(page.tools)
                cursor = page.nextCursor
                if not cursor:
                    return tools


def capture(args) -> None:
    tools = asyncio.run(list_tools(args.url))
    snapshot = {"format_version": 1, "captured_at_utc": datetime.now(timezone.utc).isoformat(),
                "mcp_server_url": args.url,
                "tools": [{"name": tool.name, "description": tool.description or "",
                           "inputSchema": tool.inputSchema} for tool in tools]}
    args.out.parent.mkdir(parents=True, exist_ok=True)
    with args.out.open("x", encoding="utf-8") as stream:
        json.dump(snapshot, stream, ensure_ascii=False, indent=2)
    servers = sorted({tool.name.split("_1mcp_")[0] for tool in tools})
    print(f"captured {len(tools)} tools from {len(servers)} servers -> {args.out}")


def fixture(args) -> None:
    kbs = {}
    for label, path in (("early", args.early), ("middle", args.middle), ("late", args.late)):
        kb = json.loads(path.read_text(encoding="utf-8"))
        validate_kb(kb)
        if kb["server_version"] != label:
            raise SystemExit(f"{path}: server_version must be {label!r}")
        kbs[label] = kb
    live_servers = {card["server_id"] for kb in kbs.values() for card in kb["cards"]}
    tasks = []
    for case in pilot_cases(args.tasks):
        server_ids = []
        for name in case["servers"]:
            found = sorted(s for s in live_servers if matches(s, name))
            server_ids.extend(found)
        if not server_ids:
            raise SystemExit(f"task {case['idx']}: none of its servers appear in the live KBs")
        tasks.append({"task_id": str(case["idx"]), "server_id": server_ids[0],
                      "server_ids": server_ids, "prompt": case["question"],
                      "stage": "late", "change_classes": []})
    (args.out / "kb").mkdir(parents=True, exist_ok=False)
    for label, kb in kbs.items():
        (args.out / "kb" / f"{label}.json").write_text(
            json.dumps(kb, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    (args.out / "tasks.json").write_text(json.dumps(tasks, ensure_ascii=False, indent=2) + "\n",
                                         encoding="utf-8")
    print(f"fixture root {args.out}: {len(tasks)} tasks, cards "
          + ", ".join(f"{label}={len(kb['cards'])}" for label, kb in kbs.items()))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("mcp-config")
    p.add_argument("--version", required=True, choices=["original", "iter3", "iter5"])
    p.add_argument("--servers-dir", type=Path)
    p.add_argument("--node-override", action="append", default=[], metavar="SERVER=SCRIPT",
                   help="run SERVER with `node SCRIPT` instead of its configured command")
    p.add_argument("--tasks", type=Path, default=PILOT)
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=mcp_config)
    p = sub.add_parser("capture")
    p.add_argument("--url", default="http://127.0.0.1:3050/mcp")
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=capture)
    p = sub.add_parser("fixture")
    for label in ("early", "middle", "late"):
        p.add_argument(f"--{label}", type=Path, required=True)
    p.add_argument("--tasks", type=Path, default=PILOT)
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=fixture)
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
