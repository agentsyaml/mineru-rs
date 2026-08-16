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

### Why not MinerU 4.x (alpha)

MinerU 4.0.0a1–a6 (2026-07/08) removed the `vlm-http-client` backend value and
renamed the transport baseline to `http-client`; the snapshot protocol this
project implements is not audited against 4.x and is **not** covered by this
declaration. 4.x is pre-release and its backend names and semantics are still
moving between alphas, so it is not pinned here. If 4.0 final ships with a
frozen backend contract and real users need it, a second, separate contract
baseline would be evaluated then. 4.x model hosting (e.g. GGUF repos on
personal mirrors) is likewise not adopted: GGUF sources stay user-chosen via
`LLAMA_ARG_HF_REPO`.

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

The `hybrid-http-client` backend is admitted at the protocol layer only. In API
mode (`--api-url`) it is passed through verbatim and the server decides the
semantics; in direct mode it is an alias for `vlm-http-client` (this build has
no local layout/OCR/formula models — every run warns on stderr) with identical
behavior. No local inference, no lightweight hybrid pipeline, and no
`effort` semantics are implemented for it; anything beyond the VLM-HTTP
transport is explicitly excluded from this declaration.

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

## Legacy office formats (`.doc`/`.ppt`/`.xls`/`.odt`/`.rtf`/`.epub`/`.ods`/`.odp`/`.csv`)

The optional `legacy-office` feature extracts Markdown text from legacy office
formats through the pure-Rust `anydoc` crate in the `mineru-office-convert`
helper, with no VLM service required. It is **outside** the MinerU 3.4.5 VLM
scope: output is a `{stem}.md` text file only, with no layout JSON, no
`content_list.json`, and no cropped assets. Image references remain as
unresolved Markdown references. Output is not validated against any MinerU
contract and makes no compatibility claim. The `mineru-api` server does not
accept these formats.
