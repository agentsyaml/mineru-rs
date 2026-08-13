# MinerU Rust

[简体中文](README.zh-CN.md) | [English](README.md)

Parse PDF, image, and Office documents into clean Markdown with MinerU: a
Rust client library, command-line tools, and a local API server. PDF
rendering is pure Rust and needs no native PDF runtime such as PDFium.

The MinerU VLM model can run in two ways:

- **Remote** — point the tools at an OpenAI-compatible MinerU VLM service and
  parse documents without running a model on your own machine.
- **Local (optional)** — serve a quantized MinerU model yourself with llama.cpp
  (`llama-server`) and point the tools at it; see [Docker](#docker).

Within the MinerU 3.4.4 VLM scope, MinerU Rust is a drop-in replacement for the
MinerU Python SDK's `vlm-http-client` path and can replace that VLM workflow
completely. It does not implement or claim compatibility with non-VLM backends.
See [the compatibility contract](docs/compatibility.md), the
[Chinese usage guide](docs/usage.md), and the [English usage guide](docs/usage.en.md).
Document-limit controls and their CLI/API applicability are summarized in the usage guides.

The remote protocol is pinned to the MinerU `vlm-http-client` transport
baseline, so a MinerU-compatible VLM endpoint is required — a general-purpose
chat model will not produce layout results.

Requires Rust 1.89 or newer.

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
on your endpoint. On success the `output/` directory contains `document.md`,
`document.json`, `middle.json`, `content_list.json`, cropped `assets/`, and a
layout preview `{stem}_layout.pdf`.

Prefer the `MINERU_VL_API_KEY` environment variable over `--api-key` so the
key does not end up in shell history.

### Local: serve the model with llama.cpp

Run the compose `llama-server` profile (or start `llama-server` yourself with a
quantized MinerU GGUF model), then point `MINERU_VL_SERVER` at
`http://localhost:30000/v1`. See [Docker](#docker).

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

The package installs the `mineru` and `mineru-api` binaries. To also install
the `mineru-office-convert` Office conversion helper, build with `--features
office`:

```sh
cargo install mineru --features office
```

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

### Prebuilt

- **Docker** — llama.cpp server profile, see [Docker](#docker).
- **crates.io** — install the remote-only command-line tools (requires Rust
  1.89+):

  ```sh
  cargo install mineru
  cargo install mineru --features office   # also installs the Office helper
  ```
- **Python** — `pip install mineru-rs` (CPython 3.9+).
- **Node.js** — `npm install @alexsun-top/mineru` (Node.js 18+).

### Build from source

Clone the repository and build the remote CLI/API tools:

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
cargo build --release
./target/release/mineru --help
```

As a library in your own project: `cargo add mineru`.

## Command-line usage

### Options

The canonical `mineru` command reads the service address and model from the
`MINERU_VL_SERVER` and `MINERU_VL_MODEL_NAME` environment variables:

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"
mineru -p input.pdf -o output
```

### Output

- `mineru` (remote) writes to `output/` directly: `document.md`,
  `document.json`, `middle.json`, `content_list.json`, cropped `assets/`, and
  a layout preview `{stem}_layout.pdf`.

`{stem}` is the input filename without its extension.

### API server

`mineru-api` is the HTTP API server: it accepts documents, calls the
configured VLM, and returns result archives. It performs no local inference.
See [CLI and API server](#cli-and-api-server) for the API-mode submission
flow.

## 输入上限与放大配置 / Input limits and how to raise them

流水线在多个独立阶段执行大小上限。触发上限时，报错消息会给出具体文件名、大小、限制值与放大旋钮（flag 或环境变量）；单个文档失败不会中断整批处理，其余文档继续。本地解析大文件会按文件大小占用内存（磁盘总量上限与常驻内存上限相互独立）。

| 上限 | 默认值 | Flag | 环境变量 | 触发阶段 |
| --- | ---: | --- | --- | --- |
| 本地驻留/解析上限 `max_pdf_bytes` | 1 GiB | `--max-pdf-bytes` | `MINERU_MAX_PDF_BYTES` | 文件读取与 PDF 本地解析（含办公室文档转换后 PDF） |
| 输入传输上限 `max_input_bytes` | 4_293_918_719（≈4 GiB） | `--max-input-bytes` | `MINERU_MAX_INPUT_BYTES` | 输入摄取/传输 |
| 输出上限 `max_output_bytes` | 8 GiB | `--max-output-bytes` | `MINERU_MAX_OUTPUT_BYTES` | 输出生成 |
| OOXML 归档上限 | 1 GiB | `--ooxml-archive-bytes` | `MINERU_OOXML_ARCHIVE_BYTES` | Office 文档预检 |
| Office 转换输入上限 | 32 MiB | `--office-input-bytes` | `MINERU_OFFICE_INPUT_BYTES` | LibreOffice 转换 |
| 服务器端文件上限（`--api-url` 模式） | 1 GiB | `--file-cap`（服务端 `mineru-api`） | `MINERU_API_FILE_CAP`（服务端） | 服务器上传 |

Each limit can be raised independently via its flag or environment variable; see the [Chinese usage guide](docs/usage.md) or [English usage guide](docs/usage.en.md) for the full option tables.

## The binaries

| Binary | Purpose |
| --- | --- |
| `mineru` | Canonical CLI: PDF, image, and Office documents, either directly against a VLM or through a `mineru-api` server. |
| `mineru-api` | HTTP API server (see above). |
| `mineru-office-convert` | Office (.docx/.pptx/.xlsx) → PDF conversion helper used by `mineru`; built with `--features office`. |

## Docker

### Docker Compose profiles

The bundled [`docker-compose.yaml`](docker-compose.yaml) runs the MinerU
OpenAI-compatible server on an NVIDIA GPU behind one of two profiles:

| Profile | Image | Purpose |
| --- | --- | --- |
| `openai-server` | `alexsuntop/mineru:3.4.2` | vLLM-backed MinerU server (default, port `30000`). |
| `llama-server` | `ghcr.io/ggml-org/llama.cpp:server-cuda` | llama.cpp server for quantized (GGUF) MinerU models, port `30000`. |

Start the vLLM server:

```sh
docker compose --profile openai-server up -d
```

Start the quantized llama.cpp server. Put your GGUF file(s) in `./models`
(or set `LLAMA_MODELS_DIR`), then point `LLAMA_MODEL` at the container path:

```sh
LLAMA_MODEL=/models/mineru-q4_k_m.gguf \
docker compose --profile llama-server up -d
# or fetch from Hugging Face without local files:
# LLAMA_ARG_HF_REPO=your-org/mineru-gguf:Q4_K_M docker compose --profile llama-server up -d
```

Both server profiles expose `http://localhost:30000` (override with
`MINERU_PORT_OVERRIDE_VLLM` / `MINERU_PORT_OVERRIDE_LLAMA`); they map the same
host port, so start only one at a time.

`COMPOSE_PROFILES` can also activate a profile implicitly, e.g.
`COMPOSE_PROFILES=llama-server docker compose up -d`.

There is no CPU Docker image.

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
