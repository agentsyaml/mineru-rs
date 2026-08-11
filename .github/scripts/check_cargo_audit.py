#!/usr/bin/env python3
"""Fail closed on the temporary, reviewed cargo-audit exception set."""

import argparse
import datetime as dt
import json
import re
import tempfile
import tomllib
from pathlib import Path

VULNERABILITIES = {
    ("RUSTSEC-2026-0194", "quick-xml", "0.37.5"),
    ("RUSTSEC-2026-0195", "quick-xml", "0.37.5"),
    ("RUSTSEC-2026-0194", "quick-xml", "0.38.4"),
    ("RUSTSEC-2026-0195", "quick-xml", "0.38.4"),
}
WARNINGS = {
    ("RUSTSEC-2025-0141", "bincode", "1.3.3"),
    ("RUSTSEC-2024-0436", "paste", "1.0.15"),
    ("RUSTSEC-2026-0206", "rustybuzz", "0.20.1"),
    ("RUSTSEC-2026-0192", "ttf-parser", "0.25.1"),
    ("RUSTSEC-2024-0320", "yaml-rust", "0.4.5"),
}
EDGES = {
    ("umya-spreadsheet", "2.3.3", "quick-xml", "0.37.5"),
    # office2pdf is the reverse parent of umya-spreadsheet in Cargo.lock.
    ("office2pdf", "0.6.5", "umya-spreadsheet", "2.3.3"),
    ("office2pdf", "0.6.5", "quick-xml", "0.38.4"),
    ("office2pdf", "0.6.5", "typst", "0.14.2"),
    ("citationberg", "0.6.1", "quick-xml", "0.38.4"),
    ("hayagriva", "0.9.1", "citationberg", "0.6.1"),
    ("typst-library", "0.14.2", "hayagriva", "0.9.1"),
    ("typst", "0.14.2", "typst-library", "0.14.2"),
}
EXPIRY = dt.date(2026, 9, 30)


def fail(message):
    raise ValueError(message)


def tuple_from(finding):
    if not isinstance(finding, dict):
        fail("finding is not an object")
    advisory, package = finding.get("advisory"), finding.get("package")
    if not isinstance(advisory, dict) or not isinstance(package, dict):
        fail("finding lacks advisory or package object")
    result = (advisory.get("id"), package.get("name"), package.get("version"))
    if not all(isinstance(value, str) and value for value in result):
        fail("finding has malformed advisory/package tuple")
    return result


def exact_set(items, expected, label):
    found = [tuple_from(item) for item in items]
    if len(found) != len(set(found)):
        fail(f"duplicate {label} finding")
    if set(found) != expected:
        fail(f"unexpected {label}: {sorted(set(found) ^ expected)!r}")


def package_map(lock):
    packages = lock.get("package")
    if not isinstance(packages, list):
        fail("Cargo.lock lacks package list")
    result = {}
    for package in packages:
        if not isinstance(package, dict):
            fail("Cargo.lock package is malformed")
        name, version = package.get("name"), package.get("version")
        if not isinstance(name, str) or not isinstance(version, str):
            fail("Cargo.lock package lacks name/version")
        key = (name, version)
        if key in result:
            fail(f"duplicate Cargo.lock package {key}")
        dependencies = package.get("dependencies", [])
        if not isinstance(dependencies, list) or not all(isinstance(item, str) for item in dependencies):
            fail(f"malformed dependencies for {key}")
        result[key] = dependencies
    return result


def check_lock(lock):
    packages = package_map(lock)

    def depends_on(dependencies, child, child_version):
        if f"{child} {child_version}" in dependencies:
            return True
        if child in dependencies:
            return [key for key in packages if key[0] == child] == [(child, child_version)]
        return False

    for parent, parent_version, child, child_version in EDGES:
        dependencies = packages.get((parent, parent_version))
        if dependencies is None or not depends_on(dependencies, child, child_version):
            fail(f"missing reviewed edge {parent} {parent_version} -> {child} {child_version}")
    for version, expected_parents in {
        "0.37.5": {("umya-spreadsheet", "2.3.3")},
        "0.38.4": {("office2pdf", "0.6.5"), ("citationberg", "0.6.1")},
    }.items():
        if ("quick-xml", version) not in packages:
            fail(f"missing vulnerable quick-xml {version}")
        parents = {
            key for key, dependencies in packages.items()
            if depends_on(dependencies, "quick-xml", version)
        }
        if parents != expected_parents:
            fail(f"quick-xml {version} parent drift: {sorted(parents)!r}")


def check_version(manifest, today):
    package = manifest.get("package")
    version = package.get("version") if isinstance(package, dict) else None
    if isinstance(version, dict) and version == {"workspace": True}:
        workspace = manifest.get("workspace")
        workspace_package = workspace.get("package") if isinstance(workspace, dict) else None
        version = workspace_package.get("version") if isinstance(workspace_package, dict) else None
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", version or "")
    if not match or tuple(map(int, match.groups())) > (0, 2, 6):
        fail(f"project version is outside exception policy: {version!r}")
    if today >= EXPIRY:
        fail(f"cargo-audit exception expired on {EXPIRY.isoformat()}")


def check(report, lock, manifest, today=None):
    if not isinstance(report, dict) or set(report) != {"database", "lockfile", "settings", "vulnerabilities", "warnings"}:
        fail("malformed cargo-audit report schema")
    settings = report["settings"]
    if not isinstance(settings, dict) or settings.get("ignore") != []:
        fail("cargo-audit report was filtered or malformed")
    vulnerabilities = report["vulnerabilities"]
    if not isinstance(vulnerabilities, dict) or vulnerabilities.get("found") is not True or vulnerabilities.get("count") != 4 or not isinstance(vulnerabilities.get("list"), list):
        fail("vulnerability count/found/list mismatch")
    exact_set(vulnerabilities["list"], VULNERABILITIES, "vulnerability")
    warnings = report["warnings"]
    if not isinstance(warnings, dict) or set(warnings) != {"unmaintained"} or not isinstance(warnings["unmaintained"], list):
        fail("unexpected cargo-audit warning category")
    for finding in warnings["unmaintained"]:
        if finding.get("kind") != "unmaintained":
            fail("malformed unmaintained warning")
    exact_set(warnings["unmaintained"], WARNINGS, "warning")
    check_lock(lock)
    check_version(manifest, today or dt.datetime.now(dt.timezone.utc).date())


def fixture():
    report = {
        "database": {}, "lockfile": {}, "settings": {"ignore": []},
        "vulnerabilities": {"found": True, "count": 4, "list": []},
        "warnings": {"unmaintained": []},
    }
    report["vulnerabilities"]["list"] = [
        {"advisory": {"id": advisory}, "package": {"name": name, "version": version}}
        for advisory, name, version in sorted(VULNERABILITIES)
    ]
    report["warnings"]["unmaintained"] = [
        {"kind": "unmaintained", "advisory": {"id": advisory}, "package": {"name": name, "version": version}}
        for advisory, name, version in sorted(WARNINGS)
    ]
    packages = {}
    for parent, parent_version, child, child_version in EDGES:
        packages.setdefault((parent, parent_version), []).append(f"{child} {child_version}")
        packages.setdefault((child, child_version), [])
    lock = {"package": [
        {"name": name, "version": version, "dependencies": dependencies}
        for (name, version), dependencies in packages.items()
    ]}
    return report, lock, {"package": {"version": "0.2.3"}}


def self_test():
    report, lock, manifest = fixture()
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        report_path, lock_path, manifest_path = root / "report.json", root / "Cargo.lock", root / "Cargo.toml"
        report_path.write_text(json.dumps(report))
        lock_path.write_text("\n".join(
            "[[package]]\nname = \"{}\"\nversion = \"{}\"\ndependencies = [{}]\n".format(
                package["name"], package["version"], ", ".join(json.dumps(item) for item in package["dependencies"])
            ) for package in lock["package"]
        ))
        manifest_path.write_text('[package]\nversion = "0.2.3"\n')
        report = json.loads(report_path.read_text())
        lock = tomllib.loads(lock_path.read_text())
        manifest = tomllib.loads(manifest_path.read_text())
    check(report, lock, manifest, dt.date(2026, 1, 1))
    cases = []
    unknown = json.loads(json.dumps(report)); unknown["vulnerabilities"]["list"][0]["advisory"]["id"] = "RUSTSEC-0000-0000"; cases.append((unknown, lock, manifest))
    changed_version = json.loads(json.dumps(report)); changed_version["vulnerabilities"]["list"][0]["package"]["version"] = "0.37.6"; cases.append((changed_version, lock, manifest))
    edge = json.loads(json.dumps(lock))
    next(package for package in edge["package"] if package["name"] == "umya-spreadsheet")["dependencies"] = []
    cases.append((report, edge, manifest))
    warning = json.loads(json.dumps(report)); warning["warnings"]["unmaintained"].append(warning["warnings"]["unmaintained"][0]); cases.append((warning, lock, manifest))
    cases.append(({}, lock, manifest))
    cases.append((report, lock, {"package": {"version": "0.2.7"}}))
    for bad_report, bad_lock, bad_manifest in cases:
        try:
            check(bad_report, bad_lock, bad_manifest, dt.date(2026, 1, 1))
        except ValueError:
            continue
        raise AssertionError("self-test accepted an invalid fixture")
    try:
        check(report, lock, manifest, EXPIRY)
    except ValueError:
        return
    raise AssertionError("self-test accepted an expired exception")


def main():
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("self-test")
    production = subcommands.add_parser("check")
    production.add_argument("--report", type=Path, required=True)
    production.add_argument("--lockfile", type=Path, required=True)
    production.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "self-test":
        self_test()
        return
    try:
        report = json.loads(args.report.read_text())
        lock = tomllib.loads(args.lockfile.read_text())
        manifest = tomllib.loads(args.manifest.read_text())
    except (OSError, json.JSONDecodeError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot parse audit inputs: {error}")
    check(report, lock, manifest)
    print("cargo-audit exception approved: RUSTSEC-2026-0194, RUSTSEC-2026-0195; expires 2026-09-30 UTC")


if __name__ == "__main__":
    try:
        main()
    except ValueError as error:
        raise SystemExit(f"cargo-audit policy failed: {error}") from error
