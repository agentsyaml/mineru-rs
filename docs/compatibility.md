# Compatibility

`mineru` is pinned to the MinerU 3.4.5 `vlm-http-client` baseline:

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

### MinerU 4.0.0a6 direct Hybrid

Direct `hybrid-http-client` is a separate, pinned boundary for MinerU 4.0.0a6
at revision `90770107e5287342e7c8234446a262cda5bbd029`. It requires a user-
installed Python environment with exactly `mineru==4.0.0a6`; Python, MinerU,
and model assets are not bundled. The default `per-document` mode invokes the official
`mineru.parser.parse_async` entrypoint in one fresh subprocess per document. The explicit
`persistent` mode reuses one worker and loaded model per direct CLI run, with one active
request, sequential documents, and new session creation after cancellation or crash.
Committed requests are not automatically retried; there is no hard RSS/GPU isolation.
Select it with `--official-worker-mode persistent` or
`MINERU_OFFICIAL_WORKER_MODE=persistent`; the default remains `per-document` and CLI
values take precedence. The project-owned JSON envelopes are
`mineru-rs-official-worker/1` and internal `/2`, not official MinerU stdin/stdout
protocols.

Direct Hybrid accepts PDF and official image inputs only. `medium` keeps the
official `hybrid-http-client` backend but is local-only and needs no remote URL;
`high` and `xhigh` use the same official backend and require an explicit HTTP(S)
URL. The worker mode does not change this input, effort, or output contract. The
`auto|light|full` model stack, model root, and config are user configuration.
The separate `{stem}/hybrid-v4/` bundle is validated for schema `1.0`,
`_backend=hybrid`, safe files, and bounded bytes before atomic publication.
It is never sent through the MinerU 3.4.5 builders, `official_route`, Office
conversion, or the project-private AnyDoc lane. API Hybrid remains fail-closed.

## Scope

The source-audited MinerU 3.4.5 CLI baseline is frozen in the intended tracked
artifacts `tests/fixtures/official/mineru_3.4.5_cli_contract.json` and
`tests/official_cli_contract.rs`. They record the environment inventory,
schema, and critical distinctions. The current Rust CLI is **NOT FULL DROP-IN**
compatible with MinerU 3.4.5.

This compatibility declaration covers OpenAI-compatible VLM HTTP transport for
PDF pages, layout and block extraction, postprocessing, outputs, and layout
preview. It excludes local inference, `mineru-api`, non-PDF input, and any
general or full MinerU 3.4.5 compatibility claim.

The `hybrid-http-client` token is not an alias for `vlm-http-client`, and it
does not invoke the project-private AnyDoc native lane. Direct mode uses the
official 4.0.0a6 worker; API mode remains explicitly unsupported. The default
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

## Published Rust API image

The published GHCR image is `ghcr.io/agentsyaml/mineru-cli`. It runs the Rust
`mineru-api` default command as a non-root user on container port `8000`, serves
`GET /health`, and uses `/app/output` for task output. Configure its external
VLM with `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and
`MINERU_VL_API_KEY`; the image performs no local inference. Publish it to
loopback first with `127.0.0.1:8000:8000`; broader exposure requires a private
network or an authenticated reverse proxy because the API has no built-in
authentication or task ownership isolation.

The published release binaries use `office,legacy-office`, but the stock image
contains Rust binaries only: no Python, `mineru==4.0.0a6`, or model assets.
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
supported. This lane is not the official MinerU `hybrid-engine` or the 4.0.0a6
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
