# Rich official SDK

The rich SDK parses documents with the pinned official MinerU worker and returns
an owned result with JSON, Markdown, and assets. The existing `run`/`parse`
interfaces remain unchanged (`parse` returns Markdown and warnings).

Build the bindings from this checkout to use the rich API. For Python, build
and install the wheel with the commands in the
[release guide](releasing.md#before-triggering-a-release). For Node, run
`npm ci` and `npm run build` in `bindings/node`; `index.js` is maintained, so
restore unintended generated changes and confirm `index.d.ts` is unchanged.
Registry and release availability is covered by the [release guide](releasing.md#current-release-boundary).

## Runtime and compatibility

The worker requires `mineru==4.0.10` and `docvortex==0.5.4` with their upstream
dependencies. Use Python 3.10–3.14: the worker uses PEP 604 union annotations
and upstream MinerU requires `>=3.10,<3.15`. The Python facade itself supports
Python 3.9+. Python and model assets are not bundled. Supply trusted absolute Python,
config, and model-directory paths through `OfficialRuntime` (Rust),
`OfficialRuntime` (Python), or `OfficialRuntimeOptions` (Node). These paths are
runtime configuration, not result data; Rust `Debug` and Python `repr` hide
them. Rust/Python representations redact VLM credentials. Node options are
ordinary caller-owned JavaScript objects, so avoid logging them. Parsed VLM
credentials are not retained in rich result data.

The rich SDK's 4.0.10 worker is separate from the legacy 3.4.5 `run`/`parse`
path.

## Parse inputs and options

`OfficialParser.parse_rich`/`parseRich` accepts a filesystem path or owned bytes
with a filename. The filename on byte input is a format hint, not a source
path. Byte input defaults to an HTML source context with no local resource
root. A trusted path input with no explicit context retains its original source
URI and parent directory rather than inheriting a temporary staging directory.
An explicit `source_context`/`sourceContext` replaces that inferred context.
`local_resource_root` grants local HTML resource reads only beneath that
trusted root. Remote image URLs are not fetched, and fetching a remote document
URL is outside this SDK.

The six parse controls are `tier`, `ocr_mode`, `image_analysis`, `page_range`,
`source_context`, and `vlm_config` (camelCase in Node). Single-parse defaults
are `standard`, `auto`, `true`, all pages, and no VLM replacement. An explicit
`vlm_config` is a complete replacement for ambient VLM settings, not a merge;
it cannot be combined with the legacy VLM URL/key/model aliases.

Core recognizes 28 filename extensions:

| Input group | Extensions | Tier rule |
| --- | --- | --- |
| PDF | `.pdf` | `flash`, `basic`, `standard`, or `advanced` |
| Images (8) | `.png`, `.jpg`, `.jpeg`, `.webp`, `.gif`, `.bmp`, `.tiff`, `.jp2` | All four tiers |
| Native formats (19) | `.doc`, `.docx`, `.ppt`, `.pptx`, `.xls`, `.xlsx`, `.rtf`, `.odt`, `.ods`, `.odp`, `.html`, `.htm`, `.shtml`, `.mhtml`, `.mht`, `.csv`, `.tsv`, `.epub`, `.ofd` | `flash` only |

This does not widen the existing MinerU 3.4.5 tiered VLM route, which retains
its prior PDF/eight-image interface. Other legacy CLI routes, such as Office or
local Markdown conversion, remain separate with their existing behavior.

PDF page ranges are forwarded as the complete upstream expression (including
whitespace); examples include `all`, `3-5`, and the open-ended `3-r1` form, in
which `r1` denotes the last page. Upstream validates the selected pages; Core
does not substitute a bounding interval. A page range on any non-PDF input is
rejected. Image inputs remain tiered and do not accept PDF page ranges.

Batch parsing is ordered and sequential. A failed item is returned in its
original position with its own error; later items are still processed. When a
batch item omits options, PDF/images default to `standard` and native formats
default to `flash`. Explicit item options are preserved.

## Rust

```rust
use mineru::official::{
    OfficialBatchItem, OfficialInput, OfficialParseOptions, OfficialParser,
    OfficialRuntime, OfficialTier,
};

async fn example() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = OfficialRuntime::new().with_python("/trusted/venv/bin/python".into());
    let parser = OfficialParser::new(runtime)?;
    let input = OfficialInput::bytes("table.csv", b"name,value\na,1\n".to_vec());
    let options = OfficialParseOptions::new().with_tier(OfficialTier::Flash);
    let result = parser.parse_rich(input, options).await?;

    let rows = parser.parse_batch_rich(vec![
        OfficialBatchItem::new(OfficialInput::path("first.pdf")),
        OfficialBatchItem::new(OfficialInput::path("second.pdf")),
    ]).await?;
    for row in rows {
        match row.result {
            Ok(result) => println!("{}: {}", row.index, result.markdown()),
            Err(error) => eprintln!("{}: {error}", row.index),
        }
    }
    result.save("out", "table")?;
    parser.shutdown().await?;
    Ok(())
}
```

## Python

```python
import asyncio
from mineru_rs import (
    OfficialJson, OfficialParseOptions, OfficialParser, OfficialRenderFormat,
    OfficialResult, OfficialRuntime, OfficialTier,
)

async def main():
    runtime = OfficialRuntime(python="/trusted/venv/bin/python")
    parser = OfficialParser(runtime)
    result = await parser.parse_rich(
        b"name,value\na,1\n", filename="table.csv",
        options=OfficialParseOptions(tier=OfficialTier.FLASH),
    )
    restored = await OfficialResult.from_json(runtime, await result.to_json(), result.assets)
    rendered = await restored.render(OfficialRenderFormat.STRUCTURED_CONTENT)
    assert isinstance(rendered, OfficialJson)
    restored.save("out", "table")
    await parser.shutdown()

asyncio.run(main())
```

Use `OfficialBatchItem` with `OfficialParser.parse_batch_rich` for ordered
per-item outcomes. Python convenience functions `parse_rich` and
`parse_batch_rich` are also available. The existing `parse` and `run` functions
remain the legacy Markdown/warnings and output-tree interfaces.

## Node.js

The Node API is `OfficialParser`/`parseRich`; legacy `parse` and `run` retain
their existing return values. Input bytes use `{ filename, data }` with a
`Buffer` or `Uint8Array`.

```js
const {
  OfficialParser, OfficialResult, OfficialRenderFormat, OfficialTier,
} = require('@alexsun-top/mineru')

async function main() {
  const parser = new OfficialParser({ python: '/trusted/venv/bin/python' })
  const result = await parser.parseRich(
    { filename: 'table.csv', data: Buffer.from('name,value\na,1\n') },
    { tier: OfficialTier.Flash },
  )
  const restored = await OfficialResult.fromJson(parser, await result.toJson(false), result.assets)
  const rendered = await restored.render(OfficialRenderFormat.StructuredContent)
  if (rendered.kind === 'json') console.log(rendered.value)

  const batch = await parser.parseBatchRich([
    { input: 'first.pdf' },
    { input: 'second.pdf' },
  ])
  for (const item of batch) {
    if (item.error) console.error(item.index, item.error.message)
    else console.log(item.index, item.result.markdown)
  }
  restored.save('out', 'table')
  await parser.shutdown()
}

main().catch(console.error)
```

## Results, assets, and limits

`OfficialResult` owns its middle/model JSON, Markdown, structured content,
warnings, version metadata, and `images/...` assets after worker temporary
directories are removed. Accessor values are copies/owned values. `pages` and
`export_pages` return page data; the latter is a deep copy. `from_dict` and
`from_json` accept middle JSON and a separate owned asset map/list. `to_dict`
and `to_json` serialize the official middle document, not an asset envelope;
always pass assets separately when restoring. Model output is not invented by
restoration.

`render` supports exactly `markdown`, `html`, `latex`, `docx`, `epub`,
`structured_content`, `content_list`, `content_list_v2`, and `pdf`. The result is
tagged as text, JSON, or binary and carries sidecars separately. Per-format
options are closed: options valid for one target may be rejected for another.
`save(root, stem)` transactionally publishes `{root}/{stem}/hybrid-v4/`;
`save_with_writer`/`saveWithWriter` sends validated relative paths to local
callbacks. Callback effects are not rolled back if a callback itself fails.

An owned result and aggregate batch are limited to 64 MiB resident data. A
separate 1 GiB cap applies to the worker bundle on disk; it is not a promise of
unlimited scratch or output storage. Per-item batch failures do not discard
successful sibling results.

This guide covers SDK interfaces; publication availability is described above.
