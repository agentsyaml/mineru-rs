# Compatibility

## Route and format matrix

| Surface | Upstream/runtime | Inputs and behavior | Outputs |
| --- | --- | --- | --- |
| CLI `vlm-http-client` (default) | MinerU 3.4.5 VLM protocol; external OpenAI-compatible service | PDF and eight image formats; raw local layout API is separate and not integrated | Existing MinerU-style result tree |
| CLI `hybrid-http-client` | Official MinerU 4.0.12 worker; external Python 3.10–3.14 environment | Direct mode only; PDF and eight image formats | `{stem}/hybrid-v4/` official bundle |
| CLI `--backend local` | Rust text extraction; requires the `legacy-office` build feature | Legacy Office formats and clean text PDFs | Markdown only |
| Rich Rust/Python/Node SDK | Rust-native CSV/TSV; checked-in worker contract MinerU 4.0.12 / DocVortex 0.5.16; latest reference 0.6.0 | 28 extensions; native formats require `flash` | Owned results; CSV/TSV render to Markdown or structured content only |
| Official-v1 HTTP API | Rust-native CSV/TSV; checked-in worker contract MinerU 4.0.12 / DocVortex 0.5.16; latest reference 0.6.0 | Upload/job API; PDF, images, and native formats; native requires `flash` | Markdown, middle JSON, structured content, or ZIP |
| Existing legacy bindings/API | MinerU 3.4.5 VLM behavior | Existing input and option contract | Existing output tree or Markdown/warnings |

The 3.4.5 VLM and 4.0.12 official worker are separate routes. The latter's
format support does not widen the former. See the [SDK guide](official-sdk.md)
and [HTTP guide](official-http.md) for their own option and result contracts.

## MinerU 3.4.5 VLM baseline

- MinerU: `mineru==3.4.5`, `mineru-3.4.5-py3-none-any.whl`, release tag
  `mineru-3.4.5-released`, commit
  `fbb1257a555a3fde78ae5aaaa931e3b3f8fb2883`, SHA256
  `4a73b865920bb9109c1b8b1bc46567e296bf0133a67106a04effd219536ae72d`.
- Companion protocol wheel: `mineru-vl-utils==1.0.5`,
  `mineru_vl_utils-1.0.5-py3-none-any.whl`, SHA256
  `cf910e68f0607634e61b613b7f5992daf604bf80b400d81e7b9f0f117b7c3c15`.

MinerU 3.4.5 metadata specifies only `mineru-vl-utils>=1.0.5,<2`. Pinning
`1.0.5` is this project's reproducibility choice; it does not claim that the
MinerU wheel itself was lockfile-pinned.

The bundled Compose image `alexsuntop/mineru:3.4.2` is a separate provider-image
baseline. The MinerU 3.4.5 version above is the VLM protocol baseline for this
compatibility contract, not a Compose image-tag requirement.

### MinerU 4.0.12 direct Hybrid

Direct `hybrid-http-client` is a separate, pinned boundary for the MinerU 4.0.12
release (tag `mineru-4.0.12-released`, commit
`b7f3dd0c5f6e6d66a298239ffd02b328b873918e`). Install with
`python -m pip install mineru==4.0.12`. Upstream requires Python >=3.10,<3.15 and declares
`docvortex>=0.5.16,<1` and `mineru-vl-utils>=2.0.5,<3`. The confirmed wheel
`mineru-4.0.12-py3-none-any.whl` has SHA256
`a3ff8582a4608eb1f4dde55800ac156d59b01ceabd2ab4002bdf8e86306b1eab`. Python, MinerU,
and model assets are not bundled. When no worker mode is specified, selection is
automatic after input preflight: one runnable document invokes the official
`mineru.parser.parse_async` entrypoint in one fresh subprocess, while multiple runnable
documents use a persistent worker. The explicit `per-document` mode invokes one fresh
subprocess per document; the explicit `persistent` mode reuses one worker and loaded model
per direct CLI run, with one active request, sequential documents, and new session creation
after cancellation or crash.
Committed requests are not automatically retried; there is no hard RSS/GPU isolation.
On Windows, worker assignment to a `KILL_ON_JOB_CLOSE` Job Object is fail-closed,
but Tokio spawns the worker before `WindowsJob::attach` runs. A very fast
descendant created in that interval can escape the job, so cleanup is best effort
for that race. The official worker has no hard RSS or GPU quota.
Select either mode explicitly with `--official-worker-mode per-document` or
`--official-worker-mode persistent`. The project-owned JSON envelopes are
`mineru-rs-official-worker/1` and internal `/2`, not official MinerU stdin/stdout
protocols.

Direct Hybrid accepts PDF and official image inputs only. The worker calls the
official `mineru.parser.parse_async` entrypoint with the arguments used by this
project: `path`, `tier`, `ocr_mode`, `image_analysis`, `page_range`, and
`vlm_config`. This is not the complete upstream signature; the optional upstream
`source_context` argument is not supplied by this project. `--tier
flash|basic|standard|advanced` maps 1:1 to the upstream tier; `--effort medium`
maps to tier `standard`, and
`high`/`xhigh` are compatibility aliases for tier `advanced` (`--tier` wins
when both are given, and is rejected on other backends and in API mode). `--url` / `MINERU_VL_SERVER` are accepted at every
tier/effort and feed `VlmConfig.server_url` (`--api-key` and the model name
feed `VlmConfig`); without a URL, upstream uses its local VLM engine. Page
ranges apply to PDF input only and are sent in upstream grammar (`3-r1` for an
open-ended range, where `r1` is the last page, and `3-5` style bounds);
supplying `--start`/`--end` for an image fails fast. A malformed VLM URL
(embedded credentials, query, fragment, whitespace, or bad port) is rejected up
front. The worker mode does not change this input, tier, or output contract.
Model-root and config paths are user configuration; `model.stack`/
`MINERU_MODEL_STACK` was removed before 4.0.10, and the current version does not
support it. Configure `model.small_backend` + `model.vlm.engine` via
`MINERU_CONFIG`, so non-default
`--model-stack` is rejected. `--method auto|txt|ocr` maps directly to the
upstream `ocr_mode` parameter. The `--lang` guard compares the normalized
language, so `en`, `japan`, `chinese_cht`, and `latin` are accepted (they
normalize to `ch`) while other non-default values are rejected, and the legacy
env vars `MINERU_VL_API_KEY` / `MINERU_VL_MODEL_NAME` are no longer injected
into the worker environment — pass credentials through the existing mineru-rs
flags. `MINERU_HOME` is a MinerU home directory, not the model root;
`MINERU_CONFIG` remains supported, and `--official-model-dir` /
`MINERU_MODEL_BASE_DIR` supply the model root injected into the worker as
`MINERU_MODEL_BASE_DIR`.
The separate `{stem}/hybrid-v4/` bundle is validated against the shared DocVortex
protocol — schema `docvortex.middle`, schema_version `2.0` (the old
`_backend=hybrid` marker is gone) — plus safe files and bounded bytes before
atomic publication. It contains `markdown.md`, `middle_json.json`,
`structured_content.json`, optional `model_output.json`, and `images/`;
`content_list.json` is no longer produced.
It is never sent through the MinerU 3.4.5 builders, `official_route`, Office
conversion, or the project-private AnyDoc lane. API Hybrid remains fail-closed.

### Latest upstream reference and rich SDK / official HTTP API

As of 2026-10-23, the latest upstream reference tuple is MinerU 4.0.12,
DocVortex 0.6.0, and mineru-vl-utils 2.0.7. Exact commits, wheel hashes,
platform wheel availability, and the scope of the model-free check are recorded
in [`upstream-baseline.json`](upstream-baseline.json). DocVortex 0.6.0 wheels
cover Linux x86-64/ARM64, Windows x86-64/ARM64, and macOS ARM64 (macOS 14+);
there is no macOS Intel wheel or source distribution. Its Python range is
3.11–3.14. These upstream wheel facts do not narrow this project's Rust or
binding host-platform support.

The latest tuple was checked model-free on macOS ARM64 with CPython 3.14 for
CSV, MHTML, PDF flash/txt, and serialize/restore/Markdown-render operations.
This is a focused smoke check, not a blanket compatibility claim: other
platforms, Python versions, model-backed inference, formats, and tiers were not
tested. Native CSV/TSV protocol, schema, and rendering behavior was unchanged
between DocVortex 0.5.16 and 0.6.0. The 2.0.7 utility release also changes
deduplication, version naming, and priority behavior; those differences remain
pending alignment and are not silently adopted by the existing transport.

The checked-in rich worker compatibility contract remains MinerU 4.0.12 /
DocVortex 0.5.16; MinerU's declared utility range is `>=2.0.5,<3`, not an
assertion that exactly 2.0.5 was previously tested. The latest upstream
reference does not by itself mean the worker integration has migrated.

The Rust `official` module and Python/Node rich binding surfaces accept the 28
extensions listed in the [official SDK guide](official-sdk.md); 19 native
formats require tier `flash`, while PDF and eight image formats support all
four tiers. CSV and TSV currently parse natively in Rust; all other formats use
the external MinerU 4.0.12 / DocVortex 0.5.16 worker (Python 3.10–3.14). CSV/TSV
support UTF-8 with an optional BOM, Markdown and structured-content rendering,
and restoration of their own middle JSON; other render targets are unsupported.
This is a phased migration, not a fully native implementation. The Python
facade itself supports Python 3.9+.

The separate optional `local-inference` Cargo feature exposes a Rust CPU
PP-DocLayoutV2 `LayoutSession` for raw model rows, using an operator-provided
ONNX Runtime shared library and the pinned model cache. It does not replace the
official worker, integrate with the parser/CLI/API routes, or provide official
postprocessing, OCR, tier selection, or full-document inference. Without the
feature, the optional ONNX Runtime dependencies and inference API are not
enabled. Preprocessing follows MinerU 4.0.12: fixed 800x800 cubic resize
(kernel a = -0.75, uint8 fixed-point weights), RGB, and division by 255 with no
mean/std subtraction. Resize output is verified byte-exact against docvortex
0.6.0 golden fixtures (tests/fixtures/layout/resize-golden.json). ONNX input
metadata is validated fail-closed before tensor construction: inputs must
declare float32, the expected rank, and compatible shapes, with no casting.
Postprocessing remains unimplemented, so output parity is not claimed.

The legacy `run`/`parse` APIs remain unchanged; rich result types are additive.
See the [SDK guide](official-sdk.md) for binding build instructions and the
[release guide](releasing.md#current-release-boundary) for registry availability
and publication constraints.

The official-v1 upload/job service is a separate opt-in interface alongside
the existing server routes. Its accepted input, limits, and four output formats
are documented in the [HTTP API guide](official-http.md).

Rust library consumers of the 3.4.5 VLM lane should note one public-API break
shipping with this release: `VlmBatchPriority::PerItem` was removed in favor of
`All(Option<i32>)` with `incremental_priority`; batch clients needing per-item
priorities should construct them per request instead.

## Scope

The source-audited MinerU 3.4.5 CLI baseline is recorded in
`tests/fixtures/official/mineru_3.4.5_cli_contract.json` and
`tests/official_cli_contract.rs`. The current Rust CLI is not a full drop-in
replacement for MinerU 3.4.5.

This compatibility declaration covers OpenAI-compatible VLM HTTP transport for
PDF pages, layout and block extraction, postprocessing, outputs, and layout
preview. It excludes integrating local inference, `mineru-api`, non-PDF input,
and any general or full MinerU 3.4.5 compatibility claim.

The `hybrid-http-client` token is not an alias for `vlm-http-client`, and it
does not invoke the project-private AnyDoc native lane. Direct mode uses the
official 4.0.12 worker; API mode remains explicitly unsupported. The default
`vlm-http-client` remains the existing 3.4.5 VLM-only path.

Validation is semantic and structural only. It never promises byte-identical
PDFs, images, or JSON. In particular, a layout preview intentionally changes
its PDF serialization. Processing 5,000+ pages is high-memory best effort,
not a general safety guarantee.

The semantic projection compares page and block ordinal/type; block bounding
boxes normalized to displayed page coordinates (within `0.02`); Markdown with
whitespace normalized; ordered content-list type plus normalized text/content;
and existence of every referenced asset. Preview PDFs must retain page count
and CropBox geometry. It intentionally ignores PDF, image, and JSON bytes,
JSON key order, filenames, and private metadata.

## Published crate dependency advisories

The crates.io package resolves its Office dependencies from the registry. The
workspace's `[patch.crates-io]` repairs are local build inputs and never reach
consumers, so enabling `office` or `legacy-office` pulls dependency versions
with open advisories. Default features and `local-inference` are clean.

`office` (13 findings): `quick-xml` 0.37.5 and 0.38.4 each carry
RUSTSEC-2026-0194 and RUSTSEC-2026-0195; `zip` 0.6.6, 2.6.1, and 8.6.0 each
carry RUSTSEC-2026-0367, RUSTSEC-2026-0368, and RUSTSEC-2026-0369.
`legacy-office` (3 findings): `zip` 8.6.0 with the same three.

Reachability, audited against the resolved sources:

- The nine `zip` findings all concern `ZipArchive::extract` and
  `ZipStreamReader::extract`. None of the six Office dependencies calls either
  API, and this crate extracts archives with `zip` 9.0.5 through its own bounded
  reader. Those findings are not reachable through this crate.
- The four `quick-xml` findings are parsing denial of service: quadratic
  duplicate-attribute checking and unbounded namespace-declaration allocation.
  They are reachable through `office2pdf`, which parses untrusted OOXML. Office
  conversion runs in a separate helper process bounded by input, output,
  wall-clock, CPU, and address-space limits, which caps the impact but does not
  remove the advisory.

Upstream is unresolved on both paths. `citationberg` merged a `quick-xml` 0.41
bump in July 2026 but has not released it, and it reaches this crate through
`typst` -> `hayagriva`. `anydoc` has an unmerged `zip` 9 pull request and still
declares `zip` 8.6.0. Both must land before this section can be removed.

## Rust API container image

The GHCR image is `ghcr.io/agentsyaml/mineru-cli`. Verify tag availability
before pulling; see the [release guide](releasing.md#current-release-boundary).
It runs the Rust `mineru-api` default command as a non-root user on container port `8000`, serves
`GET /health`, and uses `/app/output` for task output. Configure its external
VLM with `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and
`MINERU_VL_API_KEY`; the image performs no local inference. Publish it to
loopback first with `127.0.0.1:8000:8000`; broader exposure requires a private
network or an authenticated reverse proxy because the API has no built-in
authentication or task ownership isolation.

The release workflow builds binaries with `office,legacy-office`, but the stock image
contains Rust binaries only: no Python, `mineru==4.0.12`, or model assets.
Direct official Hybrid therefore needs a separately prepared environment
explicitly supplied to the image, and API Hybrid remains fail-closed. This
container boundary adds no compatibility claim beyond the protocol scope above.

## Native PDF Markdown and legacy office formats

With the optional `legacy-office` feature, the project-private `backend=local`
lane uses the bundled Rust `mineru-office-convert` helper to run the public
AnyDoc `Format::Pdf` / `to_markdown_bytes` PDF route in an isolated child for
conservative clean text PDFs. The helper invokes no Python, Microsoft
Office/LibreOffice, VLM/model, or network request. The internal assessment is
schema-versioned and records page count,
stable acceptance/reason codes, and provenance (`anydoc::Format::Pdf via
pdf-inspector`). It rejects scanned, mixed, OCR-needed, encoding-problem,
complex-layout, low-confidence, empty, and low-quality inputs rather than
fabricating official output. Accepted output is only
`{stem}/native/{stem}.md`; it contains no `document.json`, `middle.json`,
`content-list`, preview, or assets. Native local PDF page selection is not
supported. This lane is not the official MinerU `hybrid-engine` or the 4.0.12
local-model worker.

## Legacy office formats (`.doc`/`.ppt`/`.xls`/`.odt`/`.rtf`/`.epub`/`.ods`/`.odp`/`.csv`)

The optional `legacy-office` feature provides two direct-CLI lanes. Explicit
`backend=local` uses the bundled Rust `mineru-office-convert` helper to run the
pure-Rust `anydoc` crate in an isolated child, with no Python, Office
application, VLM/model, or network service; it is **outside** the MinerU 3.4.5
VLM scope and writes a `{stem}.md` text file only, with no layout JSON or
cropped assets. Non-local direct mode uses the same isolated helper to turn the
AnyDoc Markdown into a bounded text-only PDF, emits a per-document conversion warning, and
sends that PDF through the existing PDF/VLM route. The source-to-PDF step may
lose original layout, images, tables, formulas, or macros, and may replace
non-ASCII characters with `?`, so it makes no Office-layout compatibility claim;
conversion failures recommend saving as DOCX/XLSX/PPTX with Microsoft Office or
LibreOffice. The `mineru-api` server does not accept these formats.
