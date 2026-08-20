# MinerU Rust

[简体中文](README.zh-CN.md) | [English](README.md)

Parse PDF, image, and Office documents into clean Markdown with MinerU: a
Rust client library, command-line tools, and a local API server. PDF
rendering is pure Rust and needs no native PDF runtime such as PDFium.

The MinerU VLM model can run in two ways:

- **Remote** — point the tools at an OpenAI-compatible MinerU VLM service and
  parse documents without running a model on your own machine.
- **External provider (optional)** — point the tools at a separately prepared
  OpenAI-compatible provider, such as a `llama-server` instance; see [Docker](#docker).
  This is an unvalidated provider example, not a claim about MinerU 4.0.0a6
  multimodal or model compatibility.
  This is separate from CLI `backend=local`, which uses the bundled Rust
  `mineru-office-convert` helper for isolated AnyDoc native Markdown extraction
  of supported legacy formats and clean text PDFs. That helper does not invoke
  Python, Microsoft Office/LibreOffice, a model, or the network.

Within the MinerU 3.4.5 VLM scope, MinerU Rust is a drop-in replacement for the
MinerU Python SDK's `vlm-http-client` path and can replace that VLM workflow
completely for the PDF/VLM workflow. Direct `backend=hybrid-http-client` is a
separate official MinerU 4.0.0a6 boundary: it requires a user-installed pinned
Python package and launches one embedded-shim subprocess per document. Its
`hybrid-v4` artifacts are separate from the 3.4.5 output path. The CLI also
provides the separate `backend=local` AnyDoc native-Markdown lane through the
bundled Rust helper; it is not the official Hybrid backend. API Hybrid remains
fail-closed and never aliases the 3.4.5 VLM route.
See [the compatibility contract](docs/compatibility.md), the
[Chinese usage guide](docs/usage.md), and the [English usage guide](docs/usage.en.md).
Document-limit controls and their CLI/API applicability are summarized in the usage guides.

**No GPU?** The PDF/image pipeline drives a VLM endpoint, so the CPU-only
alternative for full layout parsing is the official MinerU Python pipeline
(PP-OCRv6): it emits the same
`document.json` / `middle.json` / `content_list.json` / markdown contract and
the two outputs can be consumed interchangeably. On the non-local CLI path,
legacy office files first go through the isolated helper's bounded, text-only
best-effort PDF fallback and then the existing PDF/VLM route. Original layout,
images, tables, formulas, and macros may be lost, and non-ASCII characters may
become `?`; when that conversion is unsuitable, first use Microsoft Office or
LibreOffice to save as
DOCX/XLSX/PPTX. Without a VLM service, the separate `backend=local` route remains
a Markdown-only AnyDoc converter such as
[anydoc](https://github.com/firecrawl/anydoc), isolated in the bundled Rust
helper (Markdown only, no layout JSON).
Native PDF output is accepted only for conservative clean text PDFs; uncertain
PDFs fail clearly in local mode rather than pretending to provide official
layout output.
Official MinerU 4.0.0a6 direct Hybrid supports the official `medium`, `high`, and
`xhigh` efforts and `auto|light|full` model stacks through that Python boundary.
Python, MinerU, and model assets are not bundled; Docker/llama.cpp orchestration
is outside this C1 boundary (see [compatibility](docs/compatibility.md)).

The remote protocol is pinned to the MinerU `vlm-http-client` transport
baseline, so a MinerU-compatible VLM endpoint is required — a general-purpose
chat model will not produce layout results.

Requires Rust 1.89 or newer.

## Performance

Measured against the official MinerU Python SDK (`vlm-http-client` path), same
VLM endpoint, same input documents:

| Document | MinerU Rust | Official SDK | Speed | Memory |
| --- | --- | --- | --- | --- |
| 334 pages | 130.44 s / 2.15 GB | 162.24 s / 3.93 GB | **19.6% faster** | **45% less** |
| 738 pages | 324.48 s / 2.31 GB | 361.74 s / 4.45 GB | **10.3% faster** | **48% less** |

The Rust client keeps a smaller resident footprint end to end: it streams the
VLM response and writes the output tree incrementally instead of buffering the
full result in memory.

## Quickstart

Configure the VLM service with three environment variables:

| Variable | Meaning | Example |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM service base URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | Model ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer token | `your-key` |

Install the `mineru` command with Cargo, pip, or npm:

```sh
cargo install mineru            # Rust
pip install mineru-rs           # Python
npm install @alexsun-top/mineru # Node.js
```

Then parse a document:

```sh
mineru -p input.pdf -o out/
```

Your markdown appears in `out/`. Find a usable model ID with `GET /v1/models`
on your endpoint. On success the `out/` directory contains `document.md`,
`document.json`, `middle.json`, `content_list.json`, cropped `assets/`, and a
layout preview `{stem}_layout.pdf`.

Prefer the `MINERU_VL_API_KEY` environment variable over `--api-key` so the
key does not end up in shell history.

### Optional external OpenAI-compatible provider

Run the compose `llama-server` profile (or start `llama-server` yourself) only
after explicitly preparing and mounting the model and configuration outside this
repository. Then point `MINERU_VL_SERVER` at `http://localhost:30000/v1`.
The provider and model compatibility are not validated here; no automatic
Hugging Face model download is implied. See [Docker](#docker).

## Rust library

```sh
cargo add mineru
```

```rust
use mineru::{RunOptions, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(RunOptions::new("input.pdf", "out/")).await?;
    Ok(())
}
```

## Python

Wheels support CPython 3.9 and newer. Install with `uv` or pip:

```sh
uv add mineru-rs
# or: pip install mineru-rs
```

`parse()` returns the markdown string in memory; save it yourself:

```python
import asyncio
from pathlib import Path

import mineru_rs


async def main() -> None:
    result = await mineru_rs.parse("input.pdf")
    await asyncio.to_thread(
        Path("out.md").write_text, result.markdown, encoding="utf-8"
    )


asyncio.run(main())
```

`run()` writes the full output tree to an output directory instead (see the
[English usage guide](docs/usage.en.md)). The wheel installs two equivalent
console commands, `mineru` and `mineru-rs`; prefer `mineru-rs` when the
upstream Python `mineru` package is also installed, since both provide a
`mineru` entry point and the one earlier on `PATH` wins. Releases are wheels
only; there is no sdist or source fallback for unsupported platforms or PyPy.

The Python wheel does not bundle the `mineru-office-convert` helper: Office
format (`.docx`/`.pptx`/`.xlsx`) input is not yet supported in the binding and
fails with "office conversion is unavailable". PDF and image input are
unaffected; use the Rust CLI (`cargo install mineru --features office`) or
`mineru-api` for Office conversion.

## Node.js

Requires Node.js 18 or newer. Install with `pnpm` or npm:

```sh
pnpm add @alexsun-top/mineru
# or: npm install @alexsun-top/mineru
```

```ts
import { writeFile } from 'node:fs/promises'
import mineru from '@alexsun-top/mineru'

const { markdown } = await mineru.parse({ path: 'input.pdf' })
await writeFile('out.md', markdown)
```

`run({ path, output })` writes the full output tree to `output` instead (see
the [English usage guide](docs/usage.en.md)). The root package installs two
equivalent binaries, `mineru` and `mineru-rs`, both pointing at
`bin/mineru.js`; prefer `mineru-rs` if another `mineru` command is already on
`PATH`.

The npm packages do not bundle the `mineru-office-convert` helper: Office
format (`.docx`/`.pptx`/`.xlsx`) input is not yet supported in the binding and
fails with "office conversion is unavailable". PDF and image input are
unaffected; use the Rust CLI (`cargo install mineru --features office`) or
`mineru-api` for Office conversion.

## CLI and API server

```sh
cargo install mineru
mineru --help
```

The package installs the `mineru`, `mineru-api`, and `mineru-office-convert`
binaries. The conversion capabilities are opt-in features:

```sh
cargo install mineru --features office          # docx/pptx/xlsx → PDF + VLM
cargo install mineru --features legacy-office   # legacy → bounded text PDF + VLM; local/native PDF → bundled-helper Markdown
cargo install mineru --features office,legacy-office
```

`mineru --backend local` supports the `legacy-office` formats above and clean
text PDFs through the bundled, bounded `mineru-office-convert` Rust helper. The
helper does not invoke Python, Microsoft Office/LibreOffice, a model, or the
network. Its output is `output/{stem}/office/{stem}.md` for legacy formats and
`output/{stem}/native/{stem}.md` for native PDF Markdown. Native output contains
only that Markdown file: it does not create `document.json`, `middle.json`,
`content-list`, or assets. Scanned, mixed, garbled, low-quality, or uncertain
PDFs fail clearly in local mode instead of falling back to VLM; `backend=local`
is not a local `llama-server` backend, and API backend semantics are unchanged.
Local mode uses the helper's bounded default policy; helper-only wall/CPU/memory/
NOFILE/process-isolation controls are rejected rather than silently ignored when
not explicitly supported. Only the input/output byte limits are currently
supported for local configuration.

Direct `--backend hybrid-http-client` uses the official MinerU 4.0.0a6 Python
boundary for PDF/image inputs and writes separate `hybrid-v4` artifacts. It
requires the user-installed pinned Python package and model assets; none are
bundled. API Hybrid remains fail-closed and never silently enters the 3.4.5
VLM route. `vlm-http-client` always keeps the existing remote behavior.

Without `backend=local`, the same legacy formats use the isolated helper to
create a bounded best-effort text PDF before the existing VLM route. This may
lose Office layout, fonts, images, tables, formulas, or macros; convert first
with Microsoft Office or LibreOffice to DOCX/XLSX/PPTX when those details matter.

`mineru-api` is the HTTP API server: it accepts documents, calls the
configured VLM, and returns result archives. It performs no local inference.

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

mineru-api --port 8000
```

Submit a document as an async task and fetch the result:

```sh
curl -X POST http://127.0.0.1:8000/tasks \
  -F "files=@input.pdf" -F "backend=vlm-http-client" -F "response_format_zip=true"
# poll the returned status_url, then download result_url
```

The `mineru` client can submit through a running server:

```sh
mineru -p input.pdf -o output --api-url http://127.0.0.1:8000
```

`--api-key` can pass a Bearer token, but prefer `MINERU_VL_API_KEY`: a key on
the command line is visible in the process list.

See the [Chinese usage guide](docs/usage.md) or [English usage guide](docs/usage.en.md)
for service configuration and complete options.

## Install

- **crates.io** — `cargo install mineru` (add `--features office` for
  docx/pptx/xlsx→PDF conversion or `--features legacy-office` for legacy
  best-effort text-PDF conversion on the non-local CLI path; requires Rust 1.89+).
- **Python** — `pip install mineru-rs` (CPython 3.9+).
- **Node.js** — `npm install @alexsun-top/mineru` (Node.js 18+).
- **Docker** — see [Docker](#docker).

Build from source:

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
cargo build --release
./target/release/mineru --help
```

As a library in your own project: `cargo add mineru`.

## The binaries

| Binary | Purpose |
| --- | --- |
| `mineru` | Canonical CLI: PDF, image, and Office documents, either directly against a VLM or through a `mineru-api` server. |
| `mineru-api` | HTTP API server (see above). |
| `mineru-office-convert` | Bundled Rust helper: docx/pptx/xlsx → PDF (`--features office`), legacy doc/ppt/xls/odt/rtf/epub/ods/odp/csv → bounded best-effort text PDF for non-local VLM runs, and isolated legacy/native-PDF Markdown for `backend=local` (`--features legacy-office`). The local routes invoke no Python, Office application, model, or network. |

## Docker

### Published Rust API image

The published CPU-capable Rust API image is
`ghcr.io/agentsyaml/mineru-cli`. It listens on container port `8000`, exposes
`GET /health`, stores task output under `/app/output`, and runs its default
command as a non-root user. The published release binaries include the
`office,legacy-office` feature set; this does not add Python or local model
inference.

The stock image bundles Rust binaries only: it does not contain Python,
`mineru==4.0.0a6`, or model assets. Direct official Hybrid therefore requires a
separately prepared environment explicitly supplied to the container, while
API Hybrid remains fail-closed and never aliases the 3.4.5 VLM route.

```sh
mkdir -p output
chmod a+rwx output  # grant the image's non-root user write access
docker run --rm \
  --publish 127.0.0.1:8000:8000 \
  --volume "$PWD/output:/app/output" \
  --env MINERU_VL_SERVER="https://<server>" \
  --env MINERU_VL_MODEL_NAME="<model-id>" \
  --env MINERU_VL_API_KEY="<your-key>" \
  --env MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true \
  ghcr.io/agentsyaml/mineru-cli:latest

curl http://127.0.0.1:8000/health
```

The bind-mounted output directory must be writable by the image's default
non-root user. `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true` is an explicit,
per-container opt-in to the unauthenticated task API; keep it out of the
Dockerfile and global image environment. Publish to loopback first as shown;
broader exposure requires a private network or an authenticated reverse proxy
because the API has no built-in authentication or task ownership isolation.

### Docker Compose profiles

The bundled [`docker-compose.yaml`](docker-compose.yaml) runs the MinerU
OpenAI-compatible server on an NVIDIA GPU behind one of two profiles:

| Profile | Image | Purpose |
| --- | --- | --- |
| `openai-server` | `alexsuntop/mineru:3.4.2` | vLLM-backed MinerU server (default, port `30000`). |
| `llama-server` | `ghcr.io/ggml-org/llama.cpp:server-cuda` | Generic OpenAI-compatible provider example, port `30000`; model compatibility is not validated. |

Start the vLLM server:

```sh
docker compose --profile openai-server up -d
```

Start the generic llama.cpp provider only after explicitly preparing a model and
mounting it at the container path. Put the prepared GGUF file in
`./models` (or set `LLAMA_MODELS_DIR`), then point `LLAMA_MODEL` at it:

```sh
LLAMA_MODEL=/models/model.gguf \
docker compose --profile llama-server up -d
```

Both server profiles bind the published host port to `127.0.0.1` by default and
expose `http://localhost:30000` (override the port with
`MINERU_PORT_OVERRIDE_VLLM` / `MINERU_PORT_OVERRIDE_LLAMA`). Set
`MINERU_PROVIDER_BIND_HOST=<bind-host>` explicitly to use another bind address;
broader exposure requires a private network or an authenticated reverse proxy.
They map the same host port, so start only one at a time.

`COMPOSE_PROFILES` can also activate a profile implicitly, e.g.
`COMPOSE_PROFILES=llama-server docker compose up -d`.

The GHCR image above is the published Rust API image; it is not a bundled
Python/model runtime. The Compose profiles are separate provider examples and
do not prepare or validate a MinerU 4.0.0a6 model.

## Examples

- [examples/python-uv](examples/python-uv) — Python with `uv`
- [examples/node-pnpm](examples/node-pnpm) — Node.js with `pnpm`

## Build and test

```sh
cargo build --release
cargo test
```

## License

MIT OR Apache-2.0. Model weights downloaded from Hugging Face are subject to
the license shown on their model card.
