#!/usr/bin/env python3
"""Fail closed unless cargo-audit finds no vulnerabilities or ignored findings."""

import argparse
import json
import tempfile
import tomllib
from pathlib import Path
from typing import NoReturn

WARNINGS = {
    ("RUSTSEC-2025-0141", "bincode", "1.3.3"),
    ("RUSTSEC-2024-0436", "paste", "1.0.15"),
    ("RUSTSEC-2026-0206", "rustybuzz", "0.20.1"),
    ("RUSTSEC-2026-0192", "ttf-parser", "0.25.1"),
    ("RUSTSEC-2024-0449", "unsafe-libyaml", "0.2.11"),
    ("RUSTSEC-2024-0320", "yaml-rust", "0.4.5"),
}
QUICK_XML_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"
QUICK_XML_VERSION = "0.41.0"
QUICK_XML_CHECKSUM = "e660451e55124f798a69a5af3f49ccfbefbd41910eefd25caf2393e1f3473ec1"


def fail(message: str) -> NoReturn:
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


def exact_set(items, expected, label, parser=tuple_from):
    found = [parser(item) for item in items]
    if len(found) != len(set(found)):
        fail(f"duplicate {label} finding")
    if set(found) != expected:
        fail(f"unexpected {label}: {sorted(set(found) ^ expected)!r}")


def check_lock(lock):
    if not isinstance(lock, dict) or lock.get("version") != 4:
        fail("Cargo.lock must use the reviewed version 4 format")
    packages = lock.get("package") if isinstance(lock, dict) else None
    if not isinstance(packages, list) or not all(isinstance(item, dict) for item in packages):
        fail("Cargo.lock lacks a valid package list")
    xml_packages = [item for item in packages if item.get("name") == "quick-xml"]
    if len(xml_packages) != 1:
        fail(f"Cargo.lock must contain exactly one quick-xml package, found {len(xml_packages)}")
    package = xml_packages[0]
    if package.get("version") != QUICK_XML_VERSION:
        fail(f"Cargo.lock quick-xml must be {QUICK_XML_VERSION}")
    if package.get("source") != QUICK_XML_SOURCE:
        fail("Cargo.lock quick-xml is not from the crates.io registry")
    checksum = package.get("checksum")
    if checksum != QUICK_XML_CHECKSUM:
        fail("Cargo.lock quick-xml checksum is not the genuine crates.io 0.41.0 digest")


def check(report, lock, audit_status):
    validate_report(report, audit_status, expect_clean=True)
    warnings = report["warnings"]
    for finding in warnings["unmaintained"]:
        if not isinstance(finding, dict) or finding.get("kind") != "unmaintained":
            fail("malformed unmaintained warning")
    exact_set(warnings["unmaintained"], WARNINGS, "warning")
    check_lock(lock)


# The published crate resolves its Office dependencies from the registry because
# `[patch.crates-io]` never reaches consumers. These are the exact findings a
# consumer sees, and they are documented in docs/compatibility.md. Any addition
# or removal must update that section in the same change.
PUBLISHED_VULNERABILITIES = {
    ("RUSTSEC-2026-0194", "quick-xml", "0.37.5"),
    ("RUSTSEC-2026-0195", "quick-xml", "0.37.5"),
    ("RUSTSEC-2026-0194", "quick-xml", "0.38.4"),
    ("RUSTSEC-2026-0195", "quick-xml", "0.38.4"),
    ("RUSTSEC-2026-0367", "zip", "0.6.6"),
    ("RUSTSEC-2026-0368", "zip", "0.6.6"),
    ("RUSTSEC-2026-0369", "zip", "0.6.6"),
    ("RUSTSEC-2026-0367", "zip", "2.6.1"),
    ("RUSTSEC-2026-0368", "zip", "2.6.1"),
    ("RUSTSEC-2026-0369", "zip", "2.6.1"),
    ("RUSTSEC-2026-0367", "zip", "8.6.0"),
    ("RUSTSEC-2026-0368", "zip", "8.6.0"),
    ("RUSTSEC-2026-0369", "zip", "8.6.0"),
}


def check_published(report, audit_status):
    validate_report(report, audit_status, expect_clean=False)
    vulnerabilities = report["vulnerabilities"]
    if (
        vulnerabilities.get("found") is not True
        or type(vulnerabilities.get("count")) is not int
        or vulnerabilities["count"] != len(PUBLISHED_VULNERABILITIES)
        or not isinstance(vulnerabilities.get("list"), list)
    ):
        fail("published vulnerability count/found/list mismatch")
    exact_set(vulnerabilities["list"], PUBLISHED_VULNERABILITIES, "published vulnerability")
    warnings = report["warnings"]
    for finding in warnings["unmaintained"]:
        if not isinstance(finding, dict) or finding.get("kind") != "unmaintained":
            fail("malformed unmaintained warning")
    exact_set(warnings["unmaintained"], WARNINGS, "warning")


def validate_report(report, audit_status, expect_clean):
    if not isinstance(report, dict) or set(report) != {"database", "lockfile", "settings", "vulnerabilities", "warnings"}:
        fail("malformed cargo-audit report schema")
    if not isinstance(report["database"], dict) or not isinstance(report["lockfile"], dict):
        fail("malformed cargo-audit database or lockfile metadata")
    settings = report["settings"]
    if not isinstance(settings, dict) or settings.get("ignore") != []:
        fail("cargo-audit report was filtered or malformed")
    expected_status = 0 if expect_clean else 1
    if type(audit_status) is not int or audit_status != expected_status:
        fail(f"cargo-audit exited with status {audit_status}")
    vulnerabilities = report["vulnerabilities"]
    if not isinstance(vulnerabilities, dict) or set(vulnerabilities) != {"found", "count", "list"}:
        fail("malformed vulnerability summary")
    if expect_clean and (
        vulnerabilities.get("found") is not False
        or type(vulnerabilities.get("count")) is not int
        or vulnerabilities["count"] != 0
        or vulnerabilities.get("list") != []
    ):
        fail("vulnerability count/found/list mismatch")
    warnings = report["warnings"]
    if (
        not isinstance(warnings, dict)
        or set(warnings) != {"unmaintained"}
        or not isinstance(warnings["unmaintained"], list)
    ):
        fail("unexpected cargo-audit warning category")


def load_report(path):
    try:
        return json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as error:
        fail(f"cannot parse cargo-audit JSON: {error}")


def fixture():
    report = {
        "database": {}, "lockfile": {}, "settings": {"ignore": []},
        "vulnerabilities": {"found": False, "count": 0, "list": []},
        "warnings": {"unmaintained": []},
    }
    report["warnings"]["unmaintained"] = [
        {"kind": "unmaintained", "advisory": {"id": advisory}, "package": {"name": name, "version": version}}
        for advisory, name, version in sorted(WARNINGS)
    ]
    lock = {"version": 4, "package": [{
        "name": "quick-xml", "version": QUICK_XML_VERSION,
        "source": QUICK_XML_SOURCE, "checksum": QUICK_XML_CHECKSUM,
    }]}
    return report, lock


def self_test():
    report, lock = fixture()
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        report_path, lock_path = root / "report.json", root / "Cargo.lock"
        report_path.write_text(json.dumps(report))
        xml = lock["package"][0]
        lock_path.write_text(
            'version = 4\n\n'
            f'[[package]]\nname = "quick-xml"\nversion = "{xml["version"]}"\n'
            f'source = "{xml["source"]}"\nchecksum = "{xml["checksum"]}"\n'
        )
        report = json.loads(report_path.read_text())
        lock = tomllib.loads(lock_path.read_text())
    check(report, lock, 0)

    def rejected(bad_report=report, bad_lock=lock, status=0):
        try:
            check(bad_report, bad_lock, status)
        except ValueError:
            return
        raise AssertionError("self-test accepted an invalid audit result")

    for key, value in (("found", True), ("count", 1), ("list", [{"id": "vulnerability"}])):
        bad = json.loads(json.dumps(report)); bad["vulnerabilities"][key] = value
        rejected(bad)
    bad = json.loads(json.dumps(report)); bad["settings"]["ignore"] = ["RUSTSEC-2026-0194"]; rejected(bad)
    bad = json.loads(json.dumps(report)); bad["warnings"]["unexpected"] = []; rejected(bad)
    for key, value in (("version", "0.38.4"), ("source", "git+https://example.invalid/quick-xml"), ("checksum", "not-a-checksum")):
        bad = json.loads(json.dumps(lock)); bad["package"][0][key] = value
        rejected(bad_lock=bad)
    bad = json.loads(json.dumps(lock)); bad["package"].append(dict(bad["package"][0])); rejected(bad_lock=bad)
    rejected(status=1)
    rejected({})

    published = json.loads(json.dumps(report))
    published["vulnerabilities"] = {
        "found": True,
        "count": len(PUBLISHED_VULNERABILITIES),
        "list": [
            {"kind": "vulnerability", "advisory": {"id": advisory}, "package": {"name": name, "version": version}}
            for advisory, name, version in sorted(PUBLISHED_VULNERABILITIES)
        ],
    }
    check_published(published, 1)

    def rejected_published(bad=None, status=1):
        try:
            check_published(published if bad is None else bad, status)
        except ValueError:
            return
        raise AssertionError("self-test accepted an invalid published audit result")

    rejected_published(status=0)
    bad = json.loads(json.dumps(published)); bad["vulnerabilities"]["count"] += 1; rejected_published(bad)
    bad = json.loads(json.dumps(published)); bad["vulnerabilities"]["found"] = False; rejected_published(bad)
    bad = json.loads(json.dumps(published)); bad["vulnerabilities"]["list"].pop(); rejected_published(bad)
    bad = json.loads(json.dumps(published))
    bad["vulnerabilities"]["list"].append(
        {"kind": "vulnerability", "advisory": {"id": "RUSTSEC-2026-9999"}, "package": {"name": "zip", "version": "9.0.5"}}
    )
    bad["vulnerabilities"]["count"] += 1
    rejected_published(bad)
    bad = json.loads(json.dumps(published)); bad["settings"]["ignore"] = ["RUSTSEC-2026-0194"]; rejected_published(bad)

    with tempfile.TemporaryDirectory() as directory:
        malformed = Path(directory) / "report.json"
        malformed.write_text("{")
        try:
            load_report(malformed)
        except ValueError:
            pass
        else:
            raise AssertionError("self-test accepted malformed JSON")


def main():
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("self-test")
    production = subcommands.add_parser("check")
    production.add_argument("--report", type=Path, required=True)
    production.add_argument("--lockfile", type=Path, required=True)
    production.add_argument("--audit-status", type=int, required=True)
    published = subcommands.add_parser("published")
    published.add_argument("--report", type=Path, required=True)
    published.add_argument("--audit-status", type=int, required=True)
    args = parser.parse_args()
    if args.command == "self-test":
        self_test()
        return
    if args.command == "published":
        check_published(load_report(args.report), args.audit_status)
        print("published crate carries exactly the documented 13 advisories")
        return
    try:
        report = load_report(args.report)
        lock = tomllib.loads(args.lockfile.read_text())
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot parse audit inputs: {error}")
    check(report, lock, args.audit_status)
    print("cargo-audit report is clean; quick-xml lock entry is registry version 0.41.0")


if __name__ == "__main__":
    try:
        main()
    except ValueError as error:
        raise SystemExit(f"cargo-audit policy failed: {error}") from error
