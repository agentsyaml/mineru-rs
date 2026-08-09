# Agent Rules

- If you have read permissions and `~/AGENTS.md` exists, you should follow it without exception;
- This document must not be modified. Modifications to `AGENTS.md` in each domain should be made with caution; if there are issues with this document, please notify the user to make the necessary changes;

## Commit and Push Rules

- Before every commit, run the complete CI-equivalent check and make sure it passes: `cargo +1.89.0 fmt --all -- --check`, `cargo build --bins --features office`, `cargo test`, and `cargo clippy --all-targets --all-features` (no new warnings). Do not commit while any of these fail.
- Every commit and push requires an explicit instruction from the user. Never commit or push proactively or as a side effect of other work.
