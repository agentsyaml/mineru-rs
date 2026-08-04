#!/usr/bin/env python3
"""Fail-closed verification for a pushed multi-platform container release."""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


IMAGE_INDEX_MEDIA_TYPES = {
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.index.v1+json",
}
EXPECTED_PLATFORMS = {("linux", "amd64"), ("linux", "arm64")}
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
TAG_RE = re.compile(r"v(\d+)\.(\d+)\.(\d+)\Z")


def fail(message):
    raise ValueError(message)


def validate_digest(digest):
    if not isinstance(digest, str) or not DIGEST_RE.fullmatch(digest):
        fail(f"invalid manifest digest: {digest!r}")


def runtime_platforms(index):
    if index.get("mediaType") not in IMAGE_INDEX_MEDIA_TYPES:
        fail(f"expected a manifest list or image index, got {index.get('mediaType')!r}")
    manifests = index.get("manifests")
    if not isinstance(manifests, list) or not manifests:
        fail("manifest index has no descriptors")

    runtimes, attestations = set(), 0
    for descriptor in manifests:
        if not isinstance(descriptor, dict):
            fail("manifest index has a non-object descriptor")
        validate_digest(descriptor.get("digest"))
        platform = descriptor.get("platform")
        annotations = descriptor.get("annotations", {})
        is_attestation = isinstance(annotations, dict) and annotations.get(
            "vnd.docker.reference.type"
        ) == "attestation-manifest"
        if is_attestation:
            if not isinstance(platform, dict) or (platform.get("os"), platform.get("architecture")) != (
                "unknown",
                "unknown",
            ):
                fail("attestation descriptor does not use platform unknown/unknown")
            attestations += 1
            continue
        if not isinstance(platform, dict):
            fail("runnable descriptor has no platform")
        candidate = (platform.get("os"), platform.get("architecture"))
        if candidate not in EXPECTED_PLATFORMS:
            fail(f"unexpected runnable platform: {candidate[0]}/{candidate[1]}")
        if candidate in runtimes:
            fail(f"duplicate runnable platform: {candidate[0]}/{candidate[1]}")
        runtimes.add(candidate)
    if runtimes != EXPECTED_PLATFORMS:
        fail(f"runnable platforms are {sorted(runtimes)!r}, expected {sorted(EXPECTED_PLATFORMS)!r}")
    return runtimes, attestations


def expected_tags(image, release_tag):
    match = TAG_RE.fullmatch(release_tag)
    if not match:
        fail(f"release tag is not stable vX.Y.Z: {release_tag!r}")
    assert match is not None
    major, minor, patch = match.groups()
    return {f"{image}:{major}.{minor}.{patch}", f"{image}:{major}.{minor}", f"{image}:{major}", f"{image}:latest"}


def inspect_tag(tag):
    result = subprocess.run(
        ["docker", "buildx", "imagetools", "inspect", "--format", '{{printf "%s\\n" .Manifest.Digest}}', tag],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        fail(f"could not inspect {tag}: {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout.strip()


def command_manifest(args):
    validate_digest(args.digest)
    runtimes, attestations = runtime_platforms(json.loads(Path(args.file).read_text()))
    print(f"manifest digest: {args.digest}")
    print("runnable platforms: " + ", ".join(f"{os}/{arch}" for os, arch in sorted(runtimes)))
    print(f"BuildKit attestation descriptors: {attestations}")


def command_tags(args):
    validate_digest(args.digest)
    tags = {line.strip() for line in Path(args.tags_file).read_text().splitlines() if line.strip()}
    expected = expected_tags(args.image, args.release_tag)
    if tags != expected:
        fail(f"emitted tags are {sorted(tags)!r}, expected {sorted(expected)!r}")
    for tag in sorted(tags):
        actual = inspect_tag(tag)
        if actual != args.digest:
            fail(f"{tag} resolves to {actual!r}, expected {args.digest}")
        print(f"{tag} -> {actual}")


def self_test():
    index = {
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [
            {"digest": "sha256:" + "1" * 64, "platform": {"os": "linux", "architecture": "amd64"}},
            {"digest": "sha256:" + "2" * 64, "platform": {"os": "linux", "architecture": "arm64"}},
            {
                "digest": "sha256:" + "3" * 64,
                "platform": {"os": "unknown", "architecture": "unknown"},
                "annotations": {"vnd.docker.reference.type": "attestation-manifest"},
            },
        ],
    }
    assert runtime_platforms(index) == (EXPECTED_PLATFORMS, 1)
    malformed = json.loads(json.dumps(index))
    malformed["manifests"][0]["digest"] = "bad"
    try:
        runtime_platforms(malformed)
    except ValueError:
        pass
    else:
        raise AssertionError("malformed child digest must fail")
    assert expected_tags("ghcr.io/agentsyaml/mineru-rs", "v1.2.3") == {
        "ghcr.io/agentsyaml/mineru-rs:1.2.3",
        "ghcr.io/agentsyaml/mineru-rs:1.2",
        "ghcr.io/agentsyaml/mineru-rs:1",
        "ghcr.io/agentsyaml/mineru-rs:latest",
    }
    try:
        runtime_platforms({"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": []})
    except ValueError:
        return
    raise AssertionError("empty index must fail")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    manifest = commands.add_parser("manifest")
    manifest.add_argument("--digest", required=True)
    manifest.add_argument("--file", required=True)
    tags = commands.add_parser("tags")
    tags.add_argument("--digest", required=True)
    tags.add_argument("--image", required=True)
    tags.add_argument("--release-tag", required=True)
    tags.add_argument("--tags-file", required=True)
    commands.add_parser("self-test")
    args = parser.parse_args()
    if args.command == "manifest":
        command_manifest(args)
    elif args.command == "tags":
        command_tags(args)
    else:
        self_test()


if __name__ == "__main__":
    try:
        main()
    except (ValueError, json.JSONDecodeError) as error:
        print(f"container release verification failed: {error}", file=sys.stderr)
        sys.exit(1)
