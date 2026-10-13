"""Compare snapshot evidence with immutable Git objects."""

import hashlib
import re
import subprocess
from pathlib import Path


def git(root, *args, input_bytes=None):
    return subprocess.run(
        ["git", *args], cwd=root, input=input_bytes, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ).stdout


def verify_commit_snapshot(root, commit, version, files, actual):
    root = Path(root).resolve()
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("release evidence requires the requested full commit SHA")
    resolved = git(root, "rev-parse", "--verify", f"{commit}^{{commit}}").decode().strip()
    if resolved != commit:
        raise ValueError("release evidence requires the requested full commit SHA")
    records = git(root, "ls-tree", "-r", "-z", "--full-tree", resolved).split(b"\0")
    expected = {}
    cargo = None
    for record in records:
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, kind, oid = metadata.decode("ascii").split()
        path = raw_path.decode("utf-8", "strict")
        if any(part in {".deps", ".git", ".slim", "node_modules", "target"} for part in Path(path).parts):
            continue
        if kind != "blob" or mode not in {"100644", "100755"}:
            raise ValueError(f"unsupported source object: {path!r}")
        content = git(root, "cat-file", "blob", oid)
        expected[path] = {
            "path": path, "mode": mode, "size": len(content),
            "sha256": hashlib.sha256(content).hexdigest(),
        }
        if path == "Cargo.toml":
            cargo = content
    if actual != expected or files != list(expected.values()):
        raise ValueError("source snapshot does not exactly match requested Git commit")
    match = re.search(rb"(?ms)^\[package\]\s+.*?^version\s*=\s*\"([^\"]+)\"", cargo or b"")
    if match is None or match.group(1).decode("ascii") != version:
        raise ValueError("source snapshot version does not match Cargo.toml at requested commit")
