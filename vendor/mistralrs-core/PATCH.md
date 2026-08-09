# PATCH — mistralrs-core

## Origin

- Crate: `mistralrs-core` **0.8.1** (crates.io)
- Source checksum: `a2b8b9e5c94491d9ceeded3a30291cb6af6ae74810a5776dd55e3bbb8b3429d4`
- Published source copied verbatim from the cargo registry; only the files
  noted below are modified. License: MIT (see `LICENSE`, Copyright (c) 2024 Eric
  Buehler).

## Manifest change

`Cargo.toml` — one empty `[workspace]` table added so the crate is a standalone
workspace (cargo refuses to run its unit tests in a non-member directory nested
under the host workspace root). No dependency or feature semantics change.

## Behavior changes (3)

### 1. CPU memory probe fallback

`src/utils/memory_usage.rs` — `usable_cpu_memory` + `get_memory_available`
(`Device::Cpu` arm)

Upstream 0.8.1 reports `sysinfo::System::available_memory()` verbatim. On
macOS with sysinfo 0.36.1 that probe returns 0 while `total_memory()` is
correct (e.g. 16 GB), so an auto device map with `device_map: Cpu` sized
itself as 0 MB.

The patch adds a pure helper `usable_cpu_memory(available, total)` that
returns `available` unchanged when non-zero and falls back to `total` only
when the probe is exactly 0. Normal platforms and the CUDA/Metal paths are
untouched. Known ceiling: on 0-reporting platforms the fallback may overstate
how much memory is actually idle. Marked with a `ponytail:` comment.

Removal condition: delete the fallback when sysinfo's `available_memory()`
is reliable on macOS (or when upstream ships its own 0-fallback).

Regression test:

    cargo test -p mistralrs-core --lib utils::memory_usage

### 2. Generation decodes special tokens

`src/pipeline/sampling.rs` — `finish_or_add_toks_to_seq`

Upstream 0.8.1 decodes generated tokens with `include_special =
seq.tools.is_some() || seq.needs_special_tokens()`, so special tokens are only
kept for tool-call or think-tag paths. The MinerU tokenizer declares its
layout/table protocol markers (`<|box_start|>`, `<|ref_start|>`, `<nl>`,
`<ched>`, ...) as special tokens, so a normal chat request silently dropped
them and the layout reply parsed to zero blocks.

The patch makes generation decode always preserve special tokens via the
constant `INCLUDE_SPECIAL_TOKENS_IN_GENERATION = true`. EOS and explicit stop
tokens still never enter the completion bytes: `Sequence::add_token`
(`src/sequence.rs`) skips appending `completion_bytes` when the token stopped
the sequence (`StopReason::Eos` / `StopTok`), so stop delimiters cannot leak
  into the reply. No toktrie code, no tools hack, no chat-template change.

### 3. Qwen2-VL MRoPE local rope indices (upstream PR #2301 backport)

`src/vision_models/qwen2vl/mod.rs` — `forward` `ropeidx_attn_mask_indices`

Upstream 0.8.1 builds the per-batch gather indices as the KV-cache global
range `offset..offset+len` (zipping `seqlen_offsets`), but gathers against the
local input row. On the first decode step `seq.len == full prompt len` with
`offset > 0`, the range's first index equals (or exceeds) the row dim, so
`index_select` fails. Observed on a real MinerU page (prefill len 1395, first
decode offset 1394 → "invalid index 1395 with dim size 1395").

The patch is the exact backport of upstream PR #2301
(`Qwen2VLModel::get_rope_index` fix), commit
`1b411b1fb628a15f4e1a76d7da28eb5cff6c5115`:
`ropeidx_attn_mask_indices` is now the per-batch local range `0..len` via a
pure helper `ropeidx_local_indices(len)`. `seqlen_offsets` is no longer zipped
into these indices (the forward `position_ids = seqlen_offsets + delta` math is
unchanged). Branch 1 still recomputes delta over full tokens, so decode
`offset + delta` stays token-for-token equivalent to the HF/master full
position formula and the delta is stable as generation grows.

**CLI applicability boundary**: correct and sufficient for the current CLI's
single-batch, single-image, single-turn, full-prompt-prefill path. Known
ceiling (unreachable from the current CLI, marked with a `ponytail:` comment):
batch>1 mixed multimodal prompts, prefix-cache prompt trim, and any future
chunked prefill that crosses an image region still need the global-offset /
prompt-position-cache semantics of master.

Master removal condition: a future upstream release ships the PR #2301
semantics (or the master prompt-position-cache rewrite); then this backport and
the helper can be deleted with the rest of the vendor patch.

Regression tests:

    cargo test -p mistralrs-core --lib vision_models::qwen2vl

covers local indices never exceeding `len` (incl. the observed len 1395 /
offset 1394 point), per-batch length variation, and a pure-algebra decode
`offset + delta == full position formula` identity with a stable delta.

## Upstream status (why we patch instead of upgrading)

- mistral.rs **PR #1950** covers only the think-tag path — not MinerU tokens.
- mistral.rs **PR #2319** only fixes `special=false` `<...>` tokens.
- llguidance **issue #361** tracks a general request-level
  skip-special-tokens switch; no released mistral.rs exposes it.
- No version of mistral.rs 0.8.1–master has a request-level
  `include_special`/`skip_special_tokens` switch.

## Removal conditions

Delete this patch and the constant when:

1. The pinned mistral.rs version exposes a request-level
   `skip_special_tokens`/`include_special` switch, **and**
2. `mineru-mistralrs` sets it to `false`-skipping (i.e. keep special tokens)
   for the MinerU layout request, **and**
3. The weightless regression test below still passes against the unpatched
   upstream decode path.

Then `vendor/mistralrs-core` and the root `[patch.crates-io]` entry can be
removed in the same change.

## Packaging note (cargo limitation)

The root package declares `include = ["vendor/mistralrs-core/**"]` so the
vendored source would ship with a published `mineru` (needed because the
manifest's `[patch.crates-io]` path is relative). However, cargo refuses to
package any directory that contains a `Cargo.toml` (nested-package detection:
valid, garbage, or empty manifests all trigger it), so `cargo package` cannot
ship this vendored crate regardless of the `include` glob. Cargo also strips the
root `[patch.crates-io]` section from the packaged manifest, so a published
`mineru` with `mistralrs` enabled resolves the unpatched upstream core.
Consumers needing this backend must build from this source checkout or apply an
equivalent patch in their own workspace. Shipping it directly to published
consumers would require publishing the patched crate to a registry.

## Regression test

`src/pipeline/sampling.rs` `tests::generation_decode_preserves_mineru_special_tokens`
builds a minimal tokenizer with a `<|box_start|>` special token, mirrors the
production `build_llg_factory` trie construction, and asserts
`decode_ext(..., false)` drops the token while
`decode_ext(..., INCLUDE_SPECIAL_TOKENS_IN_GENERATION)` keeps it.

Run with:

    cargo test -p mistralrs-core --lib pipeline::sampling
