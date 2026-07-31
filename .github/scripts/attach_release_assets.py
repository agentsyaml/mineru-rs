#!/usr/bin/env python3
"""Attach verified release artifacts to a published GitHub Release.

Fail-closed, standard library only. Uploads are bound to an explicit release id
whose tag must match the tag the workflow verified. Re-runs are idempotent for
byte-identical assets and fail on any same-name digest mismatch; nothing is
clobbered and no dist-tag or release metadata is modified.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import typing
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

API = "https://api.github.com"
API_VERSION = "2022-11-28"
USER_AGENT = "mineru-rs-release-attacher/1"
SUMS_NAME = "SHA256SUMS"


def fail(message: str) -> typing.NoReturn:
    raise SystemExit(f"release attachment failed: {message}")


def sha256_bytes(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def asset_digest(name: str, value: object) -> str:
    """Return GitHub's immutable SHA-256 asset digest without its algorithm prefix."""
    prefix = "sha256:"
    if (
        not isinstance(value, str)
        or len(value) != len(prefix) + 64
        or not value.startswith(prefix)
        or any(character not in "0123456789abcdef" for character in value[len(prefix):])
    ):
        fail(f"asset {name!r} has invalid digest; expected sha256:<64 lowercase hexadecimal characters>")
    return value[len(prefix):]


def plain_files(directory: Path) -> list[Path]:
    """Top-level regular files only; symlinks and nested entries are rejected."""
    if directory.is_symlink() or not directory.is_dir():
        fail(f"{directory} must be a real directory")
    entries = sorted(directory.iterdir())
    for entry in entries:
        if entry.is_symlink() or not entry.is_file():
            fail(f"unexpected entry in {directory}: {entry.name}")
    if not entries:
        fail(f"{directory} contains no files to attach")
    return entries


def sums_text(hashes: dict[str, str]) -> str:
    """GNU coreutils binary-mode format, sorted by asset name."""
    return "".join(f"{hashes[name]} *{name}\n" for name in sorted(hashes))


def require_tag(release: object, tag: str, release_id: int) -> None:
    if not isinstance(release, dict):
        fail("malformed release payload")
    mapping = typing.cast(dict[str, object], release)
    if mapping.get("id") != release_id:
        fail(f"release id mismatch: requested {release_id}, received {mapping.get('id')!r}")
    if mapping.get("tag_name") != tag:
        fail(f"release {release_id} is tagged {mapping.get('tag_name')!r}, expected {tag!r}")


def plan_uploads(local: dict[str, str], remote: dict[str, str]) -> list[str]:
    """Names still needing upload. Identical assets are skipped, conflicts fail."""
    for name, digest in sorted(remote.items()):
        if name in local and local[name] != digest:
            fail(f"{name} already attached with a different digest; refusing to replace it")
    return [name for name in sorted(local) if name not in remote]


class RejectRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, "HTTP redirect rejected", headers, fp)


def request(url: str, token: str, method: str = "GET", body: bytes | None = None,
            content_type: str | None = None, accept: str = "application/vnd.github+json") -> bytes:
    headers = {
        "Accept": accept,
        "Authorization": f"Bearer {token}",
        "X-GitHub-Api-Version": API_VERSION,
        "User-Agent": USER_AGENT,
    }
    if content_type:
        headers["Content-Type"] = content_type
    call = urllib.request.Request(url, data=body, headers=headers, method=method)
    opener = urllib.request.build_opener(RejectRedirects())
    try:
        with opener.open(call, timeout=120) as response:
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


def _fetch_assets(repository: str, release_id: int, token: str) -> dict[str, tuple[str, str]]:
    """Return {name: (state, digest)} for every asset in the release."""
    assets: dict[str, tuple[str, str]] = {}
    page = 1
    while True:
        url = f"{API}/repos/{repository}/releases/{release_id}/assets?per_page=100&page={page}"
        raw = load(request(url, token), url)
        if not isinstance(raw, list):
            fail(f"malformed asset list from {url}")
        for entry in typing.cast(list[object], raw):
            if not isinstance(entry, dict):
                fail(f"malformed asset entry from {url}")
            item = typing.cast(dict[str, object], entry)
            name, state = item.get("name"), item.get("state")
            if not isinstance(name, str) or not isinstance(state, str):
                fail(f"malformed asset entry from {url}")
            if name in assets:
                fail(f"release {release_id} lists {name} more than once")
            assets[name] = (state, asset_digest(name, item.get("digest")))
        if len(typing.cast(list[object], raw)) < 100:
            return assets
        page += 1


def remote_hashes(repository: str, release_id: int, token: str) -> dict[str, str]:
    """Return GitHub's immutable SHA-256 digest for every existing asset."""
    return {name: digest for name, (state, digest) in _fetch_assets(repository, release_id, token).items()}


# ponytail: pure function for testability, no network I/O
def verify_remote_assets(expected: dict[str, str], remote: dict[str, tuple[str, str]]) -> None:
    """Fail unless every expected name exists remotely with state='uploaded' and matching digest."""
    for name, expected_digest in sorted(expected.items()):
        if name not in remote:
            fail(f"asset {name!r} not found in remote release")
        state, remote_digest = remote[name]
        if state != "uploaded":
            fail(f"asset {name!r} has state {state!r}, expected 'uploaded'")
        if remote_digest != expected_digest:
            fail(f"asset {name!r} digest mismatch: expected {expected_digest}, got {remote_digest}")


def stage(args: argparse.Namespace) -> None:
    destination = args.destination
    sources = [source.resolve() for source in args.source]
    resolved = destination.resolve()
    for source in sources:
        if resolved == source or source in resolved.parents or resolved in source.parents:
            fail(f"destination {destination} must not overlap source {source}")
    if destination.exists():
        fail(f"{destination} already exists")
    destination.mkdir(parents=True)
    staged: set[str] = set()
    for source in sources:
        for path in plain_files(source):
            if path.name == SUMS_NAME:
                fail(f"{SUMS_NAME} must not come from a verified payload directory")
            if path.name in staged:
                fail(f"duplicate asset name across sources: {path.name}")
            staged.add(path.name)
            shutil.copyfile(path, destination / path.name)
    print("\n".join(sorted(staged)))


def attach(args: argparse.Namespace) -> None:
    # Read the credential from the environment; argv is world-readable on the runner.
    token = os.environ.get(args.token_env, "")
    if not token:
        fail(f"{args.token_env} is unset or empty")
    sums = args.directory / SUMS_NAME
    if sums.exists():
        fail(f"{sums} already exists; stage a clean attachment directory")
    hashes = {path.name: sha256_file(path) for path in plain_files(args.directory)}
    sums.write_text(sums_text(hashes), encoding="utf-8")
    hashes[SUMS_NAME] = sha256_file(sums)

    url = f"{API}/repos/{args.repository}/releases/{args.release_id}"
    require_tag(load(request(url, token), url), args.tag, args.release_id)
    pending = plan_uploads(hashes, remote_hashes(args.repository, args.release_id, token))
    for name in pending:
        upload = "{}/repos/{}/releases/{}/assets?name={}".format(
            "https://uploads.github.com", args.repository, args.release_id,
            urllib.parse.quote(name, safe=""),
        )
        request(upload, token, method="POST", body=(args.directory / name).read_bytes(),
                content_type="application/octet-stream")
        print(f"attached {name}")
    for name in sorted(set(hashes) - set(pending)):
        print(f"already attached with matching digest: {name}")
    # Post-upload verification: every expected asset must be present with
    # state=uploaded and matching digest.
    verify_remote_assets(hashes, _fetch_assets(args.repository, args.release_id, token))


def self_test(_: argparse.Namespace) -> None:
    import tempfile

    def must_fail(call: typing.Callable[[], object], message: str) -> None:
        try:
            call()
        except SystemExit:
            return
        raise AssertionError(message)

    assert sums_text({"b.whl": "2" * 64, "a.crate": "1" * 64}) == (
        f"{'1' * 64} *a.crate\n{'2' * 64} *b.whl\n"
    )
    assert asset_digest("valid.whl", f"sha256:{'a' * 64}") == "a" * 64
    invalid_digests = (
        ("null", None),
        ("missing", {}.get("digest")),
        ("malformed", f"sha256:{'a' * 63}"),
        ("uppercase", f"sha256:{'A' * 64}"),
        ("other algorithm", f"sha512:{'a' * 64}"),
        ("invalid type", 123),
    )
    for case, digest in invalid_digests:
        must_fail(
            lambda digest=digest: asset_digest("invalid.whl", digest),
            f"{case} asset digest was accepted",
        )

    redirect_handler = RejectRedirects()
    for method, body, code in (("GET", None, 302), ("POST", b"upload", 307)):
        original = urllib.request.Request(
            "https://api.github.com/original",
            data=body,
            headers={"Authorization": "Bearer secret"},
            method=method,
        )
        assert original.get_header("Authorization") == "Bearer secret"
        try:
            redirected = redirect_handler.redirect_request(
                original, None, code, "Redirect", {}, "https://example.com/redirected"
            )
        except urllib.error.HTTPError as error:
            assert error.code == code
        else:
            raise AssertionError(f"{method} redirect returned a request: {redirected!r}")

    assert plan_uploads({"a": "1" * 64}, {}) == ["a"]
    assert plan_uploads({"a": "1" * 64}, {"a": "1" * 64}) == []
    assert plan_uploads({"a": "1" * 64, "b": "2" * 64}, {"a": "1" * 64}) == ["b"]
    assert plan_uploads({"a": "1" * 64}, {"other": "9" * 64}) == ["a"]
    must_fail(
        lambda: plan_uploads({"a": "1" * 64}, {"a": "2" * 64}),
        "same-name digest mismatch was accepted",
    )

    # verify_remote_assets — pure function tests
    d_a, d_b, d_s = "1" * 64, "2" * 64, "3" * 64
    verify_remote_assets({"a": d_a}, {"a": ("uploaded", d_a)})
    verify_remote_assets(
        {"a": d_a, "b": d_b},
        {"a": ("uploaded", d_a), "b": ("uploaded", d_b)},
    )
    verify_remote_assets(
        {"a": d_a, SUMS_NAME: d_s},
        {"a": ("uploaded", d_a), SUMS_NAME: ("uploaded", d_s)},
    )
    must_fail(
        lambda: verify_remote_assets({"a": d_a}, {}),
        "missing asset was accepted",
    )
    must_fail(
        lambda: verify_remote_assets({"a": d_a}, {"a": ("pending", d_a)}),
        "non-uploaded state was accepted",
    )
    must_fail(
        lambda: verify_remote_assets({"a": d_a}, {"a": ("uploaded", d_b)}),
        "digest mismatch was accepted",
    )
    must_fail(
        lambda: verify_remote_assets({"a": d_a, "b": d_b}, {"a": ("uploaded", d_a), "b": ("stale", d_b)}),
        "partial non-uploaded was accepted",
    )

    release = {"id": 7, "tag_name": "v1.2.3"}
    require_tag(release, "v1.2.3", 7)
    must_fail(lambda: require_tag(release, "v1.2.4", 7), "tag mismatch was accepted")
    must_fail(lambda: require_tag(release, "v1.2.3", 8), "release id mismatch was accepted")
    must_fail(lambda: require_tag([], "v1.2.3", 7), "malformed release payload was accepted")

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        crate = root / "crate"
        wheels = root / "wheels"
        crate.mkdir()
        wheels.mkdir()
        (crate / "mineru-1.2.3.crate").write_bytes(b"crate")
        (wheels / "mineru_rs-1.2.3.whl").write_bytes(b"wheel")

        assert sha256_file(crate / "mineru-1.2.3.crate") == sha256_bytes(b"crate")
        must_fail(lambda: plain_files(root / "missing"), "missing directory was accepted")
        empty = root / "empty"
        empty.mkdir()
        must_fail(lambda: plain_files(empty), "empty directory was accepted")
        nested = root / "nested"
        (nested / "inner").mkdir(parents=True)
        must_fail(lambda: plain_files(nested), "nested directory was accepted")

        upload = root / "upload"
        stage(argparse.Namespace(destination=upload, source=[crate, wheels]))
        assert sorted(p.name for p in upload.iterdir()) == [
            "mineru-1.2.3.crate", "mineru_rs-1.2.3.whl",
        ]
        must_fail(
            lambda: stage(argparse.Namespace(destination=upload, source=[crate])),
            "existing destination was accepted",
        )
        must_fail(
            lambda: stage(argparse.Namespace(destination=root / "inside", source=[root])),
            "overlapping destination was accepted",
        )
        duplicate = root / "duplicate"
        duplicate.mkdir()
        (duplicate / "mineru-1.2.3.crate").write_bytes(b"other")
        must_fail(
            lambda: stage(argparse.Namespace(destination=root / "dupe-out", source=[crate, duplicate])),
            "duplicate asset name was accepted",
        )
        polluted = root / "polluted"
        polluted.mkdir()
        (polluted / SUMS_NAME).write_text("x\n", encoding="utf-8")
        must_fail(
            lambda: stage(argparse.Namespace(destination=root / "sums-out", source=[polluted])),
            f"{SUMS_NAME} inside a payload directory was accepted",
        )

        # A pre-existing SHA256SUMS in the attachment directory is a staging bug.
        (upload / SUMS_NAME).write_text("stale\n", encoding="utf-8")
        must_fail(
            lambda: attach(argparse.Namespace(
                directory=upload, repository="o/r", release_id=1, tag="v1.2.3",
                token_env="MINERU_SELF_TEST_ABSENT_TOKEN",
            )),
            "stale SHA256SUMS was accepted",
        )
        (upload / SUMS_NAME).unlink()
        must_fail(
            lambda: attach(argparse.Namespace(
                directory=upload, repository="o/r", release_id=1, tag="v1.2.3",
                token_env="MINERU_SELF_TEST_ABSENT_TOKEN",
            )),
            "absent credential was accepted",
        )
    print("attach_release_assets self-test passed")


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="mode", required=True)

    staging = sub.add_parser("stage")
    staging.add_argument("--destination", type=Path, required=True)
    staging.add_argument("--source", type=Path, action="append", required=True)
    staging.set_defaults(func=stage)

    upload = sub.add_parser("attach")
    upload.add_argument("--repository", required=True)
    upload.add_argument("--release-id", type=int, required=True)
    upload.add_argument("--tag", required=True)
    upload.add_argument("--directory", type=Path, required=True)
    upload.add_argument("--token-env", default="GH_TOKEN")
    upload.set_defaults(func=attach)

    test = sub.add_parser("self-test")
    test.set_defaults(func=self_test)
    return p


if __name__ == "__main__":
    arguments = parser().parse_args()
    arguments.func(arguments)
