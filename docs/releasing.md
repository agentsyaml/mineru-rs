# Releasing

Releases are coordinated across crates.io `mineru`, PyPI `mineru-rs`, npm
`@alexsun-top/mineru`, and the GHCR container image
`ghcr.io/agentsyaml/mineru-cli`. Registry credentials must be short-lived
environment variables or trusted-publisher OIDC credentials. Never store a
registry token in the repository, GitHub secrets, `.npmrc`, or Cargo
configuration.

## One-time repository and registry setup

Protect `main` and allow release dispatch only from that protected branch. The
`workflow_dispatch` definition is selected from `main`; the workflow fails
closed unless its `GITHUB_SHA` equals the requested commit. Create protected
GitHub environments named `crates-io`, `pypi`, `npm`, and `ghcr`, each with
required reviewers and the appropriate OIDC trusted-publisher controls. Do not
restrict these environments to release tags.

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

## Published GHCR image contract

The existing `publish-container` job publishes the Rust API image
`ghcr.io/agentsyaml/mineru-cli` for `linux/amd64` and `linux/arm64`. Its
release binaries are built with `office,legacy-office`; the image's default
command listens on container port `8000`, serves `GET /health`, runs as its
configured non-root user, and writes task output under `/app/output`.

The image bundles Rust binaries only. It does not contain Python,
`mineru==4.0.0a6`, or model assets, so direct official Hybrid needs a separately
prepared environment explicitly supplied to the image and API Hybrid remains
fail-closed. Supply `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and
`MINERU_VL_API_KEY` for an external VLM provider. Documented local publication
is `127.0.0.1:8000:8000`; broader exposure belongs on a private network or
behind an authenticated reverse proxy. This section describes the immutable
workflow contract; it does not change that workflow.

## Non-user-facing 0.0.1 bootstrap

Perform bootstrap from a temporary checkout or branch with the Cargo workspace
version changed to `0.0.1`. Main remains at the current workspace version (`0.3.0`).

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
stable `X.Y.Z`. Commit and push those synchronized files; do not change
manifests just to prepare the workflow.

## Local preflight

The temporary `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` (`quick-xml`) exception is mitigated reachability, not fixed or unreachable: mandatory full OOXML preflight runs before Office conversion. It expires on 2026-09-30 and must be reviewed or removed then. Native macOS has no reliable no-entitlement hard memory cap, so hostile Office processing there requires an external VM or container memory boundary.
The registry-reported yanked transitive `arrayref` 0.3.9 is a reviewed lock exception. It expires on 2026-09-30 and must be reviewed or removed then.

Run from the repository root unless a subshell changes directory:

```sh
cargo +1.89.0 fmt --all -- --check
cargo +1.89.0 check --locked -p mineru --all-targets --no-default-features
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
# Install that package under node_modules, then run:
(cd bindings/node && npm test)

python3 .github/scripts/verify_release.py self-test
python3 .github/scripts/attach_release_assets.py self-test
python3 .github/scripts/verify_container_release.py self-test

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
the three documented binaries exist (`mineru`, `mineru-api`, and
`mineru-office-convert`), and no
package or target is named `mineru-cli`. Confirm Python wheels are `cp39-abi3`
and no raw `linux_*` wheel is released. Do not build or publish a Python sdist.

## Trigger and publication sequence

1. Bump the synchronized version, commit, and push it to protected `main`.
2. Confirm the selected commit is the current protected-main tip, then wait for
   its completed successful **push** CI run:

   ```sh
   commit='<40-lowercase-sha>'
   test "$(git rev-parse origin/main)" = "$commit"
   gh workflow run release.yml --ref main -f version=X.Y.Z -f commit="$commit"
   ```

   The workflow normalizes the full hexadecimal SHA, rejects a workflow ref
   whose `GITHUB_SHA` differs from it, and requires successful push CI with
   exactly that `head_sha` and `main` branch before any registry publication.
   It also rejects an existing `vX.Y.Z` tag or release.

The workflow keeps crates.io, PyPI, npm (six native packages plus root), and
GHCR publication: separate protected jobs publish crates.io, PyPI wheels, the
six npm native packages plus npm root package, and the `mineru-cli` container
image (`publish-container` builds `Dockerfile.release` for `linux/amd64` and
`linux/arm64` and publishes `ghcr.io/agentsyaml/mineru-cli`).

## GitHub Release assets

The GitHub Release has exactly seven uploaded assets (replace `X.Y.Z` with the
release version): the six main-library platform archives and `SHA256SUMS`:

1. `mineru-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
2. `mineru-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`
3. `mineru-vX.Y.Z-x86_64-apple-darwin.tar.gz`
4. `mineru-vX.Y.Z-aarch64-apple-darwin.tar.gz`
5. `mineru-vX.Y.Z-x86_64-pc-windows-msvc.zip`
6. `mineru-vX.Y.Z-aarch64-pc-windows-msvc.zip`
7. `SHA256SUMS`

Each main-library archive has one root directory and exactly `mineru` plus
`mineru-office-convert` (`.exe` on Windows). Verify the downloaded files with
`sha256sum -c SHA256SUMS` (or the platform equivalent). Crates, wheels, and npm
tarballs remain registry artifacts and are never attached to the GitHub Release.

`publish-release` creates a draft only after the full publication graph passes,
uploads and verifies this exact set, then publishes it. A failure before publish
deletes only that newly created draft. Published releases and tags are immutable:
reruns fail rather than delete, recreate, or replace them. GitHub may show
automatic `Source code (zip)` and `Source code (tar.gz)` links; those are GitHub
generated source links, not uploaded release assets and not Rust `.crate`
packages.

Re-running a release is idempotent: an asset whose name and SHA-256 already
match is skipped, and a same-name asset with a different digest fails the job
instead of being replaced. Nothing uses `--clobber`, and the job never modifies
`latest`, any dist-tag, or the release body and metadata. `SHA256SUMS` is
generated in a separate staging directory so the crate, wheel, npm, and binary
directories continue to match their exact expected payloads.

This document specifies commands; it does not assert that bootstrap, preflight,
trusted-publisher setup, or publication has been performed.
