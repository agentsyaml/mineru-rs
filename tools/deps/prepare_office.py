#!/usr/bin/env python3
"""Prepare or verify the pinned, local Office XML dependency sources."""

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path, PurePosixPath

from archive_safe import (
    ArchiveError,
    _DeadlineHTTPHandler,
    _DeadlineHTTPSHandler,
    _DeadlineHTTPConnection,
    _DeadlineHTTPSConnection,
    MAX_DOWNLOAD,
    MAX_EXPANDED,
    MAX_FILE,
    MAX_MEMBERS,
    extract_tar_gz,
    validate_relative_path,
)


ROOT = Path(__file__).resolve().parents[2]
OUTPUT = ROOT / ".deps" / "office-xml041"
LOCK = ROOT / ".deps" / ".office-xml041.prepare.lock"
SOURCE_PIN = "a89283b679b4fed25a6f0d43b6990954e3735a091f47e4a946ddfafd674cb43a"
RECIPE_PIN = "23f030e0872b6d3f6882968b2a513caa90569cf7380c447e8a3a111b598dc049"
ALLOWED_HOSTS = {"static.crates.io", "codeload.github.com"}
TIMEOUT_SECONDS = 30
MAX_DOWNLOAD_SECONDS = 120


class PreparationError(RuntimeError):
    """Pinned source preparation failed closed."""


def _sha(data):
    return hashlib.sha256(data).hexdigest()


def _read_pinned(path, expected):
    if path.is_symlink() or not path.is_file():
        raise PreparationError("tracked preparation metadata is missing")
    raw = path.read_bytes()
    if not re.fullmatch(r"[0-9a-f]{64}", expected) or _sha(raw) != expected:
        raise PreparationError("tracked preparation metadata failed its digest check")
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise PreparationError("preparation metadata is invalid JSON") from None


class _ApprovedRedirect(Exception):
    def __init__(self, url):
        self.url = url


class _PinnedRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        try:
            parsed = urllib.parse.urlsplit(newurl)
            if (parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS
                    or parsed.username or parsed.password or parsed.port not in (None, 443)):
                raise PreparationError("archive redirect is not approved HTTPS")
        except ValueError:
            raise PreparationError("archive redirect URL is invalid") from None
        finally:
            fp.close()
        raise _ApprovedRedirect(newurl)


class _DeadlineHTTPHandler(urllib.request.HTTPHandler):
    def __init__(self, deadline, clock):
        super().__init__()
        self.deadline = deadline
        self.clock = clock

    def http_open(self, req):
        connection = lambda host, **kwargs: _DeadlineHTTPConnection(
            host, self.deadline, self.clock, **kwargs
        )
        return self.do_open(connection, req)


class _DeadlineHTTPSHandler(urllib.request.HTTPSHandler):
    def __init__(self, deadline, clock):
        super().__init__()
        self.deadline = deadline
        self.clock = clock

    def https_open(self, req):
        connection = lambda host, **kwargs: _DeadlineHTTPSConnection(
            host, self.deadline, self.clock, **kwargs
        )
        # check_hostname was removed from HTTPSConnection in Python 3.12; the
        # validating context still verifies hostnames.
        return self.do_open(
            connection,
            req,
            context=getattr(self, "_context", None),
        )


def _proxy_without_credentials():
    value = os.environ.get("https_proxy") or os.environ.get("HTTPS_PROXY")
    if not value:
        return {}
    parsed = urllib.parse.urlsplit(value if "://" in value else "http://" + value)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise PreparationError("HTTPS proxy URL is invalid")
    if parsed.path not in ("", "/") or parsed.query or parsed.fragment:
        raise PreparationError("HTTPS proxy URL is invalid")
    host = parsed.hostname
    if ":" in host:
        host = "[" + host + "]"
    netloc = host + (":" + str(parsed.port) if parsed.port else "")
    return {"https": parsed.scheme + "://" + netloc}


def _response_socket(response):
    # Real HTTPResponse exposes fp.raw._sock; bounded responses expose
    # fp.raw._sock directly and test doubles may nest one level deeper.
    stream = getattr(response, "fp", response)
    raw = getattr(stream, "raw", None) or getattr(getattr(stream, "fp", None), "raw", None)
    sock = getattr(raw, "_sock", None) or getattr(getattr(raw, "_source", None), "_sock", None)
    if sock is None or not callable(getattr(sock, "settimeout", None)):
        raise PreparationError("archive response has no adjustable socket")
    return sock


def _download(source, destination, clock=None):
    clock = clock or time.monotonic
    deadline = clock() + MAX_DOWNLOAD_SECONDS
    parsed = urllib.parse.urlsplit(source["url"])
    if (parsed.scheme != "https" or parsed.hostname not in ALLOWED_HOSTS
            or parsed.username or parsed.password or parsed.port not in (None, 443)):
        raise PreparationError("archive URL is not approved HTTPS")
    headers = {"Accept-Encoding": "identity", "User-Agent": "mineru-office-xml041/1"}
    redirect_handler = _PinnedRedirect()
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler(_proxy_without_credentials()),
        redirect_handler,
        _DeadlineHTTPHandler(deadline, clock),
        _DeadlineHTTPSHandler(deadline, clock),
    )
    url = source["url"]
    redirect_counts = {}
    redirect_total = 0
    while True:
        remaining = deadline - clock()
        if remaining <= 0:
            raise PreparationError("archive download timed out")
        request = urllib.request.Request(url, headers=headers)
        try:
            response = opener.open(request, timeout=min(TIMEOUT_SECONDS, remaining))
        except _ApprovedRedirect as redirect:
            redirect_total += 1
            redirect_counts[redirect.url] = redirect_counts.get(redirect.url, 0) + 1
            if (redirect_total > redirect_handler.max_redirections
                    or redirect_counts[redirect.url] > redirect_handler.max_repeats):
                raise PreparationError("archive redirect limit exceeded") from None
            url = redirect.url
            continue
        if clock() >= deadline:
            response.close()
            raise PreparationError("archive download timed out")
        break

    count = 0
    digest = hashlib.sha256()
    try:
        with response:
            final = urllib.parse.urlsplit(response.geturl())
            if (final.scheme != "https" or final.hostname not in ALLOWED_HOSTS
                    or final.username or final.password or final.port not in (None, 443)):
                raise PreparationError("archive response URL is not approved HTTPS")
            read = getattr(response, "read1", None)
            if not callable(read):
                raise PreparationError("archive response has no bounded read operation")
            sock = _response_socket(response)
            headers = getattr(response, "headers", None)
            content_length = headers.get("Content-Length") if headers else None
            expected_total = int(content_length) if content_length and content_length.isdigit() else None
            with destination.open("xb") as output:
                while True:
                    remaining = deadline - clock()
                    if remaining <= 0:
                        raise PreparationError("archive download timed out")
                    sock.settimeout(remaining)
                    block = read(min(64 * 1024, MAX_DOWNLOAD + 1 - count))
                    if clock() >= deadline:
                        raise PreparationError("archive download timed out")
                    if not block:
                        break
                    count += len(block)
                    if count > MAX_DOWNLOAD:
                        raise PreparationError("archive download limit exceeded")
                    digest.update(block)
                    output.write(block)
                    # Newer Python closes the response socket once Content-Length
                    # is exactly consumed, so a following settimeout would see a
                    # dead descriptor; stop touching the socket when complete.
                    if expected_total is not None and count >= expected_total:
                        break
    except (urllib.error.URLError, OSError, TimeoutError) as error:
        raise PreparationError("pinned source download failed") from error
    if digest.hexdigest() != source["sha256"]:
        raise PreparationError("downloaded archive checksum mismatch")


def _safe_relative(value):
    if value.endswith("/"):
        raise PreparationError("unsafe recipe or tree path")
    try:
        parts = validate_relative_path(value)
    except (ArchiveError, UnicodeEncodeError):
        raise PreparationError("unsafe recipe or tree path")
    return PurePosixPath(*parts)


def _file_at(root, relative):
    path = _safe_relative(relative)
    current = root
    for part in path.parts:
        current = current / part
        if current.is_symlink():
            raise PreparationError("prepared source contains a symlink")
    if not current.is_file():
        raise PreparationError("expected prepared source file is missing")
    return current


def _check_fragments(path, fragments):
    text = path.read_text(encoding="utf-8")
    for fragment in fragments:
        if text.count(fragment) < 1:
            raise PreparationError("source package fragment check failed: " + fragment)


def _apply_recipes(stage, recipe_data):
    parents = {"office2pdf", "umya-spreadsheet", "citationberg"}
    seen = set()
    for name, item in recipe_data.get("files", {}).items():
        relative = _safe_relative(name)
        if relative.parts[0] not in parents or name in seen:
            raise PreparationError("edit recipe path is invalid or duplicated")
        seen.add(name)
        path = _file_at(stage, name)
        raw = path.read_bytes()
        input_hash, output_hash, replacements = item
        if _sha(raw) != input_hash:
            raise PreparationError("source edit input checksum mismatch")
        for old_text, new_text, expected_count in replacements:
            old = old_text.encode("utf-8")
            new = new_text.encode("utf-8")
            if not old or raw.count(old) != expected_count:
                raise PreparationError("source edit fragment count mismatch")
            raw = raw.replace(old, new)
        if _sha(raw) != output_hash:
            raise PreparationError("source edit output checksum mismatch")
        path.write_bytes(raw)


def _write_exec_manifest(stage, executable):
    executable = sorted(executable)
    marker = stage / ".source-executable-files.json"
    marker.write_bytes((json.dumps(executable, separators=(",", ":")) + "\n").encode("utf-8"))
    return set(executable)


def _tree_digest(root):
    marker = _file_at(root, ".source-executable-files.json")
    try:
        executable = json.loads(marker.read_text(encoding="utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise PreparationError("prepared source executable manifest is invalid") from None
    if (not isinstance(executable, list) or executable != sorted(set(executable))
            or any(not isinstance(path, str) for path in executable)):
        raise PreparationError("prepared source executable manifest is invalid")
    executable = set(executable)
    actual = set()
    entries = []
    member_count = 0
    expanded = 0
    for parent, dirs, files in os.walk(str(root), followlinks=False):
        base = Path(parent)
        for name in list(dirs):
            path = base / name
            if path.is_symlink():
                raise PreparationError("prepared source contains a symlink")
            relative = path.relative_to(root).as_posix()
            _safe_relative(relative)
            member_count += 1
            if member_count > MAX_MEMBERS:
                raise PreparationError("prepared source member limit exceeded")
            entries.append((relative, path, True))
        for name in files:
            path = base / name
            if path.is_symlink() or not path.is_file():
                raise PreparationError("prepared source contains a special file")
            rel = path.relative_to(root).as_posix()
            _safe_relative(rel)
            member_count += 1
            size = path.stat().st_size
            expanded += size
            if member_count > MAX_MEMBERS or size > MAX_FILE or expanded > MAX_EXPANDED:
                raise PreparationError("prepared source size limit exceeded")
            is_exec = rel in executable
            actual.add(rel) if is_exec else None
            if os.name != "nt" and bool(path.stat().st_mode & 0o111) != is_exec:
                raise PreparationError("prepared source executable bit changed")
            entries.append((rel, path, False))
    if actual != executable:
        raise PreparationError("prepared source executable manifest does not match tree")
    digest = hashlib.sha256()
    for relative, path, is_dir in sorted(entries, key=lambda item: item[0]):
        name = relative.encode("utf-8")
        digest.update(b"D\0" + name + b"\0" if is_dir else b"F\0" + name + b"\0")
        if not is_dir:
            digest.update(b"\1" if relative in executable else b"\0")
            digest.update(path.stat().st_size.to_bytes(8, "big"))
            with path.open("rb") as stream:
                for block in iter(lambda: stream.read(1024 * 1024), b""):
                    digest.update(block)
    return digest.hexdigest()


def _verify_parent(stage, source):
    parent = source["package"]
    manifest = _file_at(stage, parent + "/Cargo.toml")
    _check_fragments(manifest, source["patched_fragments"])
    for license_file in source["license_files"]:
        _file_at(stage, parent + "/" + license_file)


def _acquire_lock():
    deps = ROOT / ".deps"
    if deps.is_symlink():
        raise PreparationError("dependency output directory is a symlink")
    deps.mkdir(exist_ok=True)
    try:
        fd = os.open(str(LOCK), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        raise PreparationError("another preparation is active or left a lock") from None
    info = os.fstat(fd)
    identity = (info.st_dev, info.st_ino)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write((str(os.getpid()) + "\n").encode("ascii"))
            stream.flush()
        return identity
    except BaseException:
        _release_lock(identity)
        raise


def _release_lock(identity):
    try:
        info = LOCK.lstat()
        if (info.st_dev, info.st_ino) == identity:
            LOCK.unlink()
    except FileNotFoundError:
        pass


def _load_metadata():
    sources = _read_pinned(ROOT / "tools/deps/office-xml041/sources.json", SOURCE_PIN)
    recipes = _read_pinned(ROOT / "tools/deps/office-xml041/edit_recipes.json", RECIPE_PIN)
    if (sources.get("schema_version") != 1 or recipes.get("schema_version") != 1
            or not re.fullmatch(r"[0-9a-f]{64}", sources.get("tree_sha256", ""))
            or len(sources.get("sources", [])) != 3):
        raise PreparationError("pinned source metadata is incomplete")
    return sources, recipes


def verify():
    if LOCK.exists() or LOCK.is_symlink():
        raise PreparationError("preparation lock is present")
    sources, _ = _load_metadata()
    if (ROOT / ".deps").is_symlink() or not (ROOT / ".deps").is_dir():
        raise PreparationError("prepared dependency directory is missing")
    if OUTPUT.is_symlink() or not OUTPUT.is_dir():
        raise PreparationError("prepared dependency tree is missing")
    if _tree_digest(OUTPUT) != sources["tree_sha256"]:
        raise PreparationError("prepared dependency tree digest mismatch")
    return sources["tree_sha256"]


def prepare():
    identity = _acquire_lock()
    stage = None
    try:
        sources, recipes = _load_metadata()
        if OUTPUT.is_symlink():
            raise PreparationError("prepared dependency destination is a symlink")
        if OUTPUT.exists():
            if not OUTPUT.is_dir() or _tree_digest(OUTPUT) != sources["tree_sha256"]:
                raise PreparationError("existing prepared dependency tree does not match")
            return sources["tree_sha256"]
        stage = Path(tempfile.mkdtemp(prefix=".office-xml041-stage-", dir=str(ROOT / ".deps")))
        downloads = stage / ".downloads"
        downloads.mkdir()
        executable = set()
        names = set()
        for source in sources["sources"]:
            package = source["package"]
            archive_name = source.get("archive_name", "")
            if (package not in {"office2pdf", "umya-spreadsheet", "citationberg"}
                    or package in names or not re.fullmatch(r"[A-Za-z0-9_.-]+", archive_name)
                    or archive_name in (".", "..")
                    or not re.fullmatch(r"[0-9a-f]{64}", source.get("sha256", ""))):
                raise PreparationError("pinned source package identity is invalid")
            names.add(package)
            archive_path = downloads / source["archive_name"]
            try:
                _download(source, archive_path)
            except PreparationError as error:
                raise PreparationError("pinned archive download failed for " + package) from error
            output = stage / package
            executable.update(
                package + "/" + item
                for item in extract_tar_gz(archive_path, output, source["archive_root"])
            )
            manifest = _file_at(stage, package + "/Cargo.toml")
            _check_fragments(manifest, source["identity_fragments"])
            for license_file in source["license_files"]:
                _file_at(stage, package + "/" + license_file)
        if names != {"office2pdf", "umya-spreadsheet", "citationberg"}:
            raise PreparationError("pinned source package set is incomplete")
        _apply_recipes(stage, recipes)
        for source in sources["sources"]:
            _verify_parent(stage, source)
        shutil.rmtree(downloads)
        _write_exec_manifest(stage, executable)
        digest = _tree_digest(stage)
        if digest != sources["tree_sha256"]:
            raise PreparationError("prepared dependency content digest mismatch")
        if OUTPUT.exists() or OUTPUT.is_symlink():
            raise PreparationError("prepared dependency destination appeared concurrently")
        os.rename(str(stage), str(OUTPUT))
        stage = None
        return digest
    finally:
        if stage is not None:
            shutil.rmtree(stage, ignore_errors=True)
        _release_lock(identity)


def _terminate(signum, frame):
    raise KeyboardInterrupt


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("prepare", help="download, verify, adapt, and publish pinned sources")
    commands.add_parser("verify", help="verify the prepared tree without network access")
    args = parser.parse_args(argv)
    old_term = signal.signal(signal.SIGTERM, _terminate)
    try:
        digest = prepare() if args.command == "prepare" else verify()
        print("{} office XML sources ({})".format(args.command, digest))
        return 0
    except KeyboardInterrupt:
        print("office XML source preparation interrupted; owned temporary state removed", file=sys.stderr)
        return 130
    except (PreparationError, ArchiveError, OSError, ValueError, tarfile.TarError):
        print("office XML source {} failed closed".format(args.command), file=sys.stderr)
        return 1
    finally:
        signal.signal(signal.SIGTERM, old_term)


if __name__ == "__main__":
    sys.exit(main())
