# Compatibility

`mineru` is pinned to the MinerU 3.4.4 `vlm-http-client` baseline:

- MinerU: `mineru==3.4.4`, `mineru-3.4.4-py3-none-any.whl`, release tag
  `mineru-3.4.4-released`, commit
  `0dfc9460cd9ab693b9af60ae3fbffd7bc111b062`, SHA256
  `d4d678539782a7683d998e2914a52d96b5720676ce65658b29666b1f4d9dfd13`.
- Companion protocol wheel: `mineru-vl-utils==1.0.5`,
  `mineru_vl_utils-1.0.5-py3-none-any.whl`, SHA256
  `cf910e68f0607634e61b613b7f5992daf604bf80b400d81e7b9f0f117b7c3c15`.

MinerU 3.4.4 metadata specifies only `mineru-vl-utils>=1.0.5,<2`. Pinning
`1.0.5` is this project's reproducibility choice; it does not claim that the
MinerU wheel itself was lockfile-pinned.

## Scope

The source-audited MinerU 3.4.4 CLI baseline is frozen in the intended tracked
artifacts `tests/fixtures/official/mineru_3.4.4_cli_contract.json` and
`tests/official_cli_contract.rs`. They record the environment inventory,
schema, and critical distinctions. The current Rust CLI is **NOT FULL DROP-IN**
compatible with MinerU 3.4.4.

This compatibility declaration covers OpenAI-compatible VLM HTTP transport for
PDF pages, layout and block extraction, postprocessing, outputs, and layout
preview. It excludes local inference, `mineru-api`, non-PDF input, and any
general or full MinerU 3.4.4 compatibility claim.

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
