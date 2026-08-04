#!/usr/bin/env python3
"""Seal and attach the exact binary-only GitHub Release asset set."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import tempfile
import typing
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

API = "https://api.github.com"
API_VERSION = "2022-11-28"
USER_AGENT = "mineru-rs-release-attacher/1"
SUMS_NAME = "SHA256SUMS"
TARGETS = (
    ("x86_64-unknown-linux-gnu", ".tar.gz"),
    ("aarch64-unknown-linux-gnu", ".tar.gz"),
    ("x86_64-apple-darwin", ".tar.gz"),
    ("aarch64-apple-darwin", ".tar.gz"),
    ("x86_64-pc-windows-msvc", ".zip"),
    ("aarch64-pc-windows-msvc", ".zip"),
)
VERSION_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
COMMIT_RE = re.compile(r"[0-9a-f]{40}\Z")


def fail(message: str) -> typing.NoReturn:
    raise SystemExit(f"release attachment failed: {message}")


def approved_archive_names(version: str) -> set[str]:
    if not VERSION_RE.fullmatch(version):
        fail(f"invalid version {version!r}")
    return {f"mineru-v{version}-{target}{suffix}" for target, suffix in TARGETS}


def approved_names(version: str) -> set[str]:
    return approved_archive_names(version) | {SUMS_NAME}


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def asset_digest(name: str, value: object) -> str:
    if not isinstance(value, str) or not value.startswith("sha256:") or not SHA256_RE.fullmatch(value[7:]):
        fail(f"asset {name!r} has invalid digest; expected sha256:<64 lowercase hexadecimal characters>")
    return value[7:]


def plain_files(directory: Path) -> list[Path]:
    """Return top-level regular files, rejecting links and all other entries."""
    if directory.is_symlink() or not directory.is_dir():
        fail(f"{directory} must be a real directory")
    entries = sorted(directory.iterdir())
    for entry in entries:
        if entry.is_symlink() or not entry.is_file():
            fail(f"unexpected entry in {directory}: {entry.name}")
    return entries


def require_exact_files(directory: Path, expected: set[str]) -> dict[str, Path]:
    entries = plain_files(directory)
    actual = {entry.name for entry in entries}
    if actual != expected:
        fail(f"{directory} has wrong asset set: missing={sorted(expected - actual)!r}, extra={sorted(actual - expected)!r}")
    return {entry.name: entry for entry in entries}


def sums_text(hashes: dict[str, str]) -> str:
    return "".join(f"{hashes[name]} *{name}\n" for name in sorted(hashes))


def parse_sums(payload: bytes, names: set[str]) -> dict[str, str]:
    try:
        text = payload.decode("ascii")
    except UnicodeDecodeError:
        fail(f"{SUMS_NAME} is not ASCII")
    records: dict[str, str] = {}
    for line in text.splitlines(keepends=True):
        if not line.endswith("\n") or not re.fullmatch(r"[0-9a-f]{64} \*[^/\\\r\n]+\n", line):
            fail(f"{SUMS_NAME} has malformed record")
        digest, name = line[:-1].split(" *", 1)
        if name in records:
            fail(f"{SUMS_NAME} lists {name!r} more than once")
        records[name] = digest
    if text != sums_text(records) or set(records) != names:
        fail(f"{SUMS_NAME} does not exactly cover approved archives in sorted order")
    return records


def sealed_hashes(directory: Path, version: str) -> dict[str, str]:
    archives = approved_archive_names(version)
    files = require_exact_files(directory, approved_names(version))
    records = parse_sums(files[SUMS_NAME].read_bytes(), archives)
    for name in archives:
        actual = sha256_file(files[name])
        if records[name] != actual:
            fail(f"{SUMS_NAME} digest mismatch for {name}")
    return {name: sha256_file(path) for name, path in files.items()}


def require_release(release: object, tag: str, release_id: int, draft: bool, commit: str) -> None:
    if not COMMIT_RE.fullmatch(commit):
        fail(f"invalid commit {commit!r}; expected 40 lowercase hexadecimal characters")
    if not isinstance(release, dict):
        fail("malformed release payload")
    mapping = typing.cast(dict[str, object], release)
    if mapping.get("id") != release_id or mapping.get("tag_name") != tag:
        fail(f"release identity mismatch for id={release_id}, tag={tag!r}")
    if mapping.get("draft") is not draft or mapping.get("prerelease") is not False:
        fail(f"release {release_id} must have draft={draft!r} and prerelease=false")
    if mapping.get("target_commitish") != commit:
        fail(f"release {release_id} target_commitish does not match requested commit")


def plan_uploads(local: dict[str, str], remote: dict[str, str]) -> list[str]:
    extras = set(remote) - set(local)
    if extras:
        fail(f"remote release has unapproved assets: {sorted(extras)!r}")
    for name in sorted(set(local) & set(remote)):
        if local[name] != remote[name]:
            fail(f"{name} already attached with a different digest; refusing to replace it")
    return sorted(set(local) - set(remote))


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, "HTTP redirect rejected", headers, fp)


def request(url: str, token: str, method: str = "GET", body: bytes | None = None,
            content_type: str | None = None, accept: str = "application/vnd.github+json") -> bytes:
    headers = {"Accept": accept, "Authorization": f"Bearer {token}", "X-GitHub-Api-Version": API_VERSION,
               "User-Agent": USER_AGENT}
    if content_type:
        headers["Content-Type"] = content_type
    call = urllib.request.Request(url, data=body, headers=headers, method=method)
    try:
        with urllib.request.build_opener(RejectRedirects()).open(call, timeout=120) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace")[:400]
        fail(f"{method} {url} returned HTTP {error.code}: {detail}")
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        fail(f"{method} {url} failed: {error}")


def load(payload: bytes, url: str) -> object:
    try:
        return json.loads(payload.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        fail(f"malformed JSON from {url}: {error}")


def _fetch_asset_entries(repository: str, release_id: int, token: str) -> dict[str, dict[str, object]]:
    assets: dict[str, dict[str, object]] = {}
    for page in range(1, 101):
        url = f"{API}/repos/{repository}/releases/{release_id}/assets?per_page=100&page={page}"
        raw = load(request(url, token), url)
        if not isinstance(raw, list):
            fail(f"malformed asset list from {url}")
        for entry in typing.cast(list[object], raw):
            if not isinstance(entry, dict):
                fail(f"malformed asset entry from {url}")
            item = typing.cast(dict[str, object], entry)
            if not isinstance(item.get("name"), str) or not isinstance(item.get("state"), str):
                fail(f"malformed asset entry from {url}")
            name = typing.cast(str, item["name"])
            if name in assets:
                fail(f"release {release_id} lists {name} more than once")
            asset_digest(name, item.get("digest"))
            assets[name] = item
        if len(raw) < 100:
            return assets
    fail("asset pagination exceeded limit")


def _fetch_assets(repository: str, release_id: int, token: str) -> dict[str, tuple[str, str]]:
    return {name: (typing.cast(str, item["state"]), asset_digest(name, item.get("digest")))
            for name, item in _fetch_asset_entries(repository, release_id, token).items()}


def verify_remote_assets(expected: dict[str, str], remote: dict[str, tuple[str, str]]) -> None:
    if set(expected) != set(remote):
        fail(f"remote asset set mismatch: missing={sorted(set(expected) - set(remote))!r}, extra={sorted(set(remote) - set(expected))!r}")
    for name, expected_digest in sorted(expected.items()):
        state, remote_digest = remote[name]
        if state != "uploaded":
            fail(f"asset {name!r} has state {state!r}, expected 'uploaded'")
        if remote_digest != expected_digest:
            fail(f"asset {name!r} digest mismatch: expected {expected_digest}, got {remote_digest}")


def stage(args: argparse.Namespace) -> None:
    destination = args.destination
    if any(source.is_symlink() for source in args.source):
        fail("payload source must not be a symlink")
    sources = [source.resolve() for source in args.source]
    resolved = destination.resolve()
    for source in sources:
        if resolved == source or source in resolved.parents or resolved in source.parents:
            fail(f"destination {destination} must not overlap source {source}")
    if destination.exists():
        fail(f"{destination} already exists")
    staged: dict[str, Path] = {}
    for source in sources:
        for path in plain_files(source):
            if path.name == SUMS_NAME:
                fail(f"{SUMS_NAME} must not come from a payload directory")
            if path.name in staged:
                fail(f"duplicate asset name across sources: {path.name}")
            staged[path.name] = path
    expected = approved_archive_names(args.version)
    if set(staged) != expected:
        fail(f"staged assets are not the approved archive set: missing={sorted(expected-set(staged))!r}, extra={sorted(set(staged)-expected)!r}")
    destination.mkdir(parents=True)
    for name, path in staged.items():
        shutil.copyfile(path, destination / name)
    print("\n".join(sorted(staged)))


def seal(args: argparse.Namespace) -> None:
    archives = approved_archive_names(args.version)
    if (args.directory / SUMS_NAME).exists():
        fail(f"{args.directory / SUMS_NAME} already exists")
    files = require_exact_files(args.directory, archives)
    hashes = {name: sha256_file(path) for name, path in files.items()}
    (args.directory / SUMS_NAME).write_text(sums_text(hashes), encoding="ascii", newline="\n")
    sealed_hashes(args.directory, args.version)
    print(SUMS_NAME)


def token_from(args: argparse.Namespace) -> str:
    token = os.environ.get(args.token_env, "")
    if not token:
        fail(f"{args.token_env} is unset or empty")
    return token


def attach(args: argparse.Namespace) -> None:
    token, hashes = token_from(args), sealed_hashes(args.directory, args.version)
    url = f"{API}/repos/{args.repository}/releases/{args.release_id}"
    require_release(load(request(url, token), url), args.tag, args.release_id, True, args.commit)
    pending = plan_uploads(hashes, {name: digest for name, (_, digest) in _fetch_assets(args.repository, args.release_id, token).items()})
    for name in pending:
        upload = f"https://uploads.github.com/repos/{args.repository}/releases/{args.release_id}/assets?name={urllib.parse.quote(name, safe='')}"
        request(upload, token, method="POST", body=(args.directory / name).read_bytes(), content_type="application/octet-stream")
        print(f"attached {name}")
    verify_remote_assets(hashes, _fetch_assets(args.repository, args.release_id, token))


def safe_download_url(url: str) -> bool:
    parsed = urllib.parse.urlparse(url)
    return parsed.scheme == "https" and bool(parsed.hostname) and (parsed.hostname == "github.com" or parsed.hostname.endswith(".githubusercontent.com"))


def download_asset(repository: str, asset: dict[str, object], token: str) -> bytes:
    asset_id = asset.get("id")
    if not isinstance(asset_id, int):
        fail("remote asset has invalid id")
    url = f"{API}/repos/{repository}/releases/assets/{asset_id}"
    headers = {"Accept": "application/octet-stream", "Authorization": f"Bearer {token}", "User-Agent": USER_AGENT,
               "X-GitHub-Api-Version": API_VERSION}
    try:
        with urllib.request.build_opener(RejectRedirects()).open(urllib.request.Request(url, headers=headers), timeout=120) as response:
            return response.read()
    except urllib.error.HTTPError as error:
        if error.code not in (301, 302, 303, 307, 308):
            fail(f"asset download returned HTTP {error.code}")
        location = error.headers.get("Location")
        if not isinstance(location, str) or not safe_download_url(location):
            fail("asset download redirect is not an approved HTTPS GitHub host")
        # Never send the bearer token to the redirected origin.
        try:
            with urllib.request.build_opener(RejectRedirects()).open(
                urllib.request.Request(location, headers={"User-Agent": USER_AGENT}), timeout=120
            ) as response:
                return response.read()
        except (urllib.error.HTTPError, urllib.error.URLError, TimeoutError, OSError) as nested:
            fail(f"redirected asset download failed: {nested}")
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        fail(f"asset download failed: {error}")


def peel_tag_payloads(ref: object, tag_objects: dict[str, object], commit: str) -> None:
    if not COMMIT_RE.fullmatch(commit) or not isinstance(ref, dict):
        fail("malformed tag reference")
    mapping = typing.cast(dict[str, object], ref)
    if not isinstance(mapping.get("object"), dict):
        fail("malformed tag reference")
    target = typing.cast(dict[str, object], mapping["object"])
    seen: set[str] = set()
    for _ in range(16):
        kind, sha = target.get("type"), target.get("sha")
        if not isinstance(kind, str) or not isinstance(sha, str) or not COMMIT_RE.fullmatch(sha):
            fail("malformed tag object")
        if kind == "commit":
            if sha != commit:
                fail(f"tag resolves to {sha}, expected {commit}")
            return
        if kind != "tag" or sha in seen or sha not in tag_objects:
            fail("tag does not safely resolve to a commit")
        seen.add(sha)
        payload = tag_objects[sha]
        if not isinstance(payload, dict):
            fail("malformed annotated tag payload")
        mapping = typing.cast(dict[str, object], payload)
        if not isinstance(mapping.get("object"), dict):
            fail("malformed annotated tag payload")
        target = typing.cast(dict[str, object], mapping["object"])
    fail("tag annotation chain is too deep")


def verify_tag(repository: str, tag: str, commit: str, token: str) -> None:
    ref_url = f"{API}/repos/{repository}/git/ref/tags/{urllib.parse.quote(tag, safe='')}"
    ref = load(request(ref_url, token), ref_url)
    if not COMMIT_RE.fullmatch(commit) or not isinstance(ref, dict):
        fail("malformed tag reference")
    mapping = typing.cast(dict[str, object], ref)
    if not isinstance(mapping.get("object"), dict):
        fail("malformed tag reference")
    target = typing.cast(dict[str, object], mapping["object"])
    tag_objects: dict[str, object] = {}
    for _ in range(16):
        if target.get("type") != "tag":
            break
        sha = target.get("sha")
        if not isinstance(sha, str) or not COMMIT_RE.fullmatch(sha) or sha in tag_objects:
            break
        url = f"{API}/repos/{repository}/git/tags/{sha}"
        payload = load(request(url, token), url)
        tag_objects[sha] = payload
        if not isinstance(payload, dict):
            break
        mapping = typing.cast(dict[str, object], payload)
        if not isinstance(mapping.get("object"), dict):
            break
        target = typing.cast(dict[str, object], mapping["object"])
    peel_tag_payloads(ref, tag_objects, commit)


def verify_release(args: argparse.Namespace) -> None:
    token, hashes = token_from(args), sealed_hashes(args.directory, args.version)
    url = f"{API}/repos/{args.repository}/releases/{args.release_id}"
    require_release(load(request(url, token), url), args.tag, args.release_id, args.draft, args.commit)
    entries = _fetch_asset_entries(args.repository, args.release_id, token)
    verify_remote_assets(hashes, {name: (typing.cast(str, item["state"]), asset_digest(name, item.get("digest"))) for name, item in entries.items()})
    if not args.draft:
        verify_tag(args.repository, args.tag, args.commit, token)
    downloaded: dict[str, bytes] = {name: download_asset(args.repository, entries[name], token) for name in sorted(hashes)}
    if {name: sha256_bytes(data) for name, data in downloaded.items()} != hashes:
        fail("downloaded asset digest mismatch")
    records = parse_sums(downloaded[SUMS_NAME], approved_archive_names(args.version))
    for name in approved_archive_names(args.version):
        if records[name] != sha256_bytes(downloaded[name]):
            fail(f"downloaded {SUMS_NAME} digest mismatch for {name}")


def self_test(_: argparse.Namespace) -> None:
    def must_fail(call: typing.Callable[[], object]) -> None:
        try:
            call()
        except SystemExit:
            return
        raise AssertionError("expected failure")

    names = approved_archive_names("1.2.3")
    assert len(names) == 6 and all("v1.2.3-" in name for name in names)
    for invalid_version in ("1.2.3-rc.1", "1.2.3+meta", "01.2.3", "../1"):
        must_fail(lambda invalid_version=invalid_version: approved_archive_names(invalid_version))
    d = "a" * 64
    assert sums_text({"b": d, "a": "b" * 64}) == f"{'b'*64} *a\n{d} *b\n"
    assert asset_digest("a", f"sha256:{d}") == d
    must_fail(lambda: asset_digest("a", "sha256:" + "A" * 64))
    redirect = RejectRedirects()
    try:
        redirect.redirect_request(urllib.request.Request("https://api.github.com/x", headers={"Authorization": "Bearer secret"}), None, 302, "x", {}, "https://evil.example/x")
    except urllib.error.HTTPError:
        pass
    else:
        raise AssertionError("redirect accepted")
    assert safe_download_url("https://github-releases.githubusercontent.com/x") and not safe_download_url("http://github.com/x") and not safe_download_url("https://evilgithubusercontent.com/x")
    local = {name: d for name in approved_names("1.2.3")}
    assert plan_uploads(local, {}) == sorted(local)
    assert plan_uploads(local, local) == []
    must_fail(lambda: plan_uploads(local, {**local, "extra": d}))
    must_fail(lambda: plan_uploads(local, {**local, SUMS_NAME: "b" * 64}))
    remote = {name: ("uploaded", digest) for name, digest in local.items()}
    verify_remote_assets(local, remote)
    must_fail(lambda: verify_remote_assets(local, {key: value for key, value in remote.items() if key != SUMS_NAME}))
    must_fail(lambda: verify_remote_assets(local, {**remote, "extra": ("uploaded", d)}))
    must_fail(lambda: verify_remote_assets(local, {name: ("pending", digest) for name, digest in local.items()}))
    must_fail(lambda: verify_remote_assets(local, {**remote, SUMS_NAME: ("uploaded", "b" * 64)}))
    commit, tag_sha = "1" * 40, "2" * 40
    draft_release = {"id": 1, "tag_name": "v1", "draft": True, "prerelease": False, "target_commitish": commit}
    require_release(draft_release, "v1", 1, True, commit)
    must_fail(lambda: require_release({"id": 1, "tag_name": "v1", "draft": False, "prerelease": False, "target_commitish": commit}, "v1", 1, True, commit))
    must_fail(lambda: require_release({"id": 1, "tag_name": "v1", "draft": True, "prerelease": True, "target_commitish": commit}, "v1", 1, True, commit))
    must_fail(lambda: require_release({"id": 1, "tag_name": "v1", "draft": True, "prerelease": False}, "v1", 1, True, commit))
    must_fail(lambda: require_release({"id": 1, "tag_name": "v1", "draft": True, "prerelease": False, "target_commitish": "not-a-sha"}, "v1", 1, True, commit))
    must_fail(lambda: require_release({**draft_release, "target_commitish": tag_sha}, "v1", 1, True, commit))
    must_fail(lambda: require_release(draft_release, "v1", 1, True, "not-a-sha"))
    peel_tag_payloads({"object": {"type": "commit", "sha": commit}}, {}, commit)
    peel_tag_payloads({"object": {"type": "tag", "sha": tag_sha}}, {tag_sha: {"object": {"type": "commit", "sha": commit}}}, commit)
    must_fail(lambda: peel_tag_payloads({"object": {"type": "tag", "sha": tag_sha}}, {}, commit))
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        incoming = root / "incoming"; incoming.mkdir()
        for name in names: (incoming / name).write_bytes(name.encode())
        staged = root / "staged"
        stage(argparse.Namespace(destination=staged, source=[incoming], version="1.2.3"))
        must_fail(lambda: stage(argparse.Namespace(destination=staged, source=[incoming], version="1.2.3")))
        must_fail(lambda: stage(argparse.Namespace(destination=root / "duplicate", source=[incoming, incoming], version="1.2.3")))
        must_fail(lambda: stage(argparse.Namespace(destination=root / "inside", source=[root], version="1.2.3")))
        seal(argparse.Namespace(directory=staged, version="1.2.3"))
        first = (staged / SUMS_NAME).read_bytes(); sealed_hashes(staged, "1.2.3"); assert first == (staged / SUMS_NAME).read_bytes()
        (staged / next(iter(names))).write_bytes(b"drift")
        must_fail(lambda: sealed_hashes(staged, "1.2.3"))
        (incoming / "bad.whl").write_bytes(b"x")
        must_fail(lambda: stage(argparse.Namespace(destination=root / "bad", source=[incoming], version="1.2.3")))
        (incoming / "bad.whl").unlink()
        (incoming / SUMS_NAME).write_text("stale\n", encoding="ascii")
        must_fail(lambda: stage(argparse.Namespace(destination=root / "stale", source=[incoming], version="1.2.3")))
        (incoming / SUMS_NAME).unlink()
        (incoming / "nested").mkdir()
        must_fail(lambda: stage(argparse.Namespace(destination=root / "nested", source=[incoming], version="1.2.3")))
        (incoming / "nested").rmdir()
        must_fail(lambda: stage(argparse.Namespace(destination=root / "missing", source=[incoming / "missing"], version="1.2.3")))
        linked = root / "linked"; linked.mkdir(); (linked / "x").symlink_to(incoming / next(iter(names)))
        must_fail(lambda: plain_files(linked))
        malformed = root / "malformed"; malformed.mkdir()
        for name in names: (malformed / name).write_bytes(b"x")
        (malformed / SUMS_NAME).write_text("not a checksum\n", encoding="ascii")
        must_fail(lambda: sealed_hashes(malformed, "1.2.3"))
    print("attach_release_assets self-test passed")


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="mode", required=True)
    staging = sub.add_parser("stage"); staging.add_argument("--destination", type=Path, required=True); staging.add_argument("--source", type=Path, action="append", required=True); staging.add_argument("--version", required=True); staging.set_defaults(func=stage)
    sealing = sub.add_parser("seal"); sealing.add_argument("--directory", type=Path, required=True); sealing.add_argument("--version", required=True); sealing.set_defaults(func=seal)
    upload = sub.add_parser("attach")
    upload.add_argument("--repository", required=True); upload.add_argument("--release-id", type=int, required=True)
    upload.add_argument("--tag", required=True); upload.add_argument("--commit", required=True); upload.add_argument("--version", required=True)
    upload.add_argument("--directory", type=Path, required=True)
    upload.add_argument("--token-env", default="GH_TOKEN"); upload.set_defaults(func=attach)
    verify = sub.add_parser("verify-release")
    verify.add_argument("--repository", required=True); verify.add_argument("--release-id", type=int, required=True)
    verify.add_argument("--tag", required=True); verify.add_argument("--commit", required=True)
    verify.add_argument("--version", required=True); verify.add_argument("--directory", type=Path, required=True)
    verify.add_argument("--draft", choices=("true", "false"), required=True); verify.add_argument("--token-env", default="GH_TOKEN"); verify.set_defaults(func=verify_release)
    test = sub.add_parser("self-test"); test.set_defaults(func=self_test)
    return p


if __name__ == "__main__":
    arguments = parser().parse_args()
    if getattr(arguments, "draft", None) is not None:
        arguments.draft = arguments.draft == "true"
    arguments.func(arguments)
