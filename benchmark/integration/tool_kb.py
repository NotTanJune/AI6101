"""Convert tool-kb/1 and frozen 1MCP snapshots. No server/model calls."""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
from pathlib import Path

from jsonschema import Draft202012Validator

ROOT = Path(__file__).resolve().parents[2]
SEPARATOR = "_1mcp_"


def validate_kb(kb: dict, *, uniform_versions: bool = True) -> None:
    """Validate the shared contract. Rust's selective patch keeps unaffected cards byte-identical
    to the source, so its KBs mix card versions; pass uniform_versions=False for those."""
    schema = json.loads((ROOT / "schemas/tool-kb.schema.json").read_text())
    Draft202012Validator(schema).validate(kb)
    routes, lineages = set(), set()
    for card in kb["cards"]:
        route = (card["server_id"], card["tool_id"])
        if route in routes or card["lineage_id"] in lineages:
            raise ValueError(f"Duplicate route or lineage: {route}")
        if SEPARATOR in card["server_id"] or SEPARATOR in card["tool_id"]:
            raise ValueError(f"Ambiguous 1MCP route: {route}")
        if uniform_versions and card["server_version"] != kb["server_version"]:
            raise ValueError(f"Card version differs from KB version: {route}")
        # The shared contract treats input_schema as an opaque object. MCP snapshots
        # contain mixed JSON Schema dialects; preserve them without rewriting keywords.
        routes.add(route)
        lineages.add(card["lineage_id"])


def to_snapshot(kb: dict) -> dict:
    validate_kb(kb)
    return {
        "format_version": 1,
        "captured_at_utc": None,  # This is a conversion, not a live capture.
        "mcp_server_url": None,
        "tools": [{"name": f"{card['server_id']}{SEPARATOR}{card['tool_id']}",
                   "description": card["description"], "inputSchema": copy.deepcopy(card["input_schema"])}
                  for card in kb["cards"]],
        "ai6101_tool_kb": copy.deepcopy(kb),
        "ai6101_metadata_only_fields": ["output_schema", "usage_rules", "provenance", "name", "lineage_id"],
    }


def from_snapshot(snapshot: dict, *, release_id: str, server_version: str, source_file: str) -> dict:
    if snapshot.get("format_version") != 1 or not isinstance(snapshot.get("tools"), list):
        raise ValueError("Expected a format_version=1 snapshot with tools")
    if "ai6101_tool_kb" in snapshot:
        kb = copy.deepcopy(snapshot["ai6101_tool_kb"])
        if kb["release_id"] != release_id or kb["server_version"] != server_version:
            raise ValueError("Requested release/version conflicts with embedded Tool KB metadata")
        if to_snapshot(kb)["tools"] != snapshot["tools"]:
            raise ValueError("Snapshot tools changed since export; embedded Tool KB metadata is stale")
        return kb
    cards = []
    for index, tool in enumerate(snapshot["tools"]):
        name = tool.get("name", "")
        if name.count(SEPARATOR) != 1:
            raise ValueError(f"Expected server{SEPARATOR}tool name: {name!r}")
        server_id, tool_id = name.split(SEPARATOR)
        if not isinstance(tool.get("description"), str) or not isinstance(tool.get("inputSchema"), dict):
            raise ValueError(f"Malformed description or inputSchema: {name}")
        cards.append({
            "lineage_id": "snapshot-route:" + hashlib.sha256(name.encode()).hexdigest(),
            "server_id": server_id, "tool_id": tool_id, "name": tool_id,
            "server_version": server_version, "description": tool["description"],
            "input_schema": copy.deepcopy(tool["inputSchema"]), "output_schema": None,
            "usage_rules": [], "provenance": {"release_id": release_id,
                "source_file": source_file, "source_pointer": f"/tools/{index}"},
        })
    digest = hashlib.sha256(json.dumps(snapshot, sort_keys=True).encode()).hexdigest()[:16]
    kb = {"schema_version": "tool-kb/1", "kb_id": f"snapshot-{digest}",
          "release_id": release_id, "server_version": server_version, "cards": cards}
    validate_kb(kb)
    return kb


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("direction", choices=["from-snapshot", "to-snapshot"])
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--release-id")
    parser.add_argument("--server-version")
    args = parser.parse_args()
    if args.input.resolve() == args.output.resolve():
        parser.error("Input and output must differ")
    payload = json.loads(args.input.read_text(encoding="utf-8"))
    if args.direction == "from-snapshot":
        if not args.release_id or not args.server_version:
            parser.error("from-snapshot requires --release-id and --server-version")
        output = from_snapshot(payload, release_id=args.release_id,
                               server_version=args.server_version, source_file=str(args.input))
    else:
        output = to_snapshot(payload)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Do not silently overwrite a curated KB or snapshot.
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(output, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
    count = len(output["cards"] if args.direction == "from-snapshot" else output["tools"])
    print(f"Converted and validated {count} tool cards: {args.output}")
    if args.direction == "to-snapshot":
        print("Legacy runners consume names, descriptions and input schemas; other KB fields remain metadata only.")


if __name__ == "__main__":
    main()
