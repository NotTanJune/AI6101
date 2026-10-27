"""Check every imported file, including all historical result files, by SHA-256."""
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main():
    manifest = json.loads((ROOT / "UPSTREAM_IMPORT.json").read_text())
    changes = json.loads((ROOT / "LOCAL_CHANGES.json").read_text())["modified_imported_files"]
    changes_by_path = {item["path"]: item for item in changes}
    paths = {item["path"] for item in manifest["imported_files"]}
    if len(changes_by_path) != len(changes) or not set(changes_by_path) <= paths:
        raise ValueError("Local change manifest contains duplicate or unknown imported paths")
    groups = {}
    changed_by_teammate = 0
    failures = []
    for item in manifest["imported_files"]:
        path = item["path"]
        expected = changes_by_path.get(path, item)
        digest_key = "integrated_sha256" if path in changes_by_path else "imported_sha256"
        bytes_key = "integrated_bytes" if path in changes_by_path else "imported_bytes"
        try:
            data = (ROOT / path).read_bytes()
        except OSError as error:
            failures.append(f"{path}: {error}")
            continue
        if len(data) != expected[bytes_key] or hashlib.sha256(data).hexdigest() != expected[digest_key]:
            failures.append(f"{path}: hash or size mismatch")
        group = groups.setdefault(path.split("/")[0], {"files": 0, "source_bytes": 0, "imported_bytes": 0, "integrated_bytes": 0})
        group["files"] += 1
        group["source_bytes"] += item["source_bytes"]
        group["imported_bytes"] += item["imported_bytes"]
        group["integrated_bytes"] += len(data)
        changed_by_teammate += item["changed_by_teammate"]
    if failures:
        raise SystemExit("\n".join(failures))
    print(json.dumps({"status": "pass", "imported_files": len(paths),
                      "teammate_changed_files": changed_by_teammate,
                      "documented_local_edits": len(changes), "groups": groups}, indent=2))


if __name__ == "__main__":
    main()
