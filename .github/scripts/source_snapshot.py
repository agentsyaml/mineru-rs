#!/usr/bin/env python3
"""Build and verify immutable tracked-source evidence without generated trees."""

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path, PurePosixPath
from typing import NoReturn

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tools" / "deps"))
from archive_safe import extract_tar_gz
from source_snapshot_git import verify_commit_snapshot

ROOT_NAME = "source"
SCHEMA = "mineru-source-snapshot-v1"
EVIDENCE_SCHEMA = "mineru-source-build-evidence-v1"
EXCLUDED = {".deps", ".git", ".slim", "node_modules", "target"}
TEST_COMMAND = "cargo +1.89.0 test --locked -p mineru --all-targets --features office,legacy-office -- --test-threads=4"
BUILD_COMMAND = "cargo +1.89.0 build --locked --release -p mineru --features office,legacy-office --bins"


def fail(message: str) -> NoReturn:
    raise ValueError(message)


def git(root, *args, input_bytes=None):
    return subprocess.run(
        ["git", *args], cwd=root, input=input_bytes, check=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    ).stdout


def safe_path(name):
    path = PurePosixPath(name)
    if path.is_absolute() or not path.parts or ".." in path.parts or "\\" in name:
        fail(f"unsafe source snapshot path: {name!r}")
    if any(part in EXCLUDED for part in path.parts):
        fail(f"generated path in source snapshot: {name!r}")
    return path


def selected(path):
    return not any(part in EXCLUDED for part in PurePosixPath(path).parts)


def commit_tree(root, commit):
    resolved = git(root, "rev-parse", "--verify", f"{commit}^{{commit}}").decode().strip()
    if resolved != commit.lower():
        fail("requested source snapshot commit did not resolve exactly")
    output = git(root, "ls-tree", "-r", "-z", "--full-tree", resolved)
    for record in output.split(b"\0"):
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, kind, oid = metadata.decode("ascii").split()
        path = raw_path.decode("utf-8", "strict")
        if not selected(path):
            continue
        safe_path(path)
        if kind != "blob" or mode not in {"100644", "100755"}:
            fail(f"unsupported tracked source entry: {path!r} ({mode} {kind})")
        yield path, int(mode, 8) & 0o777, oid


def working_tree(root, include_untracked_prefixes=()):
    tracked = set()
    for record in git(root, "ls-files", "-s", "-z").split(b"\0"):
        if not record:
            continue
        metadata, raw_path = record.split(b"\t", 1)
        mode, _oid, stage = metadata.decode("ascii").split()
        path = raw_path.decode("utf-8", "strict")
        if stage != "0":
            fail(f"unmerged index entry in source snapshot: {path!r}")
        if not selected(path):
            continue
        safe_path(path)
        tracked.add(path)
        source = root / path
        info = source.lstat()
        if not source.is_file() or source.is_symlink():
            fail(f"tracked snapshot input is not a regular file: {path!r}")
        yield path, 0o755 if mode == "100755" and info.st_mode & 0o111 else 0o644, source
    for prefix in include_untracked_prefixes:
        prefix_path = safe_path(prefix)
        if not selected(prefix):
            fail(f"generated untracked prefix is forbidden: {prefix!r}")
        for raw_path in git(root, "ls-files", "--others", "--exclude-standard", "-z", "--", prefix).split(b"\0"):
            if not raw_path:
                continue
            path = raw_path.decode("utf-8", "strict")
            if path in tracked:
                continue
            if not selected(path):
                continue
            relative = safe_path(path)
            if relative.parts[:len(prefix_path.parts)] != prefix_path.parts:
                fail(f"untracked source escaped its allowlisted prefix: {path!r}")
            source = root / path
            info = source.lstat()
            if not source.is_file() or source.is_symlink():
                fail(f"untracked snapshot input is not a regular file: {path!r}")
            yield path, 0o755 if info.st_mode & 0o111 else 0o644, source


def object_stream(root, entries):
    process = subprocess.Popen(
        ["git", "cat-file", "--batch"], cwd=root,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    assert process.stdin is not None and process.stdout is not None
    try:
        for path, mode, oid in entries:
            process.stdin.write(oid.encode("ascii") + b"\n")
            process.stdin.flush()
            header = process.stdout.readline().split()
            if len(header) != 3 or header[1] != b"blob":
                fail(f"git object is not a blob for {path!r}")
            size = int(header[2])
            content = process.stdout.read(size)
            if len(content) != size or process.stdout.read(1) != b"\n":
                fail(f"truncated git object for {path!r}")
            yield path, mode, content
        process.stdin.close()
        if process.wait() != 0:
            fail("git cat-file failed while creating source snapshot")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                stream.close()


def snapshot_create(root, mode, commit, version, directory, include_untracked_prefixes=()):
    root = Path(root).resolve()
    head = git(root, "rev-parse", "HEAD").decode().strip()
    commit = head if mode == "working-tree" else commit.lower()
    if not re.fullmatch(r"[0-9a-f]{40}", commit) or commit != head:
        fail("source snapshot must identify the checked-out full commit SHA")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        fail("source snapshot version must be X.Y.Z")
    if mode == "commit" and include_untracked_prefixes:
        fail("commit snapshots cannot include untracked paths")
    entries = commit_tree(root, commit) if mode == "commit" else working_tree(root, include_untracked_prefixes)
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=False)
    archive_path = directory / "source-snapshot.tar.gz"
    manifest_path = directory / "source-snapshot.json"
    manifest_entries = []
    try:
        with archive_path.open("xb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
                with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                    stream = object_stream(root, entries) if mode == "commit" else (
                        (name, permissions, (root / name).read_bytes())
                        for name, permissions, _source in entries
                    )
                    for name, permissions, content in stream:
                        member = tarfile.TarInfo(f"{ROOT_NAME}/{name}")
                        member.mode, member.size, member.mtime = permissions, len(content), 0
                        archive.addfile(member, io.BytesIO(content))
                        manifest_entries.append({
                            "path": name, "mode": "100755" if permissions & 0o111 else "100644",
                            "size": len(content),
                            "sha256": hashlib.sha256(content).hexdigest(),
                        })
        manifest = {
            "schema": SCHEMA, "commit": commit, "version": version,
            "source_mode": mode, "archive": archive_path.name,
            "archive_sha256": hash_file(archive_path),
            "files": manifest_entries,
        }
        manifest_path.write_text(json.dumps(manifest, sort_keys=True, indent=2) + "\n")
    except Exception:
        shutil.rmtree(directory, ignore_errors=True)
        raise
    return manifest


def record_evidence(directory, commit, version):
    directory = Path(directory)
    manifest = snapshot_verify(directory, commit, version)
    evidence = {
        "schema": EVIDENCE_SCHEMA, "commit": commit.lower(), "version": version,
        "archive_sha256": manifest["archive_sha256"], "result": "passed",
        "commands": [
            "python tools/deps/prepare_office.py prepare",
            "python tools/deps/prepare_office.py verify",
            "python -m unittest discover -s tools/deps -p 'test_*.py'",
            TEST_COMMAND, BUILD_COMMAND,
        ],
    }
    (directory / "source-build-evidence.json").write_text(json.dumps(evidence, sort_keys=True, indent=2) + "\n")
    return evidence


def verify_evidence(directory, commit, version, source_root=Path.cwd()):
    directory = Path(directory)
    names = {"source-snapshot.tar.gz", "source-snapshot.json", "source-build-evidence.json"}
    entries = list(directory.iterdir())
    if {path.name for path in entries} != names or any(path.is_symlink() or not path.is_file() for path in entries):
        fail("source build artifact contains missing or unexpected files")
    manifest = snapshot_verify(directory, commit, version, source_root=source_root)
    try:
        evidence = json.loads((directory / "source-build-evidence.json").read_text())
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read source build evidence: {error}")
    expected = {
        "schema": EVIDENCE_SCHEMA, "commit": commit.lower(), "version": version,
        "archive_sha256": manifest["archive_sha256"], "result": "passed",
        "commands": [
            "python tools/deps/prepare_office.py prepare",
            "python tools/deps/prepare_office.py verify",
            "python -m unittest discover -s tools/deps -p 'test_*.py'",
            TEST_COMMAND, BUILD_COMMAND,
        ],
    }
    if evidence != expected:
        fail("source build evidence does not match the required commit/build checks")
    return evidence


def self_test():
    with tempfile.TemporaryDirectory() as temporary:
        root = Path.cwd()
        commit = git(root, "rev-parse", "HEAD").decode().strip()
        output = Path(temporary) / "commit-snapshot"
        snapshot_create(root, "commit", commit, "0.3.1", output)
        manifest = snapshot_verify(output, commit, "0.3.1", Path(temporary) / "commit-extracted")
        paths = {item["path"] for item in manifest["files"]}
        if not {"Cargo.toml", "Cargo.lock"}.issubset(paths) or any(
            not selected(path) for path in paths
        ):
            fail("self-test snapshot tracked paths or generated-path exclusions differ")
        expected_local_paths = {path for path, _mode, _source in working_tree(root)}
        local = Path(temporary) / "working-tree-snapshot"
        snapshot_create(root, "working-tree", commit, "0.3.1", local)
        snapshot_verify(local, commit, "0.3.1", Path(temporary) / "working-tree-extracted", require_commit=False)
        local_paths = {item["path"] for item in json.loads((local / "source-snapshot.json").read_text())["files"]}
        if local_paths != expected_local_paths:
            fail("self-test working-tree snapshot included untracked files")
        corrupt = json.loads((output / "source-snapshot.json").read_text())
        corrupt["archive_sha256"] = "0" * 64
        (output / "source-snapshot.json").write_text(json.dumps(corrupt))
        try:
            snapshot_verify(output, commit, "0.3.1")
        except ValueError:
            return
        raise AssertionError("self-test accepted a tampered source snapshot")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("self-test")
    create = commands.add_parser("create")
    create.add_argument("--source", type=Path, default=Path.cwd())
    create.add_argument("--mode", choices=("commit", "working-tree"), required=True)
    create.add_argument("--commit", required=True)
    create.add_argument("--version", required=True)
    create.add_argument("--directory", type=Path, required=True)
    create.add_argument(
        "--include-untracked-prefix", action="append", default=[],
        help="working-tree mode only; include non-ignored regular files below this explicit path",
    )
    verify = commands.add_parser("verify")
    verify.add_argument("--directory", type=Path, required=True)
    verify.add_argument("--commit", required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--extract", type=Path)
    verify.add_argument("--allow-working-tree", action="store_true")
    for name in ("record", "verify-evidence"):
        command = commands.add_parser(name)
        command.add_argument("--directory", type=Path, required=True)
        command.add_argument("--commit", required=True)
        command.add_argument("--version", required=True)
        if name == "verify-evidence":
            command.add_argument("--root", type=Path, default=Path.cwd())
    args = parser.parse_args()
    if args.command == "self-test":
        self_test()
    elif args.command == "create":
        snapshot_create(
            args.source, args.mode, args.commit, args.version, args.directory,
            args.include_untracked_prefix,
        )
    elif args.command == "verify":
        snapshot_verify(args.directory, args.commit, args.version, args.extract, not args.allow_working_tree)
    elif args.command == "record":
        record_evidence(args.directory, args.commit, args.version)
    else:
        verify_evidence(args.directory, args.commit, args.version, args.root)


def hash_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_manifest(directory, commit, version):
    directory = Path(directory)
    try:
        manifest = json.loads((directory / "source-snapshot.json").read_text())
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot read source snapshot manifest: {error}")
    keys = {"schema", "commit", "version", "source_mode", "archive", "archive_sha256", "files"}
    if not isinstance(manifest, dict) or set(manifest) != keys or manifest.get("schema") != SCHEMA:
        fail("malformed source snapshot manifest")
    if manifest.get("commit") != commit.lower() or manifest.get("version") != version:
        fail("source snapshot identity mismatch")
    if not isinstance(manifest.get("source_mode"), str) or not isinstance(manifest.get("archive"), str):
        fail("malformed source snapshot mode or archive name")
    if (
        manifest.get("source_mode") not in {"commit", "working-tree"}
        or manifest.get("archive") != "source-snapshot.tar.gz"
    ):
        fail("unsupported source snapshot mode or archive name")
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        fail("source snapshot has no tracked files")
    seen = set()
    for item in files:
        if not isinstance(item, dict) or set(item) != {"path", "mode", "size", "sha256"}:
            fail("malformed source snapshot file manifest")
        path = item["path"]
        if not isinstance(path, str):
            fail("malformed source snapshot path")
        safe_path(path)
        if (
            path in seen
            or not isinstance(item["mode"], str)
            or item["mode"] not in {"100644", "100755"}
            or type(item["size"]) is not int
            or item["size"] < 0
        ):
            fail("duplicate or invalid source snapshot file entry")
        if not isinstance(item["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", item["sha256"]):
            fail("malformed source snapshot file digest")
        seen.add(path)
    if not isinstance(manifest.get("archive_sha256"), str) or not re.fullmatch(
        r"[0-9a-f]{64}", manifest["archive_sha256"]
    ):
        fail("malformed source snapshot archive digest")
    archive = directory / manifest["archive"]
    if archive.is_symlink() or not archive.is_file() or hash_file(archive) != manifest["archive_sha256"]:
        fail("source snapshot archive digest mismatch")
    return manifest, archive


def snapshot_verify(directory, commit, version, extract_to=None, require_commit=True, source_root=None):
    manifest, archive = read_manifest(directory, commit, version)
    if require_commit and manifest["source_mode"] != "commit":
        fail("release evidence must use an immutable commit snapshot")
    if extract_to is None:
        temporary = tempfile.TemporaryDirectory()
        stage = Path(temporary.name) / "source"
        destination = stage
        publish = False
    else:
        # Stage on the destination's own filesystem: the exclusive atomic
        # rename below cannot cross filesystems (EXDEV).
        destination = Path(extract_to)
        if not destination.parent.is_dir():
            fail("source snapshot extraction parent must already exist")
        temporary = tempfile.TemporaryDirectory(prefix=".source-snapshot-stage-", dir=destination.parent)
        stage = Path(temporary.name) / "source"
        publish = True
    try:
        executable = extract_tar_gz(archive, stage, ROOT_NAME)
        actual = {}
        for path in stage.rglob("*"):
            if path.is_symlink():
                fail("source snapshot extraction contains a symlink")
            if path.is_file():
                relative = path.relative_to(stage).as_posix()
                safe_path(relative)
                actual[relative] = {
                    "path": relative,
                    "mode": "100755" if relative in executable else "100644",
                    "size": path.stat().st_size,
                    "sha256": hash_file(path),
                }
        if actual != {item["path"]: item for item in manifest["files"]}:
            fail("source snapshot file list or content digest mismatch")
        if source_root is not None:
            verify_commit_snapshot(source_root, commit, version, manifest["files"], actual)
        if publish:
            publish_directory(stage, destination)
    finally:
        shutil.rmtree(temporary.name, ignore_errors=True)
    return manifest


def publish_directory(stage, destination):
    destination = Path(destination)
    if not destination.parent.is_dir():
        fail("source snapshot extraction parent must already exist")
    if sys.platform == "darwin":
        import ctypes
        result = ctypes.CDLL(None, use_errno=True).renamex_np(
            os.fsencode(stage), os.fsencode(destination), 0x00000004
        )
        if result:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), destination)
    elif sys.platform.startswith("linux"):
        import ctypes
        libc = ctypes.CDLL(None, use_errno=True)
        renameat2 = libc.renameat2
        result = renameat2(-100, os.fsencode(stage), -100, os.fsencode(destination), 1)
        if result:
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), destination)
    else:
        fail("exclusive atomic extraction publication is unsupported on this platform")


if __name__ == "__main__":
    try:
        main()
    except (OSError, subprocess.CalledProcessError, tarfile.TarError, ValueError) as error:
        raise SystemExit(f"source snapshot failed: {error}") from error
