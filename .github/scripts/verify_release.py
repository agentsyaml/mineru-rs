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
import typing
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
WHEEL_SLOTS = {
    "manylinux-x64": "manylinux2014_x86_64",
    "manylinux-arm64": "manylinux2014_aarch64",
    "macos-x64": "macosx_10_12_x86_64",
    "macos-arm64": "macosx_11_0_arm64",
    "windows-x64": "win_amd64",
}
TAG_RE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")


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
    if npm.get("name") != NPM_ROOT or npm.get("version") != version:
        fail("npm root name/version differs")
    if npm.get("license") != LICENSE or repository_url(npm.get("repository")) != REPOSITORY:
        fail("npm root repository/license differs")
    lock_root = lock.get("packages", {}).get("", {})
    if (lock.get("name"), lock.get("version"), lock_root.get("name"), lock_root.get("version")) != (
        NPM_ROOT,
        version,
        NPM_ROOT,
        version,
    ):
        fail("both npm lockfile root name/version fields must match")
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
        or maturin.get("module-name") != "mineru_rs"
    ):
        fail("PyPI distribution/module metadata differs")
    print(version)


def archive_files(path: Path) -> tuple[str, dict[str, bytes]]:
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
            files[PurePosixPath(*pure.parts[1:]).as_posix()] = stream.read()
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
    for base in (source / "src", source / "tests/fixtures"):
        expected.update(p.relative_to(source).as_posix() for p in base.rglob("*") if p.is_file())
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
        "docs/usage.md", "docs/compatibility.md", "tests/fixtures/**",
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


def zip_files(path: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(path) as archive:
        files = {}
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
    files = zip_files(path)
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
    if not any("mineru_rs" in PurePosixPath(name).name and name.endswith((".so", ".pyd")) for name in files):
        fail(f"wheel has no mineru_rs native module: {path.name}")
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


def npm_payload(manifest: dict, files: dict[str, bytes], native: typing.Optional[str]) -> None:
    declared = manifest.get("files")
    expected_declared = [native] if native else ["index.js", "index.d.ts"]
    if declared != expected_declared:
        fail(f"npm files field differs for {manifest.get('name')}: {declared}")
    payload = set(files) - {"package.json"}
    allowed_docs = {name for name in payload if PurePosixPath(name).name.lower().startswith(("readme", "license"))}
    if payload - allowed_docs != set(expected_declared):
        fail(f"npm tarball payload differs for {manifest.get('name')}: {sorted(payload)}")


def validate_npm(path: Path, version: str) -> str:
    root, raw = archive_files(path)
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
        if manifest.get("optionalDependencies") != expected:
            fail("root optionalDependencies are not the exact six native packages")
        npm_payload(manifest, raw, None)
        return name
    if not isinstance(name, str) or not name.startswith(f"{NPM_ROOT}-"):
        fail(f"unexpected npm package name {name!r}")
    suffix = name.removeprefix(f"{NPM_ROOT}-")
    if suffix not in PLATFORMS:
        fail(f"unexpected npm platform package {name}")
    os_name, cpu, libc, _ = PLATFORMS[suffix]
    if manifest.get("os") != [os_name] or manifest.get("cpu") != [cpu]:
        fail(f"npm os/cpu differs for {name}")
    if (manifest.get("libc") if libc else None) != ([libc] if libc else None):
        fail(f"npm libc differs for {name}")
    native = f"mineru.{suffix}.node"
    if manifest.get("main") != native:
        fail(f"npm native main differs for {name}")
    npm_payload(manifest, raw, native)
    return name


def check_npm(args: argparse.Namespace) -> None:
    files = only_files(args.directory, ".tgz")
    if len(files) != 7:
        fail(f"expected exactly seven npm tarballs, found {len(files)}")
    names = [validate_npm(path, args.version) for path in files]
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
