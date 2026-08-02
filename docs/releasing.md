# Releasing

Releases are coordinated across crates.io `mineru`, PyPI `mineru-rs`, npm
`@alexsun-top/mineru`, and GHCR `ghcr.io/agentsyaml/mineru-rs`. Registry
credentials must be short-lived environment variables or trusted-publisher OIDC
credentials. Never store a registry token in the repository, GitHub secrets,
`.npmrc`, or Cargo configuration.

## One-time repository and registry setup

In `agentsyaml/mineru-rs`, create protected GitHub environments named
`crates-io`, `pypi`, `npm`, and `ghcr`, with required reviewers and release-tag
restrictions as appropriate.

- PyPI: create a **pending trusted publisher** for project `mineru-rs`,
  repository `agentsyaml/mineru-rs`, workflow `release.yml`, environment
  `pypi`. Do not bootstrap or publish `0.0.1` to PyPI.
- crates.io: after the bootstrap below creates `mineru`, configure its trusted
  publisher for repository `agentsyaml/mineru-rs`, workflow `release.yml`,
  environment `crates-io`, then yank `0.0.1`.
- npm: control the `@alexsun-top` scope. After bootstrap creates all seven
  records, configure a trusted publisher on every record for repository
  `agentsyaml/mineru-rs`, workflow `release.yml`, environment `npm`.
- GHCR: `publish-container` uses the workflow `GITHUB_TOKEN` with only
  `contents: read` and `packages: write` permissions.

No long-lived registry token belongs in GitHub Actions. The crates.io, PyPI,
and npm jobs acquire OIDC credentials only after all release artifacts pass
verification; GHCR uses the job-scoped `GITHUB_TOKEN` permissions above.

## Non-user-facing 0.0.1 bootstrap

Perform bootstrap from a temporary checkout or branch with the Cargo workspace
version changed to `0.0.1`. Main remains at `0.1.0`.

### crates.io

The crates.io bootstrap is the actual package, not a placeholder. Use a
short-lived token only through the environment:

```sh
export CARGO_REGISTRY_TOKEN='<short-lived-token>'
cargo +1.89.0 generate-lockfile
cargo +1.89.0 metadata --locked --no-deps --format-version 1 | python3 -c 'import json, sys; expected = {"mineru": "0.0.1", "mineru-node": "0.0.1", "mineru-python": "0.0.1"}; actual = {p["name"]: p["version"] for p in json.load(sys.stdin)["packages"]}; assert actual == expected, actual'
cargo +1.89.0 package -p mineru --locked
cargo +1.89.0 publish -p mineru --locked
```

After publishing, restore the real workspace version and `Cargo.lock` (or
discard the temporary checkout). Configure trusted publishing immediately,
then yank the bootstrap and remove the token from the environment:

```sh
CARGO_REGISTRY_TOKEN="$CARGO_REGISTRY_TOKEN" cargo yank --vers 0.0.1 mineru
unset CARGO_REGISTRY_TOKEN
```

### npm

Create seven temporary, metadata-only `0.0.1` package directories with these
exact manifest names:

1. `@alexsun-top/mineru-darwin-x64`
2. `@alexsun-top/mineru-darwin-arm64`
3. `@alexsun-top/mineru-linux-x64-gnu`
4. `@alexsun-top/mineru-linux-arm64-gnu`
5. `@alexsun-top/mineru-win32-x64-msvc`
6. `@alexsun-top/mineru-win32-arm64-msvc`
7. `@alexsun-top/mineru`

These records deliberately contain no usable native build. They exist only to
establish ownership and trusted publishers and must never receive `latest`.
Use a short-lived granular token through `NODE_AUTH_TOKEN`. A temporary npm
config may reference the environment variable, but must not contain its value:

```sh
export NODE_AUTH_TOKEN='<short-lived-granular-token>'
export NPM_CONFIG_USERCONFIG="$(mktemp)"
trap 'rm -f "$NPM_CONFIG_USERCONFIG"' EXIT
printf '%s\n' '//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}' > "$NPM_CONFIG_USERCONFIG"

npm publish bootstrap/mineru-darwin-x64 --tag bootstrap --access public
npm publish bootstrap/mineru-darwin-arm64 --tag bootstrap --access public
npm publish bootstrap/mineru-linux-x64-gnu --tag bootstrap --access public
npm publish bootstrap/mineru-linux-arm64-gnu --tag bootstrap --access public
npm publish bootstrap/mineru-win32-x64-msvc --tag bootstrap --access public
npm publish bootstrap/mineru-win32-arm64-msvc --tag bootstrap --access public
npm publish bootstrap/mineru --tag bootstrap --access public
```

Confirm each directory's `package.json` has the exact corresponding name before
publishing. Deprecate every unusable bootstrap version:

```sh
npm deprecate '@alexsun-top/mineru-darwin-x64@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru-darwin-arm64@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru-linux-x64-gnu@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru-linux-arm64-gnu@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru-win32-x64-msvc@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru-win32-arm64-msvc@0.0.1' 'Metadata-only bootstrap; do not use.'
npm deprecate '@alexsun-top/mineru@0.0.1' 'Metadata-only bootstrap; do not use.'
rm -f "$NPM_CONFIG_USERCONFIG"
trap - EXIT
unset NPM_CONFIG_USERCONFIG NODE_AUTH_TOKEN
```

After the automated `0.1.0` OIDC release succeeds, obtain a fresh short-lived
granular token authorized for these packages and remove all bootstrap
dist-tags using a new temporary config. Ongoing releases remain OIDC-only:

```sh
export NODE_AUTH_TOKEN='<fresh-short-lived-granular-token>'
export NPM_CONFIG_USERCONFIG="$(mktemp)"
trap 'rm -f "$NPM_CONFIG_USERCONFIG"' EXIT
printf '%s\n' '//registry.npmjs.org/:_authToken=${NODE_AUTH_TOKEN}' > "$NPM_CONFIG_USERCONFIG"

npm dist-tag rm @alexsun-top/mineru-darwin-x64 bootstrap
npm dist-tag rm @alexsun-top/mineru-darwin-arm64 bootstrap
npm dist-tag rm @alexsun-top/mineru-linux-x64-gnu bootstrap
npm dist-tag rm @alexsun-top/mineru-linux-arm64-gnu bootstrap
npm dist-tag rm @alexsun-top/mineru-win32-x64-msvc bootstrap
npm dist-tag rm @alexsun-top/mineru-win32-arm64-msvc bootstrap
npm dist-tag rm @alexsun-top/mineru bootstrap

rm -f "$NPM_CONFIG_USERCONFIG"
trap - EXIT
unset NPM_CONFIG_USERCONFIG NODE_AUTH_TOKEN
```

## Version and tag contract

Before release, the Cargo workspace version, npm manifest version, both npm
lockfile root versions, and intended PyPI wheel version must all be the same
stable `X.Y.Z`. Commit those synchronized files. The GitHub Release tag must be
exactly `vX.Y.Z`, point to the release commit, and be neither a draft nor a
prerelease.

## Local preflight

The temporary `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` (`quick-xml`) exception is mitigated reachability, not fixed or unreachable: mandatory full OOXML preflight runs before Office conversion. It expires on 2026-09-30 and must be reviewed or removed then. Native macOS has no reliable no-entitlement hard memory cap, so hostile Office processing there requires an external VM or container memory boundary.

Run from the repository root unless a subshell changes directory:

```sh
cargo +1.89.0 fmt --all -- --check
cargo +1.89.0 check --locked -p mineru --all-targets
cargo +1.89.0 check --locked -p mineru-python
cargo +1.89.0 check --locked -p mineru-node
cargo +1.89.0 metadata --locked --no-deps
cargo +1.89.0 package --locked -p mineru --list
cargo +1.89.0 publish --locked -p mineru --dry-run

python3.9 -m venv /tmp/mineru-release-venv
/tmp/mineru-release-venv/bin/pip install maturin
/tmp/mineru-release-venv/bin/maturin build --release --locked --manifest-path bindings/python/Cargo.toml
/tmp/mineru-release-venv/bin/pip install --force-reinstall target/wheels/mineru_rs-*.whl
/tmp/mineru-release-venv/bin/python -m unittest discover -s bindings/python/tests -p 'test_*.py'

# index.js is maintained and hardened; index.d.ts is generated.
(cd bindings/node && npm ci && npm run build && git restore --source=HEAD -- index.js && git diff --exit-code -- index.d.ts)
# Stage the current-target addon and mineru-office-convert exactly as in CI's
# node-binding job (helper requires --features office):
cargo build --release --bin mineru-office-convert --features office
# Install that package under node_modules, then run:
(cd bindings/node && npm test)

python3 .github/scripts/verify_release.py self-test
python3 .github/scripts/attach_release_assets.py self-test

/tmp/mineru-release-venv/bin/mineru --help
/tmp/mineru-release-venv/bin/mineru-rs --help
(cd bindings/node && ./node_modules/.bin/mineru --help && ./node_modules/.bin/mineru-rs --help)
git diff --check
```

Both launcher names must work for each binding. The wheel installs `mineru` and
`mineru-rs`; the npm root package installs the same two names, and the six npm
platform packages install none.

Inspect Cargo metadata to confirm the three packages are `mineru`,
`mineru-python`, and `mineru-node`, the root library target is `mineru`, exactly
the five documented binaries exist, and no package or target is named
`mineru-cli`. Confirm Python wheels are `cp39-abi3` and no raw `linux_*` wheel
is released. Do not build or publish a Python sdist.

## Trigger

Push the synchronized release commit and tag, then publish the matching stable
GitHub Release. The release workflow is triggered only by that published
release. It builds and verifies everything before separate protected jobs
publish crates.io, PyPI wheels, the six npm native packages and npm root
package, and the GHCR image. Creating a tag alone does not publish anything.

`publish-container` runs alongside the other publishing and asset-attachment
jobs after the same `release-ready` gate, so an image failure does not block
the other registries. It builds the repository Dockerfile for `linux/amd64` and
`linux/arm64` and publishes `ghcr.io/agentsyaml/mineru-rs` with the exact
release version, `major.minor`, `major`, and `latest` tags. Buildx records the
multi-architecture digest in the job summary and publishes maximal provenance
and an SBOM.

## Release asset attachment

After every existing verification passes, `attach-assets` attaches the verified
crate, all five wheels, all seven npm tarballs, and a `SHA256SUMS` file covering
exactly those artifacts to the published GitHub Release. It re-runs the crate,
wheel, and npm checks against the sealed artifacts before uploading anything.

The job holds `contents: write` and nothing else. Uploads are bound to
`github.event.release.id`, and the job fails unless that release's `tag_name`
matches the tag the workflow verified. Attachment runs alongside the publishing
jobs and does not change their dependencies or ordering.

Re-running a release is idempotent: an asset whose name and SHA-256 already
match is skipped, and a same-name asset with a different digest fails the job
instead of being replaced. Nothing uses `--clobber`, and the job never modifies
`latest`, any dist-tag, or the release body and metadata. `SHA256SUMS` is
generated in a separate staging directory so the crate, wheel, and npm
directories continue to match their exact expected payloads.

This document specifies commands; it does not assert that bootstrap, preflight,
trusted-publisher setup, or publication has been performed.
