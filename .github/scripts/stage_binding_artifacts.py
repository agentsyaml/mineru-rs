#!/usr/bin/env python3
"""Stage and inspect target-matched Python and npm binding payloads."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import stat
import struct
import sys
from pathlib import Path
from typing import NoReturn


NPM_ROOT = "@alexsun-top/mineru"
TARGETS: dict[str, dict[str, str | None]] = {
    "x86_64-apple-darwin": {
        "suffix": "darwin-x64",
        "os": "darwin",
        "cpu": "x64",
        "libc": None,
        "addon": "mineru.darwin-x64.node",
        "helper": "mineru-office-convert",
    },
    "aarch64-apple-darwin": {
        "suffix": "darwin-arm64",
        "os": "darwin",
        "cpu": "arm64",
        "libc": None,
        "addon": "mineru.darwin-arm64.node",
        "helper": "mineru-office-convert",
    },
    "x86_64-unknown-linux-gnu": {
        "suffix": "linux-x64-gnu",
        "os": "linux",
        "cpu": "x64",
        "libc": "glibc",
        "addon": "mineru.linux-x64-gnu.node",
        "helper": "mineru-office-convert",
    },
    "aarch64-unknown-linux-gnu": {
        "suffix": "linux-arm64-gnu",
        "os": "linux",
        "cpu": "arm64",
        "libc": "glibc",
        "addon": "mineru.linux-arm64-gnu.node",
        "helper": "mineru-office-convert",
    },
    "x86_64-pc-windows-msvc": {
        "suffix": "win32-x64-msvc",
        "os": "win32",
        "cpu": "x64",
        "libc": None,
        "addon": "mineru.win32-x64-msvc.node",
        "helper": "mineru-office-convert.exe",
    },
    "aarch64-pc-windows-msvc": {
        "suffix": "win32-arm64-msvc",
        "os": "win32",
        "cpu": "arm64",
        "libc": None,
        "addon": "mineru.win32-arm64-msvc.node",
        "helper": "mineru-office-convert.exe",
    },
}


class ValidationError(Exception):
    pass


def fail(message: str) -> NoReturn:
    raise ValidationError(message)


def target_config(target: str) -> dict[str, str | None]:
    try:
        return TARGETS[target]
    except KeyError:
        fail(f"unsupported target {target!r}; musl and non-allowlisted targets are rejected")


def regular_file(path: Path, expected_basename: str) -> os.stat_result:
    if path.name != expected_basename:
        fail(f"source basename {path.name!r} != {expected_basename!r}")
    try:
        info = path.lstat()
    except FileNotFoundError:
        fail(f"file does not exist: {path}")
    if not stat.S_ISREG(info.st_mode):
        fail(f"not a regular file (symlinks are rejected): {path}")
    return info


def unix_mode(mode: int, path: Path, *, exact: bool) -> None:
    actual = stat.S_IMODE(mode)
    if exact:
        if actual != 0o755:
            fail(f"Unix executable mode for {path} is {actual:04o}, expected 0755")
    elif actual & 0o111 != 0o111:
        fail(f"Unix executable lacks owner/group/other execute mode: {path} ({actual:04o})")


def macho_cpu(path: Path, expected_cpu: int) -> None:
    thin_magic = {b"\xcf\xfa\xed\xfe": "<", b"\xfe\xed\xfa\xcf": ">"}
    fat_magic = {
        b"\xca\xfe\xba\xbe": (">", False),
        b"\xbe\xba\xfe\xca": ("<", False),
        b"\xca\xfe\xba\xbf": (">", True),
        b"\xbf\xba\xfe\xca": ("<", True),
    }
    size = path.stat().st_size
    with path.open("rb") as binary:
        header = binary.read(8)
        endian = thin_magic.get(header[:4])
        if endian:
            if len(header) < 8 or struct.unpack(f"{endian}I", header[4:8])[0] != expected_cpu:
                fail(f"Mach-O architecture does not match target: {path}")
            return
        fat = fat_magic.get(header[:4])
        if not fat or len(header) < 8:
            fail(f"not a supported 64-bit Mach-O executable: {path}")
        endian, is_64 = fat
        count = struct.unpack(f"{endian}I", header[4:8])[0]
        entry_size = 32 if is_64 else 20
        if not 1 <= count <= 32:
            fail(f"invalid Mach-O universal architecture count in {path}")
        entries = binary.read(count * entry_size)
        if len(entries) != count * entry_size:
            fail(f"truncated Mach-O universal header: {path}")
        for index in range(count):
            entry = entries[index * entry_size : (index + 1) * entry_size]
            cpu = struct.unpack(f"{endian}I", entry[:4])[0]
            if is_64:
                offset, slice_size = struct.unpack(f"{endian}QQ", entry[8:24])
            else:
                offset, slice_size = struct.unpack(f"{endian}II", entry[8:16])
            if cpu != expected_cpu:
                continue
            if slice_size < 8 or offset > size or slice_size > size - offset:
                fail(f"invalid Mach-O universal slice for target in {path}")
            binary.seek(offset)
            slice_header = binary.read(8)
            slice_endian = thin_magic.get(slice_header[:4])
            if not slice_endian or struct.unpack(f"{slice_endian}I", slice_header[4:8])[0] != expected_cpu:
                fail(f"invalid Mach-O universal target slice in {path}")
            return
    fail(f"Mach-O universal binary lacks the target architecture: {path}")


def elf_cpu(path: Path, expected_machine: int) -> None:
    with path.open("rb") as binary:
        header = binary.read(20)
    if (
        len(header) < 20
        or header[:4] != b"\x7fELF"
        or header[4] != 2
        or header[5] != 1
        or header[6] != 1
        or struct.unpack("<H", header[18:20])[0] != expected_machine
    ):
        fail(f"ELF is not little-endian 64-bit target machine {expected_machine}: {path}")


def pe_cpu(path: Path, expected_machine: int) -> None:
    size = path.stat().st_size
    with path.open("rb") as binary:
        dos = binary.read(64)
        if len(dos) < 64 or dos[:2] != b"MZ":
            fail(f"missing PE DOS header: {path}")
        offset = struct.unpack("<I", dos[60:64])[0]
        if offset > size or size - offset < 26:
            fail(f"invalid PE header offset: {path}")
        binary.seek(offset)
        header = binary.read(26)
    if (
        header[:4] != b"PE\0\0"
        or struct.unpack("<H", header[4:6])[0] != expected_machine
        or struct.unpack("<H", header[24:26])[0] != 0x20B
    ):
        fail(f"PE is not a 64-bit target machine {expected_machine:#x}: {path}")


def binary_file(path: Path, target: str, expected_basename: str, *, executable: bool) -> None:
    config = target_config(target)
    info = regular_file(path, expected_basename)
    os_name, cpu = config["os"], config["cpu"]
    if os_name == "darwin":
        macho_cpu(path, 0x01000007 if cpu == "x64" else 0x0100000C)
    elif os_name == "linux":
        elf_cpu(path, 62 if cpu == "x64" else 183)
    else:
        pe_cpu(path, 0x8664 if cpu == "x64" else 0xAA64)
    if executable and os_name != "win32":
        unix_mode(info.st_mode, path, exact=False)


def package_directory(path: Path) -> None:
    if path.exists() or path.is_symlink():
        if path.is_symlink() or not path.is_dir():
            fail(f"package path is not a real directory: {path}")
        return
    path.mkdir(parents=True)


def opposite_helper(helper: str) -> str:
    return "mineru-office-convert" if helper.endswith(".exe") else "mineru-office-convert.exe"


def inspect_python(target: str, package_dir: Path) -> Path:
    config = target_config(target)
    helper = str(config["helper"])
    expected = package_dir / helper
    opposite = package_dir / opposite_helper(helper)
    if opposite.exists() or opposite.is_symlink():
        fail(f"opposite-platform helper is present: {opposite}")
    binary_file(expected, target, helper, executable=True)
    if config["os"] != "win32":
        unix_mode(expected.lstat().st_mode, expected, exact=True)
    return expected


def stage_python(target: str, helper_source: Path, package_dir: Path) -> Path:
    config = target_config(target)
    helper = str(config["helper"])
    binary_file(helper_source, target, helper, executable=True)
    package_directory(package_dir)
    opposite = package_dir / opposite_helper(helper)
    if opposite.exists() or opposite.is_symlink():
        info = opposite.lstat()
        if not stat.S_ISREG(info.st_mode):
            fail(f"refusing to remove non-regular opposite helper: {opposite}")
        opposite.unlink()
    destination = package_dir / helper
    if destination.exists() or destination.is_symlink():
        regular_file(destination, helper)
    temporary = package_dir / f".{helper}.tmp-{os.getpid()}"
    try:
        shutil.copyfile(helper_source, temporary)
        if config["os"] != "win32":
            temporary.chmod(0o755)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)
    return inspect_python(target, package_dir)


def load_root_manifest(path: Path) -> dict:
    regular_file(path, "package.json")
    try:
        root = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot read root npm manifest {path}: {error}")
    if not isinstance(root, dict) or root.get("name") != NPM_ROOT:
        fail(f"root npm manifest name must be {NPM_ROOT!r}: {path}")
    if not isinstance(root.get("version"), str) or not root["version"]:
        fail(f"root npm manifest has no valid version: {path}")
    return root


def npm_manifest(target: str, root: dict) -> dict:
    config = target_config(target)
    addon, helper = str(config["addon"]), str(config["helper"])
    manifest = {
        "name": f"{NPM_ROOT}-{config['suffix']}",
        "version": root["version"],
        "cpu": [config["cpu"]],
        "main": addon,
        "files": [addon, helper],
    }
    for field in (
        "description",
        "keywords",
        "author",
        "authors",
        "homepage",
        "license",
        "engines",
        "repository",
        "bugs",
    ):
        if field in root:
            manifest[field] = root[field]
    if "publishConfig" in root:
        publish = root["publishConfig"]
        if not isinstance(publish, dict):
            fail("root npm publishConfig must be an object")
        manifest["publishConfig"] = {key: publish[key] for key in ("registry", "access") if key in publish}
    manifest["os"] = [config["os"]]
    if config["libc"]:
        manifest["libc"] = [config["libc"]]
    manifest["exports"] = {
        ".": f"./{addon}",
        "./helper": f"./{helper}",
        "./package.json": "./package.json",
    }
    return manifest


def matching_manifest(actual: object, expected: dict, path: Path) -> None:
    if actual != expected:
        fail(f"npm platform manifest differs from the exact expected manifest: {path}")


def inspect_npm(target: str, package_dir: Path, root_package_json: Path) -> Path:
    config = target_config(target)
    root = load_root_manifest(root_package_json)
    if package_dir.is_symlink() or not package_dir.is_dir():
        fail(f"npm package is not a real directory: {package_dir}")
    addon, helper = str(config["addon"]), str(config["helper"])
    expected_names = {"package.json", addon, helper}
    actual_names = {entry.name for entry in package_dir.iterdir()}
    if actual_names != expected_names:
        fail(f"npm package payload differs: expected {sorted(expected_names)}, got {sorted(actual_names)}")
    manifest_path = package_dir / "package.json"
    regular_file(manifest_path, "package.json")
    try:
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"cannot read npm platform manifest {manifest_path}: {error}")
    matching_manifest(manifest, npm_manifest(target, root), manifest_path)
    binary_file(package_dir / addon, target, addon, executable=False)
    helper_path = package_dir / helper
    binary_file(helper_path, target, helper, executable=True)
    if config["os"] != "win32":
        unix_mode(helper_path.lstat().st_mode, helper_path, exact=True)
    return package_dir


def stage_npm(
    target: str,
    addon_source: Path,
    helper_source: Path,
    package_dir: Path,
    root_package_json: Path,
) -> Path:
    config = target_config(target)
    addon, helper = str(config["addon"]), str(config["helper"])
    root = load_root_manifest(root_package_json)
    binary_file(addon_source, target, addon, executable=False)
    binary_file(helper_source, target, helper, executable=True)
    if package_dir.exists() or package_dir.is_symlink():
        fail(f"npm output directory must not already exist: {package_dir}")
    package_dir.mkdir(parents=True)
    try:
        shutil.copyfile(addon_source, package_dir / addon)
        shutil.copyfile(helper_source, package_dir / helper)
        if config["os"] != "win32":
            (package_dir / helper).chmod(0o755)
        (package_dir / "package.json").write_text(
            json.dumps(npm_manifest(target, root), indent=2) + "\n", encoding="utf-8"
        )
        return inspect_npm(target, package_dir, root_package_json)
    except Exception:
        shutil.rmtree(package_dir)
        raise


def self_test() -> None:
    assert set(TARGETS) == {
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    }
    assert target_config("aarch64-apple-darwin")["addon"] == "mineru.darwin-arm64.node"
    try:
        target_config("x86_64-unknown-linux-musl")
    except ValidationError:
        pass
    else:
        raise AssertionError("musl target was accepted")

    root = {"name": NPM_ROOT, "version": "1.2.3", "license": "MIT OR Apache-2.0"}
    expected = npm_manifest("x86_64-unknown-linux-gnu", root)
    assert expected == {
        "name": "@alexsun-top/mineru-linux-x64-gnu",
        "version": "1.2.3",
        "cpu": ["x64"],
        "main": "mineru.linux-x64-gnu.node",
        "files": ["mineru.linux-x64-gnu.node", "mineru-office-convert"],
        "license": "MIT OR Apache-2.0",
        "os": ["linux"],
        "libc": ["glibc"],
        "exports": {
            ".": "./mineru.linux-x64-gnu.node",
            "./helper": "./mineru-office-convert",
            "./package.json": "./package.json",
        },
    }
    invalid_manifests = [
        ({**expected, "bin": "mineru-office-convert"}, "npm bin field was accepted"),
        (
            {**expected, "exports": {key: value for key, value in expected["exports"].items() if key != "./package.json"}},
            "missing package.json export was accepted",
        ),
        (
            {**expected, "exports": {**expected["exports"], "./extra": "./extra"}},
            "extra export was accepted",
        ),
    ]
    for manifest, message in invalid_manifests:
        try:
            matching_manifest(manifest, expected, Path("package.json"))
        except ValidationError:
            pass
        else:
            raise AssertionError(message)
    try:
        unix_mode(0o100644, Path("mineru-office-convert"), exact=False)
    except ValidationError:
        pass
    else:
        raise AssertionError("non-executable Unix mode was accepted")
    unix_mode(0o100755, Path("mineru-office-convert"), exact=True)
    print("stage_binding_artifacts self-test passed")


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    subcommands = command.add_subparsers(dest="command", required=True)

    stage_py = subcommands.add_parser("stage-python")
    stage_py.add_argument("--target", required=True)
    stage_py.add_argument("--helper", type=Path, required=True)
    stage_py.add_argument("--package-dir", type=Path, required=True)

    inspect_py = subcommands.add_parser("inspect-python")
    inspect_py.add_argument("--target", required=True)
    inspect_py.add_argument("--package-dir", type=Path, required=True)

    stage_node = subcommands.add_parser("stage-npm")
    stage_node.add_argument("--target", required=True)
    stage_node.add_argument("--addon", type=Path, required=True)
    stage_node.add_argument("--helper", type=Path, required=True)
    stage_node.add_argument("--package-dir", type=Path, required=True)
    stage_node.add_argument("--root-package-json", type=Path, required=True)

    inspect_node = subcommands.add_parser("inspect-npm")
    inspect_node.add_argument("--target", required=True)
    inspect_node.add_argument("--package-dir", type=Path, required=True)
    inspect_node.add_argument("--root-package-json", type=Path, required=True)

    subcommands.add_parser("self-test")
    return command


def main() -> None:
    args = parser().parse_args()
    try:
        if args.command == "stage-python":
            result = stage_python(args.target, args.helper, args.package_dir)
        elif args.command == "inspect-python":
            result = inspect_python(args.target, args.package_dir)
        elif args.command == "stage-npm":
            result = stage_npm(
                args.target, args.addon, args.helper, args.package_dir, args.root_package_json
            )
        elif args.command == "inspect-npm":
            result = inspect_npm(args.target, args.package_dir, args.root_package_json)
        else:
            self_test()
            return
    except (ValidationError, OSError) as error:
        print(f"binding artifact validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from None
    print(result)


if __name__ == "__main__":
    main()
