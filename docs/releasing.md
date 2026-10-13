# Release maintenance

This guide covers maintainer preflight and the protected GitHub Actions release
workflow in `.github/workflows/release.yml`. Run repository commands from the
root. Release builds use Rust 1.89.0 and pinned local Office sources. Python
3.9+ can prepare those sources and build the local wheel; use Python 3.11+ for
the audit checker, which reads TOML with `tomllib`. The workflow builds five
Python wheels, six native npm packages plus the npm root package, six binary
archives, and a two-architecture GHCR image.

## Current release boundary

Crates.io publication is suspended because the root workspace's local
`[patch.crates-io]` repairs are not inherited by downstream consumers. Do not
enable or perform crate publication until the published dependency chain is
safe. Before dispatch, inspect GitHub tags/releases and query each enabled
registry for the candidate `X.Y.Z`; reuse an existing version only when its
artifacts match the workflow's expected identity and digests. CI success alone
does not establish publication. PyPI, npm, the CLI container, and GitHub binary
releases are separate outputs. See
[`office-xml041/PROVENANCE.md`](../tools/deps/office-xml041/PROVENANCE.md) for
the local dependency source records and constraints.

## Before triggering a release

1. Prepare the ignored Office sources before any Cargo command. Python 3.9+
   suffices for these source tools; preparation may download the pinned archives,
   while verification is offline:

   ```sh
   python3 tools/deps/prepare_office.py prepare
   python3 tools/deps/prepare_office.py verify
   python3 -m unittest discover -s tools/deps -p 'test_*.py'
   python3 tools/deps/office_xml_parity.py
   ```

2. Synchronize the workspace version, workspace package entries in
   `Cargo.lock`, `bindings/node/package.json`, both root version entries in
   `bindings/node/package-lock.json`, and the expected version constants in
   maintained `bindings/node/index.js` to stable `X.Y.Z`. Python wheel version
   is derived from the workspace; do not edit separate Python version metadata.
   Verify root package and target identities:

   ```sh
   cargo +1.89.0 metadata --locked --no-deps --format-version 1
   ```

   Verify all six native platform-package manifests use `X.Y.Z`. They are
   generated during release staging; this checkout has no tracked per-platform
   `package.json` files to edit.

3. Complete every local check below before committing and pushing:

   ```sh
   cargo +1.89.0 fmt --all -- --check
   cargo +1.89.0 test --locked -p mineru --all-targets --features office
   cargo +1.89.0 check --locked -p mineru --all-targets --no-default-features
   RUSTDOCFLAGS="-D warnings" cargo +1.89.0 doc --locked -p mineru --no-deps
   cargo +1.89.0 clippy --locked -p mineru --all-targets --features office
   cargo build --bins --features office
   ```

In addition to the Cargo checks, run the audit check and six self-tests below;
Python 3.11+ is required by the audit policy checker. Install cargo-audit under
the ignored build directory rather than replacing a globally installed tool,
and pass its exit status to the checker:

```sh
cargo +1.89.0 install cargo-audit --version 0.22.2 --locked --root target/tools
audit_report="$(mktemp)"
trap 'rm -f "$audit_report"' EXIT
set +e
target/tools/bin/cargo-audit audit --json > "$audit_report"
audit_status=$?
set -e
python3.11 .github/scripts/check_cargo_audit.py check \
  --report "$audit_report" --lockfile Cargo.lock --audit-status "$audit_status"
```

Run all six release-script self-tests:

```sh
python3 .github/scripts/verify_release.py self-test
python3 .github/scripts/stage_binding_artifacts.py self-test
python3 .github/scripts/attach_release_assets.py self-test
python3.11 .github/scripts/check_cargo_audit.py self-test
python3 .github/scripts/verify_container_release.py self-test
python3 .github/scripts/container_smoke.py --self-check
```

4. Confirm full CI passes for the commit on protected `main`. Release dispatch
   separately requires a completed successful **push** CI run for exactly that
   commit and branch. Do not dispatch from a branch or an unverified SHA.

For a fuller local binding preflight, build/install the Python wheel and run
`python -m unittest discover -s bindings/python/tests -p 'test_*.py'`; in
`bindings/node`, run `npm ci`, `npm run build`, and `npm test`. `index.js` is
maintained; restore any generated change and confirm `index.d.ts` is unchanged.

The Python wheel smoke sequence used for local preflight is:

```sh
python3.9 -m venv /tmp/mineru-release-venv
/tmp/mineru-release-venv/bin/pip install maturin
/tmp/mineru-release-venv/bin/maturin build --release --locked --manifest-path bindings/python/Cargo.toml
/tmp/mineru-release-venv/bin/pip install --force-reinstall target/wheels/mineru_rs-*.whl
/tmp/mineru-release-venv/bin/python -m unittest discover -s bindings/python/tests -p 'test_*.py'
```

The automated Node release build uses Node 24.18.1 and npm 12.0.2; package
`engines` requires Node 18 or newer. `npm ci` must use the committed
`bindings/node/package-lock.json`.

## Dispatch and publication

After synchronized version changes pass CI, commit and push them to protected
`main`. Select that exact tip and dispatch the workflow from `main`:

```sh
commit='<40-lowercase-sha>'
test "$(git rev-parse origin/main)" = "$commit"
gh workflow run release.yml --ref main -f version=X.Y.Z -f commit="$commit"
```

The workflow checks that the requested commit equals the dispatch ref's
`GITHUB_SHA`, verifies exact successful push-CI evidence, and rejects an
existing `vX.Y.Z` tag or release before building. Protected environments named
`pypi`, `npm`, and `ghcr` gate registry jobs; PyPI and npm use trusted-publisher
OIDC, and GHCR uses a job-scoped `GITHUB_TOKEN` with `packages: write`. Do not
store registry tokens in the repository, GitHub secrets, `.npmrc`, or Cargo
configuration. There is no active crates.io publication job.

The workflow verifies and publishes five wheels, seven npm tarballs, and the
multi-platform container only after its build and artifact checks succeed. The
source-build job records evidence from a fresh tracked-source snapshot; it is
not a `.crate` package and is not uploaded as a binary release asset.

GitHub Release assets are exactly six main-library archives and `SHA256SUMS`:

```text
mineru-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
mineru-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz
mineru-vX.Y.Z-x86_64-apple-darwin.tar.gz
mineru-vX.Y.Z-aarch64-apple-darwin.tar.gz
mineru-vX.Y.Z-x86_64-pc-windows-msvc.zip
mineru-vX.Y.Z-aarch64-pc-windows-msvc.zip
SHA256SUMS
```

Each archive contains `mineru` and `mineru-office-convert` in one root
directory (`.exe` on Windows). Verify downloads with `sha256sum -c SHA256SUMS`
or the platform equivalent. Crates, wheels, and npm tarballs remain registry
artifacts, not GitHub Release uploads.

`publish-release` creates and publishes a draft only after publication and
asset verification pass. On failure it removes only the draft and exact tag
created by that run; it preserves anything that no longer matches those exact
identities. Published releases and tags are not replaced. Reruns fail closed
for conflicting assets: a matching name and digest is skipped, while a
same-name/different-digest asset fails instead of being overwritten. The
workflow does not use `--clobber` or modify the release body. GitHub's automatic
source-code archives are not uploaded assets or Rust `.crate` packages.

## One-time registry configuration

Protect `main` and restrict release dispatch to it. Configure protected GitHub
environments `pypi`, `npm`, and `ghcr` with required reviewers. Configure
trusted publishers for PyPI project `mineru-rs` and all seven npm records under
`@alexsun-top`, using repository `agentsyaml/mineru-rs`, workflow `release.yml`,
and the corresponding `pypi` or `npm` environment. GHCR publication uses the
job-scoped GitHub token. The crates.io `mineru` bootstrap and publication
instructions are intentionally omitted: the current dependency-boundary issue
keeps crate publishing suspended.
