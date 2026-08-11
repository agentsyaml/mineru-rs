# Agent Rules

- If you have read permissions and `~/AGENTS.md` exists, you should follow it without exception;
- This document must not be modified. Modifications to `AGENTS.md` in each domain should be made with caution; if there are issues with this document, please notify the user to make the necessary changes;

## Commit and Push Rules

- Before every commit and push, run the complete CI-equivalent check locally and make sure every step passes. The CI jobs that can be reproduced locally are:
  - `cargo +1.89.0 fmt --all -- --check`
  - `cargo +1.89.0 test --locked -p mineru --all-targets --features office`
  - `cargo +1.89.0 check --locked -p mineru --all-targets --no-default-features`
  - `RUSTDOCFLAGS="-D warnings" cargo +1.89.0 doc --locked -p mineru --no-deps`
  - `cargo +1.89.0 clippy --locked -p mineru --all-targets --features office` (no new warnings)
  - `cargo build --bins --features office`
  - cargo-audit: `cargo audit --json` piped through `.github/scripts/check_cargo_audit.py check`
  - All release-script self-tests: `python3 .github/scripts/verify_release.py self-test`, `stage_binding_artifacts.py self-test`, `attach_release_assets.py self-test`, `check_cargo_audit.py self-test`, `verify_container_release.py self-test`, `container_smoke.py --self-check`
- When changing Rust source or Cargo files, do not rely on the last commit's checks: re-run the full check set above on the new working tree before pushing.
- Every commit and push requires an explicit instruction from the user. Never commit or push proactively or as a side effect of other work.
