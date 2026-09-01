#!/usr/bin/env python3
"""Fail-closed GHCR planning, promotion, and verification for a container release."""

import argparse
import base64
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path


IMAGE_INDEX_MEDIA_TYPES = {
    "application/vnd.docker.distribution.manifest.list.v2+json",
    "application/vnd.oci.image.index.v1+json",
}
IMAGE_MANIFEST_MEDIA_TYPES = {
    "application/vnd.docker.distribution.manifest.v2+json",
    "application/vnd.oci.image.manifest.v1+json",
}
REGISTRY = "ghcr.io"
REGISTRY_MANIFEST_ACCEPT = ", ".join(
    (
        "application/vnd.oci.image.index.v1+json",
        "application/vnd.docker.distribution.manifest.list.v2+json",
        "application/vnd.oci.image.manifest.v1+json",
        "application/vnd.docker.distribution.manifest.v2+json",
    )
)
DEFAULT_PLATFORMS = {("linux", "amd64"), ("linux", "arm64")}
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}\Z")
COMMIT_RE = re.compile(r"[0-9a-f]{40}\Z")
TAG_RE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
VERSION_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\Z")
MAX_REGISTRY_RESPONSE_BYTES = 16 * 1024 * 1024


def fail(message):
    raise ValueError(message)


def validate_digest(digest):
    if not isinstance(digest, str) or not DIGEST_RE.fullmatch(digest):
        fail(f"invalid manifest digest: {digest!r}")


def validate_identity(version, commit):
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        fail(f"release version is not stable X.Y.Z: {version!r}")
    if not isinstance(commit, str) or not COMMIT_RE.fullmatch(commit):
        fail(f"release commit is not a full lowercase SHA: {commit!r}")


def version_tuple(version):
    if not isinstance(version, str) or not VERSION_RE.fullmatch(version):
        fail(f"release version is not stable X.Y.Z: {version!r}")
    return tuple(int(part) for part in version.split("."))


def stable_version_from_ref(ref):
    prefix = "refs/tags/"
    if not ref.startswith(prefix):
        return None
    match = TAG_RE.fullmatch(ref[len(prefix) :])
    return tuple(int(part) for part in match.groups()) if match else None


def validate_release_refs(document, version):
    requested = version_tuple(version)
    if not isinstance(document, list):
        fail("GitHub release refs response was not a JSON array")
    if all(isinstance(item, dict) for item in document):
        items = document
    else:
        items = []
        for page in document:
            if not isinstance(page, list):
                fail("GitHub release refs response had malformed pagination")
            items.extend(page)
    for item in items:
        if not isinstance(item, dict) or not isinstance(item.get("ref"), str):
            fail("GitHub release refs response had a malformed ref")
        candidate = stable_version_from_ref(item["ref"])
        if candidate is None:
            continue
        if candidate[:2] == requested[:2] and candidate > requested:
            fail(f"GitHub minor release tag {item['ref']} is newer than v{version}")
        if candidate[0] == requested[0] and candidate > requested:
            fail(f"GitHub major release tag {item['ref']} is newer than v{version}")


def validate_release_refs_file(path, version):
    try:
        document = json.loads(Path(path).read_text())
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RegistryError("GitHub release refs file was malformed JSON") from error
    validate_release_refs(document, version)


class RegistryError(ValueError):
    pass


class RegistryHTTPError(RegistryError):
    def __init__(self, status, detail):
        super().__init__(f"HTTP {status}: {detail or 'no response body'}")
        self.status = status


def validate_manifest_digest(digest, payload, reference):
    validate_digest(digest)
    actual_digest = "sha256:" + hashlib.sha256(payload).hexdigest()
    if actual_digest != digest:
        raise RegistryError(
            f"manifest {reference!r} payload digest {actual_digest} does not match "
            f"Docker-Content-Digest header {digest}"
        )


def _https_origin(url):
    parsed = urllib.parse.urlsplit(url)
    scheme = parsed.scheme.lower()
    if scheme != "https" or parsed.hostname is None:
        return None
    try:
        port = parsed.port
    except ValueError:
        return None
    return scheme, parsed.hostname.lower(), 443 if port is None else port


class HTTPSRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        target_url = urllib.parse.urljoin(req.full_url, newurl)
        target_origin = _https_origin(target_url)
        if target_origin is None:
            raise urllib.error.HTTPError(
                target_url, code, "redirect target must use HTTPS", headers, fp
            )
        redirect = super().redirect_request(req, fp, code, msg, headers, target_url)
        if redirect is not None and _https_origin(req.full_url) != target_origin:
            for header_collection in (redirect.headers, redirect.unredirected_hdrs):
                for header_name in list(header_collection):
                    if header_name.lower() == "authorization":
                        del header_collection[header_name]
        return redirect


class RegistryClient:
    def __init__(self, image, username, password, scope="pull"):
        if not isinstance(image, str) or not image.startswith(f"{REGISTRY}/"):
            fail(f"image must be a GHCR reference: {image!r}")
        self.image = image
        self.repository = image[len(REGISTRY) + 1 :]
        if not self.repository or ":" in self.repository or "@" in self.repository:
            fail(f"image must not contain a tag or digest: {image!r}")
        if not username or not password:
            fail("GHCR registry credentials are required")
        credentials = f"{username}:{password}".encode()
        self.basic_auth = base64.b64encode(credentials).decode()
        self.scope = scope
        self.token = None

    @staticmethod
    def _read(response):
        payload = response.read(MAX_REGISTRY_RESPONSE_BYTES + 1)
        if len(payload) > MAX_REGISTRY_RESPONSE_BYTES:
            raise RegistryError("registry response is too large")
        return payload

    def _request(self, url, headers, method="GET", data=None, expected_status=None):
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            opener = urllib.request.build_opener(HTTPSRedirectHandler())
            with opener.open(request, timeout=30) as response:
                if expected_status is not None and response.status != expected_status:
                    raise RegistryError(f"registry returned unexpected HTTP status {response.status}")
                return response.headers, self._read(response)
        except urllib.error.HTTPError as error:
            detail = error.read(4096).decode("utf-8", errors="replace")
            raise RegistryHTTPError(error.code, detail) from error
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise RegistryError(f"registry request failed: {error}") from error

    def _get_token(self):
        query = urllib.parse.urlencode(
            {"service": REGISTRY, "scope": f"repository:{self.repository}:{self.scope}"}
        )
        try:
            _, payload = self._request(
                f"https://{REGISTRY}/token?{query}",
                {
                    "Accept": "application/json",
                    "Authorization": f"Basic {self.basic_auth}",
                    "User-Agent": "mineru-rs-container-verifier/1",
                },
            )
        except RegistryHTTPError as error:
            raise RegistryError(f"GHCR token request failed: {error}") from error
        try:
            document = json.loads(payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RegistryError("GHCR token response was malformed JSON") from error
        if not isinstance(document, dict):
            raise RegistryError("GHCR token response was not a JSON object")
        token = document.get("token") or document.get("access_token")
        if not isinstance(token, str) or not token:
            raise RegistryError("GHCR token response did not contain a token")
        return token

    def _url(self, kind, reference):
        repository = urllib.parse.quote(self.repository, safe="/")
        reference = urllib.parse.quote(reference, safe=":@")
        return f"https://{REGISTRY}/v2/{repository}/{kind}/{reference}"

    def manifest(self, reference):
        if self.token is None:
            self.token = self._get_token()
        return self._request(
            self._url("manifests", reference),
            {
                "Accept": REGISTRY_MANIFEST_ACCEPT,
                "Authorization": f"Bearer {self.token}",
                "User-Agent": "mineru-rs-container-verifier/1",
            },
        )

    def blob(self, digest):
        if self.token is None:
            self.token = self._get_token()
        return self._request(
            self._url("blobs", digest),
            {
                "Accept": "application/octet-stream",
                "Authorization": f"Bearer {self.token}",
                "User-Agent": "mineru-rs-container-verifier/1",
            },
        )

    def put_manifest(self, reference, payload, media_type):
        if self.token is None:
            self.token = self._get_token()
        self._request(
            self._url("manifests", reference),
            {
                "Accept": REGISTRY_MANIFEST_ACCEPT,
                "Authorization": f"Bearer {self.token}",
                "Content-Type": media_type,
                "User-Agent": "mineru-rs-container-verifier/1",
            },
            method="PUT",
            data=payload,
            expected_status=201,
        )

    def inspect_tag(self, tag):
        try:
            headers, payload = self.manifest(tag)
        except RegistryHTTPError as error:
            if error.status == 404:
                return {"tag": tag, "state": "missing", "digest": None}
            raise RegistryError(f"cannot inspect stable tag {tag!r}: {error}") from error
        digest = headers.get("Docker-Content-Digest")
        validate_manifest_digest(digest, payload, tag)
        parse_manifest(payload, tag)
        return {"tag": tag, "state": "present", "digest": digest}

    def fetch_manifest(self, reference):
        try:
            headers, payload = self.manifest(reference)
        except RegistryHTTPError as error:
            raise RegistryError(f"cannot fetch manifest {reference!r}: {error}") from error
        digest = headers.get("Docker-Content-Digest")
        validate_manifest_digest(digest, payload, reference)
        parse_manifest(payload, reference)
        return digest, payload


def registry_client(args, scope="pull"):
    username = os.environ.get(args.username_env)
    password = os.environ.get(args.token_env)
    return RegistryClient(args.image, username, password, scope)


def parse_manifest(payload, reference):
    try:
        manifest = json.loads(payload)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RegistryError(f"manifest {reference!r} was malformed JSON") from error
    if not isinstance(manifest, dict):
        raise RegistryError(f"manifest {reference!r} was not a JSON object")
    media_type = manifest.get("mediaType")
    if media_type not in IMAGE_INDEX_MEDIA_TYPES | IMAGE_MANIFEST_MEDIA_TYPES:
        raise RegistryError(f"manifest {reference!r} has unsupported media type: {media_type!r}")
    if media_type in IMAGE_INDEX_MEDIA_TYPES:
        if not isinstance(manifest.get("manifests"), list) or not manifest["manifests"]:
            raise RegistryError(f"manifest {reference!r} has no descriptors")
        for descriptor in manifest["manifests"]:
            if not isinstance(descriptor, dict):
                raise RegistryError(f"manifest {reference!r} has a malformed descriptor")
            validate_digest(descriptor.get("digest"))
    else:
        config = manifest.get("config")
        if not isinstance(config, dict):
            raise RegistryError(f"manifest {reference!r} has no config descriptor")
        validate_digest(config.get("digest"))
    return manifest


def parse_platforms(value):
    platforms = set()
    for item in value.split(","):
        item = item.strip()
        if not item:
            continue
        parts = item.split("/")
        if len(parts) != 2 or not all(parts):
            fail(f"invalid platform: {item!r}")
        platforms.add((parts[0], parts[1]))
    if not platforms:
        fail(f"no platforms specified: {value!r}")
    return platforms


def runtime_descriptors(index, expected_platforms):
    if index.get("mediaType") not in IMAGE_INDEX_MEDIA_TYPES:
        fail(f"expected a manifest list or image index, got {index.get('mediaType')!r}")
    manifests = index.get("manifests")
    if not isinstance(manifests, list) or not manifests:
        fail("manifest index has no descriptors")

    runtimes, attestations = {}, 0
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
        if candidate not in expected_platforms:
            fail(f"unexpected runnable platform: {candidate[0]}/{candidate[1]}")
        if candidate in runtimes:
            fail(f"duplicate runnable platform: {candidate[0]}/{candidate[1]}")
        runtimes[candidate] = descriptor
    if set(runtimes) != expected_platforms:
        fail(f"runnable platforms are {sorted(runtimes)!r}, expected {sorted(expected_platforms)!r}")
    return runtimes, attestations


def runtime_platforms(index, expected_platforms):
    runtimes, attestations = runtime_descriptors(index, expected_platforms)
    return set(runtimes), attestations


def expected_tags(image, release_tag):
    return {f"{image}:{tag}" for tag in stable_tag_names(release_tag)}


def stable_tag_names(release_tag):
    match = TAG_RE.fullmatch(release_tag)
    if not match:
        fail(f"release tag is not stable vX.Y.Z: {release_tag!r}")
    assert match is not None
    major, minor, patch = match.groups()
    return {f"{major}.{minor}.{patch}", f"{major}.{minor}", major}


def plan_promotion(release_tag, states):
    if not isinstance(states, list) or not states:
        fail("stable tag inspection returned no states")
    # The patch tag is immutable; aliases are intentionally allowed to differ.
    expected = stable_tag_names(release_tag)
    by_tag = {}
    for state in states:
        if not isinstance(state, dict) or not isinstance(state.get("tag"), str):
            fail("stable tag inspection returned a malformed state")
        tag = state["tag"]
        if tag in by_tag:
            fail(f"stable tag inspection returned a duplicate: {tag!r}")
        by_tag[tag] = state
    if set(by_tag) != expected:
        fail(f"stable tag inspection returned the wrong tag set: {sorted(by_tag)!r}")
    patch_tag = next(tag for tag in expected if tag.count(".") == 2)
    patch = by_tag[patch_tag]
    if patch.get("role") != "patch":
        fail("exact patch tag has the wrong role")
    if patch.get("state") == "current":
        validate_digest(patch.get("digest"))
        digest = patch["digest"]
    elif patch.get("state") == "missing":
        if patch.get("digest") is not None:
            fail("missing exact patch tag unexpectedly has a digest")
        digest = None
    else:
        fail("exact patch tag is present but was not validated as current")
    normalized = []
    for tag in sorted(expected):
        state = by_tag[tag]
        role = "patch" if tag == patch_tag else "alias"
        if state.get("role") != role:
            fail(f"stable tag {tag!r} has the wrong role")
        current_state = state.get("state")
        current_digest = state.get("digest")
        if role == "alias":
            if current_state == "missing":
                if current_digest is not None:
                    fail(f"missing alias {tag!r} unexpectedly has a digest")
            elif current_state == "present":
                validate_digest(current_digest)
            else:
                fail(f"alias {tag!r} has an unknown state")
        action = "skip" if role == "patch" and current_state == "current" else "promote"
        if role == "alias" and digest is not None and current_state == "present" and current_digest == digest:
            action = "skip"
        if "action" in state and state["action"] != action:
            fail(f"stable tag {tag!r} has an invalid planned action")
        normalized.append({"action": action, "digest": current_digest, "role": role, "state": current_state, "tag": tag})
    return {"mode": "reuse" if digest else "build", "digest": digest, "tags": normalized}


def verify_registry_manifest(client, index, expected_platforms, version, commit):
    validate_identity(version, commit)
    runtimes, attestations = runtime_descriptors(index, expected_platforms)
    annotations = index.get("annotations", {})
    if isinstance(annotations, dict) and "org.opencontainers.image.revision" in annotations:
        if annotations["org.opencontainers.image.revision"] != commit:
            fail("manifest index OCI revision does not match the release commit")
    for platform, descriptor in sorted(runtimes.items()):
        child_digest = descriptor["digest"]
        if descriptor.get("mediaType") not in IMAGE_MANIFEST_MEDIA_TYPES:
            fail(f"child manifest for {platform[0]}/{platform[1]} has an unsupported media type")
        remote_digest, payload = client.fetch_manifest(child_digest)
        if remote_digest != child_digest:
            fail(f"child manifest digest changed for {platform[0]}/{platform[1]}")
        child = parse_manifest(payload, child_digest)
        if child.get("mediaType") not in IMAGE_MANIFEST_MEDIA_TYPES:
            fail(f"child reference for {platform[0]}/{platform[1]} is not an image manifest")
        config = child.get("config")
        if not isinstance(config, dict):
            fail(f"child manifest for {platform[0]}/{platform[1]} has no config descriptor")
        config_digest = config.get("digest")
        validate_digest(config_digest)
        _, config_payload = client.blob(config_digest)
        actual_config_digest = "sha256:" + hashlib.sha256(config_payload).hexdigest()
        if actual_config_digest != config_digest:
            fail(f"image config digest changed for {platform[0]}/{platform[1]}")
        try:
            config_document = json.loads(config_payload)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RegistryError(f"image config for {platform[0]}/{platform[1]} was malformed JSON") from error
        config_section = config_document.get("config") if isinstance(config_document, dict) else None
        labels = config_section.get("Labels") if isinstance(config_section, dict) else None
        revision = labels.get("org.opencontainers.image.revision") if isinstance(labels, dict) else None
        if revision != commit:
            fail(f"OCI revision label for {platform[0]}/{platform[1]} does not match the release commit")
        image_version = labels.get("org.opencontainers.image.version") if isinstance(labels, dict) else None
        if image_version != version:
            fail(f"OCI version label for {platform[0]}/{platform[1]} does not match the release version")
    return runtimes, attestations


def command_plan(args):
    validate_identity(args.version, args.commit)
    validate_release_refs_file(args.release_refs_file, args.version)
    names = sorted(stable_tag_names(args.release_tag))
    client = registry_client(args)
    patch_tag = next(tag for tag in names if tag.count(".") == 2)
    states = []
    for tag in names:
        state = client.inspect_tag(tag)
        state["role"] = "patch" if tag == patch_tag else "alias"
        if state["role"] == "patch" and state["state"] == "present":
            digest = state["digest"]
            remote_digest, payload = client.fetch_manifest(digest)
            if remote_digest != digest:
                fail("exact patch tag manifest digest changed during inspection")
            index = parse_manifest(payload, digest)
            verify_registry_manifest(client, index, DEFAULT_PLATFORMS, args.version, args.commit)
            state["state"] = "current"
        states.append(state)
    plan = plan_promotion(args.release_tag, states)
    document = {"image": args.image, "release_tag": args.release_tag, **plan}
    Path(args.output).write_text(json.dumps(document, sort_keys=True, indent=2) + "\n")
    print(f"GHCR stable tag plan: {plan['mode']}")
    for state in plan["tags"]:
        detail = state["digest"] if state["state"] in {"present", "current"} else "missing"
        print(f"{args.image}:{state['tag']} ({state['role']}/{state['state']}/{state['action']}) -> {detail}")


def load_plan(path, image, release_tag):
    try:
        document = json.loads(Path(path).read_text())
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RegistryError("GHCR promotion plan was malformed JSON") from error
    if not isinstance(document, dict) or document.get("image") != image or document.get("release_tag") != release_tag:
        fail("GHCR promotion plan does not match this release")
    expected = stable_tag_names(release_tag)
    states = document.get("tags")
    if not isinstance(states, list) or {state.get("tag") for state in states if isinstance(state, dict)} != expected:
        fail("GHCR promotion plan has the wrong stable tag set")
    recomputed = plan_promotion(release_tag, states)
    if (
        document.get("mode") != recomputed["mode"]
        or document.get("digest") != recomputed["digest"]
        or document.get("tags") != recomputed["tags"]
    ):
        fail("GHCR promotion plan changed after inspection")
    return document


def promote_tag(client, image, tag, digest, role, planned_state, planned_digest):
    state = client.inspect_tag(tag)
    if role == "patch":
        if planned_state == "current":
            if state["state"] != "present" or state["digest"] != digest:
                fail(f"exact patch tag {tag} changed after identity verification")
            print(f"{image}:{tag} -> {digest} (immutable; skipped)")
            return
        if planned_state != "missing" or state["state"] == "present":
            fail(f"exact patch tag {tag} conflicts with the requested release")
        if state["state"] != "missing":
            fail(f"exact patch tag {tag} has an unknown state")
    else:
        if state["state"] == "present" and state["digest"] == digest:
            print(f"{image}:{tag} -> {digest} (matching alias; skipped)")
            return
        if state["state"] == "present":
            if planned_state != "present" or state["digest"] != planned_digest:
                fail(f"alias tag {tag} changed before promotion")
        elif state["state"] != "missing":
            fail(f"alias tag {tag} has an unknown state")
    try:
        source_digest, payload = client.fetch_manifest(digest)
        if source_digest != digest:
            fail(f"promotion source resolved to {source_digest!r}, expected {digest}")
        source = parse_manifest(payload, digest)
        client.put_manifest(tag, payload, source["mediaType"])
    except RegistryError as error:
        observed = client.inspect_tag(tag)
        if role == "alias" and observed["state"] == "present" and observed["digest"] == digest:
            print(f"{image}:{tag} -> {digest} (already promoted)")
            return
        if observed["state"] == "present":
            fail(f"tag {tag} conflicts after promotion failure")
        fail(f"could not promote stable tag {tag}: {error}")
    observed = client.inspect_tag(tag)
    if observed["state"] != "present" or observed["digest"] != digest:
        fail(f"stable tag {tag} did not resolve to {digest} after promotion")
    print(f"{image}:{tag} -> {digest} (promoted)")


def command_promote(args):
    validate_digest(args.digest)
    validate_release_refs_file(args.release_refs_file, args.version)
    document = load_plan(args.plan, args.image, args.release_tag)
    if document["digest"] is not None and document["digest"] != args.digest:
        fail(f"selected digest {args.digest} differs from the planned digest {document['digest']}")
    client = registry_client(args, "pull,push")
    patch = next(state for state in document["tags"] if state["role"] == "patch")
    promote_tag(client, args.image, patch["tag"], args.digest, patch["role"], patch["state"], patch["digest"])
    patch_state = client.inspect_tag(patch["tag"])
    if patch_state["state"] != "present" or patch_state["digest"] != args.digest:
        fail(f"exact patch tag {patch['tag']} was not verified before alias promotion")
    for state in document["tags"]:
        if state["role"] == "alias":
            promote_tag(client, args.image, state["tag"], args.digest, state["role"], state["state"], state["digest"])


def command_manifest(args):
    validate_digest(args.digest)
    expected = parse_platforms(args.platforms)
    if args.image:
        if not args.version or not args.commit:
            fail("--image requires --version and --commit")
        client = registry_client(args)
        remote_digest, payload = client.fetch_manifest(args.digest)
        if remote_digest != args.digest:
            fail(f"requested manifest digest resolved to {remote_digest!r}")
        Path(args.file).write_bytes(payload)
        index = parse_manifest(payload, args.digest)
        runtimes, attestations = verify_registry_manifest(client, index, expected, args.version, args.commit)
    else:
        runtimes, attestations = runtime_platforms(json.loads(Path(args.file).read_text()), expected)
    print(f"manifest digest: {args.digest}")
    print("expected platforms: " + ", ".join(f"{os}/{arch}" for os, arch in sorted(expected)))
    print("runnable platforms: " + ", ".join(f"{os}/{arch}" for os, arch in sorted(runtimes)))
    print(f"BuildKit attestation descriptors: {attestations}")


def command_tags(args):
    validate_digest(args.digest)
    expected_names = stable_tag_names(args.release_tag)
    if args.tags_file:
        tags = {line.strip() for line in Path(args.tags_file).read_text().splitlines() if line.strip()}
        expected = expected_tags(args.image, args.release_tag)
        if tags != expected:
            fail(f"emitted tags are {sorted(tags)!r}, expected {sorted(expected)!r}")
    client = registry_client(args)
    for tag in sorted(expected_names):
        state = client.inspect_tag(tag)
        if state["state"] == "missing":
            fail(f"stable tag {tag} is missing after promotion")
        if state["digest"] != args.digest:
            fail(f"{args.image}:{tag} resolves to {state['digest']!r}, expected {args.digest}")
        print(f"{args.image}:{tag} -> {state['digest']}")


def self_test():
    def rejects_release_refs(document, message):
        try:
            validate_release_refs(document, "1.2.3")
        except ValueError:
            return
        raise AssertionError(f"release ref check did not reject {message}")

    rejects_release_refs([{"ref": "refs/tags/v1.2.4"}], "higher same-minor release")
    rejects_release_refs([{"ref": "refs/tags/v1.3.0"}], "higher same-major release")
    validate_release_refs(
        [[
            {"ref": "refs/tags/v1.2.2"},
            {"ref": "refs/tags/v1.2.4-rc.1"},
            {"ref": "refs/tags/v1.1.9"},
            {"ref": "refs/tags/v2.0.0"},
        ]],
        "1.2.3",
    )
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
    assert runtime_platforms(index, DEFAULT_PLATFORMS) == (DEFAULT_PLATFORMS, 1)
    manifest_payload = json.dumps(index, separators=(",", ":")).encode()
    manifest_digest = "sha256:" + hashlib.sha256(manifest_payload).hexdigest()
    validate_manifest_digest(manifest_digest, manifest_payload, "matching")
    client = RegistryClient("ghcr.io/agentsyaml/mineru-cli", "user", "token")
    setattr(client, "manifest", lambda reference: ({"Docker-Content-Digest": manifest_digest}, manifest_payload))
    assert client.inspect_tag("matching") == {
        "tag": "matching",
        "state": "present",
        "digest": manifest_digest,
    }
    assert client.fetch_manifest("matching") == (manifest_digest, manifest_payload)
    setattr(
        client,
        "manifest",
        lambda reference: ({"Docker-Content-Digest": "sha256:" + "0" * 64}, manifest_payload),
    )
    for operation, reference in ((client.inspect_tag, "mismatching inspect"), (client.fetch_manifest, "mismatching fetch")):
        try:
            operation(reference)
        except RegistryError:
            pass
        else:
            raise AssertionError(f"{reference} digest mismatch must fail")

    redirect_handler = HTTPSRedirectHandler()
    request = urllib.request.Request(
        "https://ghcr.io/v2/agentsyaml/mineru-cli/blobs/sha256:" + "4" * 64,
        headers={"Authorization": "Bearer scoped"},
    )
    for target_url in (
        "https://objects.example/blobs/sha256:" + "4" * 64,
        "https://ghcr.io:444/blobs/sha256:" + "4" * 64,
    ):
        redirect = redirect_handler.redirect_request(request, None, 307, "temporary redirect", {}, target_url)
        assert redirect.get_header("Authorization") is None
    same_origin = redirect_handler.redirect_request(
        request, None, 307, "temporary redirect", {}, "https://ghcr.io:443/v2/next"
    )
    assert same_origin.get_header("Authorization") == "Bearer scoped"
    try:
        redirect_handler.redirect_request(request, None, 307, "temporary redirect", {}, "http://ghcr.io/v2/next")
    except urllib.error.HTTPError:
        pass
    else:
        raise AssertionError("non-HTTPS redirect must fail")
    amd64_only = {
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"digest": "sha256:" + "4" * 64, "platform": {"os": "linux", "architecture": "amd64"}}],
    }
    assert runtime_platforms(amd64_only, {("linux", "amd64")}) == ({("linux", "amd64")}, 0)
    malformed = json.loads(json.dumps(index))
    malformed["manifests"][0]["digest"] = "bad"
    try:
        runtime_platforms(malformed, DEFAULT_PLATFORMS)
    except ValueError:
        pass
    else:
        raise AssertionError("malformed child digest must fail")
    assert expected_tags("ghcr.io/agentsyaml/mineru-cli", "v1.2.3") == {
        "ghcr.io/agentsyaml/mineru-cli:1.2.3",
        "ghcr.io/agentsyaml/mineru-cli:1.2",
        "ghcr.io/agentsyaml/mineru-cli:1",
    }
    assert stable_tag_names("v1.2.3") == {"1.2.3", "1.2", "1"}
    digest_a = "sha256:" + "a" * 64
    digest_b = "sha256:" + "b" * 64
    def tag_states(patch, minor, major):
        return [
            {"tag": "1.2.3", "role": "patch", **patch},
            {"tag": "1.2", "role": "alias", **minor},
            {"tag": "1", "role": "alias", **major},
        ]

    prior_aliases = plan_promotion("v1.2.3", tag_states(
        {"state": "missing", "digest": None},
        {"state": "present", "digest": digest_a},
        {"state": "present", "digest": digest_b},
    ))
    assert prior_aliases["mode"] == "build" and prior_aliases["digest"] is None
    assert all(item["action"] == "promote" for item in prior_aliases["tags"])
    stale_aliases = plan_promotion("v1.2.3", tag_states(
        {"state": "current", "digest": digest_a},
        {"state": "present", "digest": digest_b},
        {"state": "present", "digest": digest_b},
    ))
    assert stale_aliases["mode"] == "reuse" and stale_aliases["digest"] == digest_a
    assert {item["tag"]: item["action"] for item in stale_aliases["tags"]} == {
        "1.2.3": "skip", "1.2": "promote", "1": "promote"
    }
    partial = plan_promotion("v1.2.3", tag_states(
        {"state": "current", "digest": digest_a},
        {"state": "present", "digest": digest_a},
        {"state": "missing", "digest": None},
    ))
    assert {item["tag"]: item["action"] for item in partial["tags"]} == {
        "1.2.3": "skip", "1.2": "skip", "1": "promote"
    }
    for invalid_states, message in (
        (tag_states(
            {"state": "present", "digest": digest_b},
            {"state": "present", "digest": digest_a},
            {"state": "present", "digest": digest_a},
        ), "conflicting exact patch tag"),
        (tag_states(
            {"state": "missing", "digest": None},
            {"state": "unknown", "digest": None},
            {"state": "present", "digest": digest_a},
        ), "unknown alias state"),
        ([{"tag": "1.2.3", "role": "alias", "state": "missing", "digest": None}], "wrong tag set"),
    ):
        try:
            plan_promotion("v1.2.3", invalid_states)
        except ValueError:
            pass
        else:
            raise AssertionError(f"planner did not reject {message}")
    assert parse_platforms("linux/amd64") == {("linux", "amd64")}
    try:
        runtime_platforms({"mediaType": "application/vnd.oci.image.index.v1+json", "manifests": []}, DEFAULT_PLATFORMS)
    except ValueError:
        pass
    else:
        raise AssertionError("empty index must fail")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    manifest = commands.add_parser("manifest")
    manifest.add_argument("--digest", required=True)
    manifest.add_argument("--file", required=True)
    manifest.add_argument("--image")
    manifest.add_argument("--version")
    manifest.add_argument("--commit")
    manifest.add_argument("--username-env", default="GHCR_USERNAME")
    manifest.add_argument("--token-env", default="GHCR_TOKEN")
    manifest.add_argument(
        "--platforms",
        default="linux/amd64,linux/arm64",
        help="comma-separated expected runnable platforms (default: linux/amd64,linux/arm64)",
    )
    tags = commands.add_parser("tags")
    tags.add_argument("--digest", required=True)
    tags.add_argument("--image", required=True)
    tags.add_argument("--release-tag", required=True)
    tags.add_argument("--tags-file")
    tags.add_argument("--username-env", default="GHCR_USERNAME")
    tags.add_argument("--token-env", default="GHCR_TOKEN")
    plan = commands.add_parser("plan")
    plan.add_argument("--image", required=True)
    plan.add_argument("--release-tag", required=True)
    plan.add_argument("--version", required=True)
    plan.add_argument("--commit", required=True)
    plan.add_argument("--release-refs-file", required=True)
    plan.add_argument("--output", required=True)
    plan.add_argument("--username-env", default="GHCR_USERNAME")
    plan.add_argument("--token-env", default="GHCR_TOKEN")
    promote = commands.add_parser("promote")
    promote.add_argument("--image", required=True)
    promote.add_argument("--release-tag", required=True)
    promote.add_argument("--version", required=True)
    promote.add_argument("--digest", required=True)
    promote.add_argument("--plan", required=True)
    promote.add_argument("--release-refs-file", required=True)
    promote.add_argument("--username-env", default="GHCR_USERNAME")
    promote.add_argument("--token-env", default="GHCR_TOKEN")
    commands.add_parser("self-test")
    args = parser.parse_args()
    if args.command == "manifest":
        command_manifest(args)
    elif args.command == "plan":
        command_plan(args)
    elif args.command == "promote":
        command_promote(args)
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
