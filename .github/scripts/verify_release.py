#!/usr/bin/env python3
"""Fail-closed release artifact checks using only the Python standard library."""

from __future__ import annotations

import argparse
import email.parser
import hashlib
import io
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
import typing
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path, PurePosixPath

tomllib: typing.Any
try:
    import tomllib as _tomllib

    tomllib = _tomllib
except ModuleNotFoundError:  # Python 3.9 wheel lanes never parse TOML.
    tomllib = None

REPOSITORY = "https://github.com/agentsyaml/mineru-rs"
LICENSE = "MIT OR Apache-2.0"
BINS = {"mineru", "mineru-api", "mineru-vlm", "mineru-vlm-api", "mineru-office-convert"}
NPM_ROOT = "@alexsun-top/mineru"
PLATFORMS = {
    "darwin-x64": ("darwin", "x64", None, "macosx_10_12_x86_64"),
    "darwin-arm64": ("darwin", "arm64", None, "macosx_11_0_arm64"),
    "linux-x64-gnu": ("linux", "x64", "glibc", "manylinux2014_x86_64"),
    "linux-arm64-gnu": ("linux", "arm64", "glibc", "manylinux2014_aarch64"),
    "win32-x64-msvc": ("win32", "x64", None, "win_amd64"),
    "win32-arm64-msvc": ("win32", "arm64", None, ""),
}
NPM_ROOT_FILES = ["index.js", "index.d.ts", "api.js", "api.d.ts", "bin/mineru.js"]
WHEEL_SLOTS = {
    "manylinux-x64": "manylinux2014_x86_64",
    "manylinux-arm64": "manylinux2014_aarch64",
    "macos-x64": "macosx_10_12_x86_64",
    "macos-arm64": "macosx_11_0_arm64",
    "windows-x64": "win_amd64",
}
TAG_RE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")
NPM_DEV_DEPENDENCIES = {"@napi-rs/cli": "^3.8.2"}
PYPI_USER_AGENT = "mineru-rs-release-verifier/1"
PYTHON_SCRIPTS = {"mineru": "mineru_rs._cli:main", "mineru-rs": "mineru_rs._cli:main"}
NODE_ROOT_BIN = {"mineru": "bin/mineru.js", "mineru-rs": "bin/mineru.js"}
WHEEL_ENTRY_POINTS = b"[console_scripts]\nmineru=mineru_rs._cli:main\nmineru-rs=mineru_rs._cli:main\n"
CRATE_FIXTURES = {
    "tests/fixtures/pdf/minimal.pdf",
    "tests/fixtures/vlm/layout.txt",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_model.json",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_middle.json",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_content_list.json",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5_content_list_v2.json",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/2410.21169v5.md",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/images/cc9d646c918053bb628e661ed5772ce1ec4682952a90dc8e687eff8cb42f5df2.jpg",
    "tests/fixtures/official/arxiv_2410.21169v5/vlm/images/c87758e60fb7ba943d6d429071e045b3ea6c5305534d4799a5797960ea34699e.jpg",
}


def fail(message: str) -> typing.NoReturn:
    raise SystemExit(f"release verification failed: {message}")


def load_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as f:
        return json.load(f)


def repository_url(value: object) -> str:
    if isinstance(value, str):
        url = value
    elif isinstance(value, dict):
        mapping = typing.cast(dict[str, object], value)
        candidate = mapping.get("url")
        if not isinstance(candidate, str):
            fail(f"invalid repository metadata: {value!r}")
        url = candidate
    else:
        fail(f"invalid repository metadata: {value!r}")
    return url.removeprefix("git+").removesuffix(".git")


def only_files(directory: Path, suffix: str) -> list[Path]:
    files = sorted(p for p in directory.rglob("*") if p.is_file())
    selected = [p for p in files if p.name.endswith(suffix)]
    if files != selected:
        fail(f"unexpected files in {directory}: {[str(p) for p in files if p not in selected]}")
    return selected


def validate_node_root(npm: dict, lock: dict, version: str) -> None:
    if npm.get("name") != NPM_ROOT or npm.get("version") != version:
        fail("npm root name/version differs")
    if npm.get("license") != LICENSE or repository_url(npm.get("repository")) != REPOSITORY:
        fail("npm root repository/license differs")
    if (
        "dependencies" in npm
        or "optionalDependencies" in npm
        or npm.get("devDependencies") != NPM_DEV_DEPENDENCIES
        or npm.get("main") != "api.js"
        or npm.get("types") != "api.d.ts"
        or npm.get("bin") != NODE_ROOT_BIN
        or npm.get("files") != NPM_ROOT_FILES
        or any(field in npm for field in ("os", "cpu", "libc", "exports"))
    ):
        fail("npm root facade/dependency/platform metadata differs")
    packages = lock.get("packages")
    lock_root = packages.get("") if isinstance(packages, dict) else None
    if not isinstance(lock_root, dict):
        fail("npm lockfile has no valid root package")
    if (lock.get("name"), lock.get("version"), lock_root.get("name"), lock_root.get("version")) != (
        NPM_ROOT,
        version,
        NPM_ROOT,
        version,
    ):
        fail("both npm lockfile root name/version fields must match")
    if (
        "dependencies" in lock_root
        or "optionalDependencies" in lock_root
        or lock_root.get("devDependencies") != npm["devDependencies"]
        or lock_root.get("bin") != npm["bin"]
    ):
        fail("npm lockfile root dependency/bin metadata differs")


def check_identity(args: argparse.Namespace) -> None:
    tag = args.tag
    match = TAG_RE.fullmatch(tag)
    if not match:
        fail(f"release tag is not a plain stable vX.Y.Z: {tag!r}")
    version = tag[1:]
    if args.draft == "true" or args.prerelease == "true":
        fail("draft and prerelease releases cannot publish")
    if args.ref != f"refs/tags/{tag}":
        fail(f"event ref {args.ref!r} does not exactly match release tag")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    peeled = subprocess.check_output(["git", "rev-parse", "--verify", f"{tag}^{{commit}}"], text=True).strip()
    if len(args.sha) != 40 or head != args.sha or peeled != args.sha:
        fail(f"release identity differs: tag={peeled}, HEAD={head}, event={args.sha}")

    metadata = load_json(args.metadata)
    packages = {p["name"]: p for p in metadata["packages"]}
    if set(packages) != {"mineru", "mineru-python", "mineru-node"}:
        fail(f"unexpected Cargo workspace packages: {sorted(packages)}")
    for name, package in packages.items():
        if package["version"] != version:
            fail(f"{name} version {package['version']} != {version}")
        if package.get("repository") != REPOSITORY or package.get("license") != LICENSE:
            fail(f"{name} repository/license metadata differs")
        for target in package["targets"]:
            if target["name"] == "mineru-cli":
                fail("forbidden mineru-cli target exists")
    root = packages["mineru"]
    bins = {t["name"] for t in root["targets"] if "bin" in t["kind"]}
    libs = {t["name"] for t in root["targets"] if "lib" in t["kind"]}
    if bins != BINS or libs != {"mineru"}:
        fail(f"unexpected root targets: bins={sorted(bins)}, libs={sorted(libs)}")
    binding_targets = {
        name: {target["name"] for target in packages[name]["targets"] if "cdylib" in target["kind"]}
        for name in ("mineru-python", "mineru-node")
    }
    if binding_targets != {"mineru-python": {"mineru_rs"}, "mineru-node": {"mineru_node"}}:
        fail(f"unexpected binding targets: {binding_targets}")

    npm = load_json(args.root / "bindings/node/package.json")
    lock = load_json(args.root / "bindings/node/package-lock.json")
    validate_node_root(npm, lock, version)
    with (args.root / "bindings/python/pyproject.toml").open("rb") as f:
        if tomllib is None:
            fail("TOML verification requires Python 3.11+")
        pyproject = tomllib.load(f)
    project = pyproject.get("project", {})
    maturin = pyproject.get("tool", {}).get("maturin", {})
    if (
        project.get("name") != "mineru-rs"
        or project.get("license") != LICENSE
        or project.get("urls", {}).get("Repository") != REPOSITORY
        or project.get("dynamic") != ["version"]
        or project.get("scripts") != PYTHON_SCRIPTS
        or maturin.get("module-name") != "mineru_rs._native"
        or maturin.get("python-source") != "python"
    ):
        fail("PyPI distribution/module metadata differs")
    print(version)


def archive_contents(path: Path) -> tuple[str, dict[str, bytes], dict[str, int]]:
    with tarfile.open(path, "r:gz") as archive:
        members = archive.getmembers()
        normalized = []
        seen = set()
        for member in members:
            pure = PurePosixPath(member.name)
            if pure.is_absolute() or ".." in pure.parts or not pure.parts or member.issym() or member.islnk():
                fail(f"unsafe tar member {member.name!r}")
            name = pure.as_posix()
            if name in seen:
                fail(f"duplicate normalized tar member {name!r} in {path}")
            seen.add(name)
            normalized.append((member, pure))
        roots = {pure.parts[0] for _, pure in normalized}
        if len(roots) != 1:
            fail(f"archive {path} must have one root directory")
        root = roots.pop()
        files: dict[str, bytes] = {}
        modes: dict[str, int] = {}
        for member, pure in normalized:
            if member.isdir():
                continue
            if not member.isfile():
                fail(f"unexpected non-file tar member {member.name!r}")
            if len(pure.parts) == 1:
                fail(f"archive root must be a directory: {member.name!r}")
            stream = archive.extractfile(member)
            if stream is None:
                fail(f"cannot read tar member {member.name!r}")
            relative = PurePosixPath(*pure.parts[1:]).as_posix()
            files[relative] = stream.read()
            modes[relative] = member.mode & 0o777
    return root, files, modes


def archive_files(path: Path) -> tuple[str, dict[str, bytes]]:
    root, files, _ = archive_contents(path)
    return root, files


def expected_crate_files(source: Path) -> set[str]:
    expected = {
        "Cargo.toml",
        "Cargo.toml.orig",
        "Cargo.lock",
        "README.md",
        "LICENSE-MIT",
        "LICENSE-APACHE",
        "docs/usage.md",
        "docs/usage.en.md",
        "docs/compatibility.md",
    }
    # Cargo emits this generated file only when the source has a VCS revision.
    has_revision = subprocess.run(
        ["git", "-C", str(source), "rev-parse", "--verify", "HEAD"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    ).returncode == 0
    if has_revision:
        expected.add(".cargo_vcs_info.json")
    expected.update(p.relative_to(source).as_posix() for p in (source / "src").rglob("*") if p.is_file())
    expected.update(CRATE_FIXTURES)
    return expected


def validate_crate(path: Path, version: str, source: Path) -> tuple[str, dict[str, bytes]]:
    root, files = archive_files(path)
    if root != f"mineru-{version}":
        fail(f"crate root {root!r} != mineru-{version}")
    expected = expected_crate_files(source)
    if set(files) != expected:
        fail(f"crate contents differ; missing={sorted(expected - set(files))}, unexpected={sorted(set(files) - expected)}")
    if ".cargo_vcs_info.json" in files:
        vcs = json.loads(files[".cargo_vcs_info.json"])
        revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
        if vcs.get("git", {}).get("sha1") != revision:
            fail("crate VCS revision differs from checked-out source")
    if tomllib is None:
        fail("TOML verification requires Python 3.11+")
    manifest = tomllib.loads(files["Cargo.toml"].decode())
    package = manifest.get("package", {})
    if (package.get("name"), package.get("version"), package.get("license"), package.get("repository")) != (
        "mineru",
        version,
        LICENSE,
        REPOSITORY,
    ):
        fail("normalized Cargo.toml identity differs")
    bins = {entry.get("name") for entry in manifest.get("bin", [])}
    if bins != BINS or "mineru-cli" in bins:
        fail(f"normalized Cargo.toml binaries differ: {sorted(bins)}")
    if set(package.get("include", [])) != {
        "Cargo.toml", "Cargo.lock", "src/**", "README.md", "LICENSE-MIT", "LICENSE-APACHE",
        "docs/usage.md", "docs/usage.en.md", "docs/compatibility.md", "!tests/fixtures/input/README.md", *CRATE_FIXTURES,
    }:
        fail("normalized Cargo.toml include policy differs")
    return root, files


def crate_path(directory: Path, version: str) -> Path:
    expected = directory / f"mineru-{version}.crate"
    files = []
    unexpected = []
    for path in sorted(directory.iterdir()):
        if path.is_symlink():
            unexpected.append(path)
        elif path.is_dir():
            continue
        elif path.is_file():
            files.append(path)
        else:
            unexpected.append(path)
    if files != [expected] or unexpected:
        fail(f"expected only immediate crate {expected}; files={files}, unexpected={unexpected}")
    return expected


def check_crate(args: argparse.Namespace) -> None:
    path = args.crate or crate_path(args.directory, args.version)
    root, files = validate_crate(path, args.version, args.source)
    if args.compare:
        _, other = validate_crate(args.compare, args.version, args.source)
        hashes = {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}
        other_hashes = {name: hashlib.sha256(data).hexdigest() for name, data in other.items()}
        if hashes != other_hashes:
            fail("recreated crate paths/content hashes differ from reference crate")
    if args.extract:
        destination = args.extract / root
        if destination.exists():
            shutil.rmtree(destination)
        for name, data in files.items():
            out = destination / name
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(data)
    print(path)


def zip_contents(path: Path) -> tuple[dict[str, bytes], dict[str, int]]:
    with zipfile.ZipFile(path) as archive:
        files = {}
        modes = {}
        seen = set()
        for member in archive.infolist():
            pure = PurePosixPath(member.filename)
            if pure.is_absolute() or ".." in pure.parts or not pure.parts:
                fail(f"unsafe ZIP member {member.filename!r}")
            name = pure.as_posix()
            if name in seen:
                fail(f"duplicate normalized ZIP member {name!r} in {path}")
            seen.add(name)
            if not member.is_dir():
                files[name] = archive.read(member)
                modes[name] = (member.external_attr >> 16) & 0o777
        return files, modes


def zip_files(path: Path) -> dict[str, bytes]:
    files, _ = zip_contents(path)
    return files


def expected_wheel_tags(platform: str) -> set[str]:
    return {f"cp39-abi3-{part}" for part in platform.split(".")}


def validate_wheel(path: Path, version: str) -> str:
    match = re.fullmatch(rf"mineru_rs-{re.escape(version)}-cp39-abi3-(.+)\.whl", path.name)
    if not match:
        fail(f"wheel filename is not mineru_rs-{version}-cp39-abi3: {path.name}")
    platform = match.group(1)
    if any(part.startswith("linux_") for part in platform.split(".")):
        fail(f"raw linux wheel is forbidden: {path.name}")
    files, modes = zip_contents(path)
    metadata_names = [n for n in files if n.endswith(".dist-info/METADATA")]
    wheel_names = [n for n in files if n.endswith(".dist-info/WHEEL")]
    if len(metadata_names) != 1 or len(wheel_names) != 1:
        fail(f"wheel must contain exactly one METADATA and WHEEL: {path.name}")
    metadata = email.parser.BytesParser().parsebytes(files[metadata_names[0]])
    if metadata.get("Name") != "mineru-rs" or metadata.get("Version") != version:
        fail(f"wheel METADATA identity differs: {path.name}")
    wheel = email.parser.BytesParser().parsebytes(files[wheel_names[0]])
    tags = wheel.get_all("Tag", [])
    if set(tags) != expected_wheel_tags(platform):
        fail(f"wheel tags are not exclusively cp39-abi3: {tags}")
    native = [name for name in files if re.fullmatch(r"mineru_rs/_native(?:\.abi3)?\.(?:so|pyd)", name)]
    windows = "win_" in platform
    helper = f"mineru_rs/mineru-office-convert{'.exe' if windows else ''}"
    package_files = {name for name in files if name.startswith("mineru_rs/")}
    expected_package = {"mineru_rs/__init__.py", "mineru_rs/_cli.py", helper, *native}
    if len(native) != 1 or package_files != expected_package:
        fail(f"wheel mixed package payload differs in {path.name}: {sorted(package_files)}")
    dist_info = f"mineru_rs-{version}.dist-info"
    expected_metadata = {
        f"{dist_info}/METADATA",
        f"{dist_info}/WHEEL",
        f"{dist_info}/entry_points.txt",
        f"{dist_info}/RECORD",
        f"{dist_info}/sboms/mineru-python.cyclonedx.json",
    }
    if set(files) - package_files != expected_metadata:
        fail(f"wheel metadata payload differs in {path.name}")
    if files[f"{dist_info}/entry_points.txt"] != WHEEL_ENTRY_POINTS:
        fail(f"wheel console entry point differs in {path.name}")
    if not windows and modes.get(helper) != 0o755:
        fail(f"wheel helper mode is not 0755 in {path.name}")
    return platform


def check_wheels(args: argparse.Namespace) -> None:
    files = only_files(args.directory, ".whl")
    if len(files) != args.count:
        fail(f"expected {args.count} wheels, found {len(files)}")
    platforms = {path: validate_wheel(path, args.version) for path in files}
    required = WHEEL_SLOTS if args.platform == "all" else {args.platform: WHEEL_SLOTS[args.platform]}
    for slot, marker in required.items():
        matching = [path for path, platform in platforms.items() if marker in platform]
        if len(matching) != 1:
            fail(f"expected exactly one {slot} wheel, found {[p.name for p in matching]}")
    print("\n".join(str(path) for path in files))


def local_wheel_hashes(directory: Path, project: str, version: str) -> dict[str, str]:
    if not directory.is_dir() or directory.is_symlink():
        fail(f"wheel directory is not a real directory: {directory}")
    files = only_files(directory, ".whl")
    distribution = re.sub(r"[-_.]+", "_", project).lower()
    wheel_re = re.compile(
        rf"{re.escape(distribution)}-{re.escape(version)}-(?:[0-9][0-9A-Za-z_.]*-)?[^-]+-[^-]+-[^-]+\.whl\Z"
    )
    if not files:
        fail(f"no wheels found in {directory}")
    if any(path.parent != directory or path.is_symlink() or not wheel_re.fullmatch(path.name) for path in files):
        fail(f"unexpected wheel path or filename in {directory}: {[str(path) for path in files]}")
    hashes = {path.name: hashlib.sha256(path.read_bytes()).hexdigest() for path in files}
    if len(hashes) != len(files):
        fail("duplicate local wheel filenames")
    return hashes


def parse_pypi_files(payload: object) -> dict[str, str]:
    if not isinstance(payload, dict):
        fail("malformed PyPI JSON: root must be an object")
    root = typing.cast(dict[str, object], payload)
    urls = root.get("urls")
    if not isinstance(urls, list):
        fail("malformed PyPI JSON: urls must be a list")
    files: dict[str, str] = {}
    for entry in urls:
        if not isinstance(entry, dict):
            fail("malformed PyPI JSON: URL entry must be an object")
        url_entry = typing.cast(dict[str, object], entry)
        filename = url_entry.get("filename")
        digests = url_entry.get("digests")
        digest = typing.cast(dict[str, object], digests).get("sha256") if isinstance(digests, dict) else None
        if (
            not isinstance(filename, str)
            or not filename
            or Path(filename).name != filename
            or not isinstance(digest, str)
            or not SHA256_RE.fullmatch(digest)
        ):
            fail("malformed PyPI filename or SHA-256 digest")
        if filename in files:
            fail(f"duplicate PyPI filename: {filename}")
        files[filename] = digest
    return files


def pypi_missing(local: dict[str, str], remote: dict[str, str]) -> set[str]:
    extra = set(remote) - set(local)
    if extra:
        fail(f"PyPI has unexpected files: {sorted(extra)}")
    mismatched = sorted(name for name in remote if remote[name] != local[name])
    if mismatched:
        fail(f"PyPI SHA-256 mismatch: {mismatched}")
    return set(local) - set(remote)


def require_pypi_postflight(local: dict[str, str], remote: dict[str, str]) -> None:
    missing = pypi_missing(local, remote)
    if missing:
        fail(f"PyPI is missing files: {sorted(missing)}")


def pypi_release_files(project: str, version: str) -> tuple[int, dict[str, str] | None, int | None]:
    url = "https://pypi.org/pypi/{}/{}/json".format(
        urllib.parse.quote(project, safe=""), urllib.parse.quote(version, safe="")
    )
    request = urllib.request.Request(url, headers={"User-Agent": PYPI_USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            status = response.status
            retry_after = response.headers.get("Retry-After")
            if status != 200:
                if status == 429 or 500 <= status < 600:
                    return status, None, int(retry_after) if retry_after and retry_after.strip().isdigit() else None
                fail(f"unexpected PyPI HTTP status {status}")
            try:
                payload = json.loads(response.read().decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                fail(f"malformed PyPI JSON: {error}")
            return 200, parse_pypi_files(payload), None
    except urllib.error.HTTPError as error:
        retry_after = error.headers.get("Retry-After") if error.headers else None
        if error.code == 404 or error.code == 429 or 500 <= error.code < 600:
            return error.code, None, int(retry_after) if retry_after and retry_after.strip().isdigit() else None
        fail(f"unexpected PyPI HTTP status {error.code}")
    except (urllib.error.URLError, TimeoutError, OSError):
        return 0, None, None


def retry_pypi(delay: int | None, fallback: int) -> None:
    time.sleep(min(delay, 30) if delay is not None else fallback)


def rebuild_missing_directory(local_directory: Path, missing_directory: Path, missing: set[str]) -> None:
    local = local_directory.resolve()
    destination = missing_directory.resolve()
    if local == destination or local in destination.parents or destination in local.parents:
        fail("missing directory must not overlap the local wheel directory")
    if missing_directory.is_symlink():
        fail("missing directory must not be a symlink")
    if missing_directory.exists():
        if not missing_directory.is_dir():
            fail("missing directory exists and is not a directory")
        shutil.rmtree(missing_directory)
    missing_directory.mkdir(parents=True)
    for filename in sorted(missing):
        shutil.copy2(local_directory / filename, missing_directory / filename)


def pypi_preflight(args: argparse.Namespace) -> None:
    local = local_wheel_hashes(args.directory, args.project, args.version)
    remote: dict[str, str] | None = None
    last_status = 0
    for attempt in range(4):
        status, fetched, retry_after = pypi_release_files(args.project, args.version)
        last_status = status
        if status == 404:
            remote = {}
            break
        if status == 200:
            remote = typing.cast(dict[str, str], fetched)
            break
        if attempt < 3:
            retry_pypi(retry_after, 2**attempt)
    if remote is None:
        fail(f"PyPI preflight unavailable after 4 attempts; last status={last_status or 'network error'}")
    missing = pypi_missing(local, remote)
    rebuild_missing_directory(args.directory, args.missing_directory, missing)
    print(f"PyPI preflight: already={len(local) - len(missing)} missing={len(missing)}")


def pypi_postflight(args: argparse.Namespace) -> None:
    local = local_wheel_hashes(args.directory, args.project, args.version)
    last_remote: dict[str, str] | None = None
    last_status = 0
    for attempt in range(8):
        status, remote, retry_after = pypi_release_files(args.project, args.version)
        last_status = status
        if status == 200:
            last_remote = typing.cast(dict[str, str], remote)
            missing = pypi_missing(local, last_remote)
            if not missing:
                print(f"PyPI postflight: verified={len(local)}")
                return
        if attempt < 7:
            retry_pypi(retry_after, 2**attempt)
    if last_remote is not None:
        require_pypi_postflight(local, last_remote)
    fail(f"PyPI postflight unavailable after 8 attempts; last status={last_status or 'network error'}")


def expected_platform_manifest(root: dict, suffix: str, version: str) -> dict:
    os_name, cpu, libc, _ = PLATFORMS[suffix]
    native = f"mineru.{suffix}.node"
    helper = f"mineru-office-convert{'.exe' if os_name == 'win32' else ''}"
    manifest: dict[str, typing.Any] = {
        "name": f"{NPM_ROOT}-{suffix}",
        "version": version,
        "cpu": [cpu],
        "main": native,
        "files": [native, helper],
    }
    for field in ("description", "keywords", "author", "authors", "homepage", "license", "engines", "repository", "bugs"):
        if field in root:
            manifest[field] = root[field]
    if "publishConfig" in root:
        manifest["publishConfig"] = {
            key: root["publishConfig"][key] for key in ("registry", "access") if key in root["publishConfig"]
        }
    manifest["os"] = [os_name]
    if libc:
        manifest["libc"] = [libc]
    manifest["exports"] = {
        ".": f"./{native}",
        "./helper": f"./{helper}",
        "./package.json": "./package.json",
    }
    return manifest


def validate_npm(path: Path, version: str, root_manifest: dict) -> str:
    root, raw, modes = archive_contents(path)
    if root != "package" or "package.json" not in raw:
        fail(f"npm tarball {path} has invalid root")
    manifest = json.loads(raw["package.json"])
    name = manifest.get("name")
    if manifest.get("version") != version or manifest.get("license") != LICENSE:
        fail(f"npm identity differs in {path.name}")
    if repository_url(manifest.get("repository")) != REPOSITORY:
        fail(f"npm repository differs in {path.name}")
    if isinstance(name, str):
        expected_filename = f"{name.removeprefix('@').replace('/', '-')}-{version}.tgz"
        if path.name != expected_filename:
            fail(f"npm tarball filename {path.name!r} != {expected_filename!r}")
    if name == NPM_ROOT:
        if any(field in manifest for field in ("os", "cpu", "libc")):
            fail("root npm package must not have platform constraints")
        expected = {f"{NPM_ROOT}-{suffix}": version for suffix in PLATFORMS}
        if (
            manifest.get("main") != "api.js"
            or manifest.get("types") != "api.d.ts"
            or manifest.get("bin") != NODE_ROOT_BIN
            or manifest.get("files") != NPM_ROOT_FILES
            or manifest.get("optionalDependencies") != expected
            or any(field in manifest for field in ("os", "cpu", "libc", "exports"))
        ):
            fail("npm root facade/files/bin/platform metadata differs")
        if set(raw) != {"package.json", *NPM_ROOT_FILES}:
            fail(f"npm root payload differs: {sorted(raw)}")
        bin_path = "bin/mineru.js"
        if not raw[bin_path].startswith(b"#!/usr/bin/env node\n") or modes.get(bin_path) != 0o755:
            fail("npm root bin shebang/mode differs")
        return name
    if not isinstance(name, str) or not name.startswith(f"{NPM_ROOT}-"):
        fail(f"unexpected npm package name {name!r}")
    suffix = name.removeprefix(f"{NPM_ROOT}-")
    if suffix not in PLATFORMS:
        fail(f"unexpected npm platform package {name}")
    expected = expected_platform_manifest(root_manifest, suffix, version)
    if manifest != expected or "bin" in manifest:
        fail(f"npm platform manifest differs for {name}")
    native, helper = expected["files"]
    if set(raw) != {"package.json", native, helper}:
        fail(f"npm platform payload differs for {name}: {sorted(raw)}")
    if PLATFORMS[suffix][0] != "win32" and modes.get(helper) != 0o755:
        fail(f"npm platform helper mode differs for {name}")
    return name


def check_npm(args: argparse.Namespace) -> None:
    files = only_files(args.directory, ".tgz")
    if len(files) != 7:
        fail(f"expected exactly seven npm tarballs, found {len(files)}")
    manifests = []
    for path in files:
        _, raw = archive_files(path)
        if "package.json" not in raw:
            fail(f"npm tarball {path} has no package.json")
        manifests.append(json.loads(raw["package.json"]))
    roots = [manifest for manifest in manifests if manifest.get("name") == NPM_ROOT]
    if len(roots) != 1:
        fail("npm package set must contain exactly one root manifest")
    names = [validate_npm(path, args.version, roots[0]) for path in files]
    expected = {NPM_ROOT, *(f"{NPM_ROOT}-{suffix}" for suffix in PLATFORMS)}
    if len(names) != len(set(names)) or set(names) != expected:
        fail(f"npm package set differs: {sorted(names)}")
    print("\n".join(str(path) for path in files))


def check_node_native(args: argparse.Namespace) -> None:
    expected = args.directory / f"mineru.{args.suffix}.node"
    files = sorted(args.directory.glob("*.node"))
    if files != [expected] or expected.stat().st_size == 0:
        fail(f"expected one nonempty native artifact {expected}, found {files}")
    print(expected)


def self_test(_: argparse.Namespace) -> None:
    assert TAG_RE.fullmatch("v0.1.0") and not TAG_RE.fullmatch("v01.1.0")
    assert repository_url({"url": "git+https://github.com/agentsyaml/mineru-rs.git"}) == REPOSITORY
    assert expected_wheel_tags("manylinux_2_17_aarch64.manylinux2014_aarch64") == {
        "cp39-abi3-manylinux_2_17_aarch64", "cp39-abi3-manylinux2014_aarch64",
    }
    with tempfile.TemporaryDirectory() as tmp:
        def must_fail(call: typing.Callable[[], object], message: str) -> None:
            try:
                call()
            except SystemExit:
                return
            raise AssertionError(message)

        def write_wheel(path: Path, helper_mode: int = 0o755, entry_point: bytes = WHEEL_ENTRY_POINTS) -> None:
            dist = "mineru_rs-1.2.3.dist-info"
            contents = {
                "mineru_rs/__init__.py": b"",
                "mineru_rs/_cli.py": b"",
                "mineru_rs/_native.abi3.so": b"native",
                "mineru_rs/mineru-office-convert": b"helper",
                f"{dist}/METADATA": b"Name: mineru-rs\nVersion: 1.2.3\n",
                f"{dist}/WHEEL": b"Wheel-Version: 1.0\nTag: cp39-abi3-macosx_11_0_arm64\n",
                f"{dist}/entry_points.txt": entry_point,
                f"{dist}/RECORD": b"",
                f"{dist}/sboms/mineru-python.cyclonedx.json": b"{}",
            }
            with zipfile.ZipFile(path, "w") as archive:
                for name, data in contents.items():
                    member = zipfile.ZipInfo(name)
                    mode = helper_mode if name.endswith("mineru-office-convert") else 0o644
                    member.external_attr = mode << 16
                    archive.writestr(member, data)

        def write_npm_tar(path: Path, manifest: dict, payload: dict[str, bytes], modes: dict[str, int]) -> None:
            with tarfile.open(path, "w:gz") as archive:
                for name, data in {"package.json": json.dumps(manifest).encode(), **payload}.items():
                    member = tarfile.TarInfo(f"package/{name}")
                    member.size = len(data)
                    member.mode = modes.get(name, 0o644)
                    archive.addfile(member, io.BytesIO(data))

        version = "1.2.3"
        optional = {f"{NPM_ROOT}-{suffix}": version for suffix in PLATFORMS}
        identity_npm = {
            "name": NPM_ROOT,
            "version": version,
            "license": LICENSE,
            "repository": {"url": f"git+{REPOSITORY}.git"},
            "main": "api.js",
            "types": "api.d.ts",
            "bin": NODE_ROOT_BIN,
            "files": NPM_ROOT_FILES,
            "devDependencies": NPM_DEV_DEPENDENCIES,
        }
        identity_lock = {
            "name": NPM_ROOT,
            "version": version,
            "packages": {"": {
                "name": NPM_ROOT,
                "version": version,
                "bin": identity_npm["bin"],
                "devDependencies": NPM_DEV_DEPENDENCIES,
            }},
        }
        validate_node_root(identity_npm, identity_lock, version)
        for alias in NODE_ROOT_BIN:
            bad_npm = json.loads(json.dumps(identity_npm))
            del bad_npm["bin"][alias]
            must_fail(
                lambda bad_npm=bad_npm: validate_node_root(bad_npm, identity_lock, version),
                f"root npm package without {alias} alias was accepted",
            )
        bad_npm = json.loads(json.dumps(identity_npm))
        bad_npm["dependencies"] = {}
        must_fail(lambda: validate_node_root(bad_npm, identity_lock, version), "root runtime dependencies were accepted")
        bad_npm = json.loads(json.dumps(identity_npm))
        bad_npm["optionalDependencies"] = optional
        must_fail(lambda: validate_node_root(bad_npm, identity_lock, version), "source optional dependencies were accepted")
        bad_lock = json.loads(json.dumps(identity_lock))
        bad_lock["packages"][""]["dependencies"] = {}
        must_fail(lambda: validate_node_root(identity_npm, bad_lock, version), "lock root dependencies were accepted")
        bad_lock = json.loads(json.dumps(identity_lock))
        bad_lock["packages"][""]["optionalDependencies"] = optional
        must_fail(lambda: validate_node_root(identity_npm, bad_lock, version), "lock optional dependencies were accepted")
        bad_lock = json.loads(json.dumps(identity_lock))
        bad_lock["packages"][""]["devDependencies"] = {"@napi-rs/cli": "wrong"}
        must_fail(lambda: validate_node_root(identity_npm, bad_lock, version), "lock devDependency mismatch was accepted")

        local_hashes = {"one.whl": "a" * 64, "two.whl": "b" * 64}
        assert pypi_missing(local_hashes, {}) == set(local_hashes)
        assert pypi_missing(local_hashes, {"one.whl": "a" * 64}) == {"two.whl"}
        must_fail(
            lambda: pypi_missing(local_hashes, {"one.whl": "c" * 64}),
            "PyPI hash mismatch was accepted",
        )
        must_fail(
            lambda: pypi_missing(local_hashes, {"extra.whl": "c" * 64}),
            "extra PyPI file was accepted",
        )
        must_fail(
            lambda: require_pypi_postflight(local_hashes, {"one.whl": "a" * 64}),
            "incomplete PyPI postflight was accepted",
        )
        require_pypi_postflight(local_hashes, local_hashes)
        assert parse_pypi_files({"urls": [{"filename": "one.whl", "digests": {"sha256": "a" * 64}}]}) == {
            "one.whl": "a" * 64
        }
        must_fail(
            lambda: parse_pypi_files({"urls": [{"filename": "one.whl", "digests": {"sha256": "A" * 64}}]}),
            "malformed PyPI digest was accepted",
        )

        pypi_dir = Path(tmp) / "pypi"
        pypi_dir.mkdir()
        pypi_wheel = pypi_dir / "mineru_rs-1.2.3-cp39-abi3-any.whl"
        pypi_wheel.write_bytes(b"wheel")
        assert local_wheel_hashes(pypi_dir, "mineru-rs", version) == {
            pypi_wheel.name: hashlib.sha256(b"wheel").hexdigest()
        }

        package = Path(tmp) / "package"
        package.mkdir()
        expected_crate = package / "mineru-1.2.3.crate"
        expected_crate.write_bytes(b"crate")
        (package / "mineru-1.2.3").mkdir()
        (package / "mineru-1.2.3/Cargo.toml").write_text("[package]\n")
        assert crate_path(package, "1.2.3") == expected_crate
        unexpected = package / "unexpected.txt"
        unexpected.write_text("no")
        try:
            crate_path(package, "1.2.3")
        except SystemExit:
            pass
        else:
            raise AssertionError("unexpected immediate crate-package file was accepted")

        duplicate_zip = Path(tmp) / "duplicate.whl"
        with zipfile.ZipFile(duplicate_zip, "w") as archive:
            archive.writestr("same", b"a")
            archive.writestr("./same", b"b")
        try:
            zip_files(duplicate_zip)
        except SystemExit:
            pass
        else:
            raise AssertionError("duplicate normalized ZIP member was accepted")

        duplicate_tar = Path(tmp) / "duplicate.tar.gz"
        with tarfile.open(duplicate_tar, "w:gz") as archive:
            for name in ("root/same", "./root/same"):
                member = tarfile.TarInfo(name)
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))
        try:
            archive_files(duplicate_tar)
        except SystemExit:
            pass
        else:
            raise AssertionError("duplicate normalized tar member was accepted")

        wheel_path = Path(tmp) / "mineru_rs-1.2.3-cp39-abi3-macosx_11_0_arm64.whl"
        write_wheel(wheel_path)
        assert validate_wheel(wheel_path, "1.2.3") == "macosx_11_0_arm64"
        write_wheel(wheel_path, 0o644)
        must_fail(lambda: validate_wheel(wheel_path, "1.2.3"), "non-executable wheel helper was accepted")
        for alias, target in PYTHON_SCRIPTS.items():
            write_wheel(wheel_path, entry_point=WHEEL_ENTRY_POINTS.replace(f"{alias}={target}\n".encode(), b""))
            must_fail(
                lambda: validate_wheel(wheel_path, "1.2.3"),
                f"wheel without {alias} console alias was accepted",
            )

        npm_dir = Path(tmp) / "npm"
        npm_dir.mkdir()
        root_manifest = {
            "name": NPM_ROOT,
            "version": version,
            "description": "Native Node.js helpers for MinerU Rust",
            "main": "api.js",
            "types": "api.d.ts",
            "bin": NODE_ROOT_BIN,
            "license": LICENSE,
            "repository": {"type": "git", "url": f"git+{REPOSITORY}.git"},
            "bugs": {"url": f"{REPOSITORY}/issues"},
            "files": NPM_ROOT_FILES,
            "optionalDependencies": optional,
            "publishConfig": {"access": "public", "registry": "https://registry.npmjs.org/"},
            "engines": {"node": ">=18"},
        }
        root_payload = {name: (b"#!/usr/bin/env node\n" if name == "bin/mineru.js" else b"x") for name in NPM_ROOT_FILES}
        root_path = npm_dir / f"alexsun-top-mineru-{version}.tgz"
        write_npm_tar(root_path, root_manifest, root_payload, {"bin/mineru.js": 0o755})
        for suffix, platform_config in PLATFORMS.items():
            os_name = platform_config[0]
            manifest = expected_platform_manifest(root_manifest, suffix, version)
            native, helper = manifest["files"]
            platform_path = npm_dir / f"alexsun-top-mineru-{suffix}-{version}.tgz"
            write_npm_tar(
                platform_path,
                manifest,
                {native: b"native", helper: b"helper"},
                {helper: 0o644 if os_name == "win32" else 0o755},
            )
        check_npm(argparse.Namespace(directory=npm_dir, version=version))

        bad_root_path = Path(tmp) / f"alexsun-top-mineru-{version}.tgz"
        write_npm_tar(bad_root_path, root_manifest, root_payload, {"bin/mineru.js": 0o644})
        must_fail(
            lambda: validate_npm(bad_root_path, version, root_manifest),
            "non-executable npm root bin was accepted",
        )
        for alias in NODE_ROOT_BIN:
            bad_root_manifest = json.loads(json.dumps(root_manifest))
            del bad_root_manifest["bin"][alias]
            write_npm_tar(bad_root_path, bad_root_manifest, root_payload, {"bin/mineru.js": 0o755})
            must_fail(
                lambda: validate_npm(bad_root_path, version, root_manifest),
                f"packed npm root without {alias} alias was accepted",
            )

        bad_dir = Path(tmp) / "bad-npm"
        bad_dir.mkdir()
        suffix = "darwin-arm64"
        bad_manifest = expected_platform_manifest(root_manifest, suffix, version)
        bad_manifest["exports"]["./extra"] = "./extra"
        native, helper = bad_manifest["files"]
        bad_path = bad_dir / f"alexsun-top-mineru-{suffix}-{version}.tgz"
        write_npm_tar(bad_path, bad_manifest, {native: b"native", helper: b"helper"}, {helper: 0o755})
        must_fail(
            lambda: validate_npm(bad_path, version, root_manifest),
            "extra npm platform export was accepted",
        )
    print("verify_release self-test passed")


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="mode", required=True)
    identity = sub.add_parser("identity")
    identity.add_argument("--root", type=Path, default=Path("."))
    identity.add_argument("--metadata", type=Path, required=True)
    identity.add_argument("--tag", required=True)
    identity.add_argument("--ref", required=True)
    identity.add_argument("--sha", required=True)
    identity.add_argument("--draft", choices=("true", "false"), required=True)
    identity.add_argument("--prerelease", choices=("true", "false"), required=True)
    identity.set_defaults(func=check_identity)

    crate = sub.add_parser("crate")
    choice = crate.add_mutually_exclusive_group(required=True)
    choice.add_argument("--crate", type=Path)
    choice.add_argument("--directory", type=Path)
    crate.add_argument("--version", required=True)
    crate.add_argument("--source", type=Path, default=Path("."))
    crate.add_argument("--compare", type=Path)
    crate.add_argument("--extract", type=Path)
    crate.set_defaults(func=check_crate)

    wheel = sub.add_parser("wheel")
    wheel.add_argument("--directory", type=Path, required=True)
    wheel.add_argument("--version", required=True)
    wheel.add_argument("--count", type=int, required=True)
    wheel.add_argument("--platform", choices=(*WHEEL_SLOTS, "all"), required=True)
    wheel.set_defaults(func=check_wheels)

    preflight = sub.add_parser("pypi-preflight")
    preflight.add_argument("--project", required=True)
    preflight.add_argument("--version", required=True)
    preflight.add_argument("--directory", type=Path, required=True)
    preflight.add_argument("--missing-directory", type=Path, required=True)
    preflight.set_defaults(func=pypi_preflight)

    postflight = sub.add_parser("pypi-postflight")
    postflight.add_argument("--project", required=True)
    postflight.add_argument("--version", required=True)
    postflight.add_argument("--directory", type=Path, required=True)
    postflight.set_defaults(func=pypi_postflight)

    npm = sub.add_parser("npm")
    npm.add_argument("--directory", type=Path, required=True)
    npm.add_argument("--version", required=True)
    npm.set_defaults(func=check_npm)

    native = sub.add_parser("node-native")
    native.add_argument("--directory", type=Path, required=True)
    native.add_argument("--suffix", choices=tuple(PLATFORMS), required=True)
    native.set_defaults(func=check_node_native)

    test = sub.add_parser("self-test")
    test.set_defaults(func=self_test)
    return p


if __name__ == "__main__":
    arguments = parser().parse_args()
    arguments.func(arguments)
