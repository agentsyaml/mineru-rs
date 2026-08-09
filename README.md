# MinerU Rust

[简体中文](README.zh-CN.md) | [English](README.md)

Parse PDF, image, and Office documents into clean Markdown with MinerU: a
Rust client library, command-line tools, and a local API server. PDF
rendering is pure Rust and needs no native PDF runtime such as PDFium.

The MinerU VLM model can run in two ways:

- **Remote** — point the tools at an OpenAI-compatible MinerU VLM service and
  parse documents without running a model on your own machine.
- **Local (optional)** — run the MinerU Qwen2-VL model with the
  `mineru-mistralrs` backend on CPU or NVIDIA CUDA. The weights (~2.3 GB) can
  be downloaded automatically on first use.

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

### Local: parse with the model on your machine

Build the local backend and run it (CPU):

```sh
cargo build --release --locked --features mistralrs --bin mineru-mistralrs

./target/release/mineru-mistralrs input.pdf --output output
```

By default the first run downloads the fixed model
`opendatalab/MinerU2.5-2509-1.2B` (~2.3 GB) into the Hugging Face cache, then
reuses it. Pass `--model-path /path/to/model` to use a local model directory
instead, or `--allow-download=false` to forbid downloading (then `--model-path`
is required). Results are written to `output/<stem>/vlm/` in MinerU's official
output shape. See [Local model settings](#local-model-settings),
[Docker](#docker) for the CUDA image, and
[Standalone binaries](#standalone-binaries) for prebuilt executables.

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

## CLI and API server

```sh
cargo install mineru
mineru --help
```

The package installs the `mineru`, `mineru-api`, and `mineru-vlm-api`
binaries. To also install the `mineru-office-convert` Office conversion
helper, build with `--features office`:

```sh
cargo install mineru --features office
```

`mineru-api` and `mineru-vlm-api` are two names for the same HTTP service:
it accepts documents, calls the configured VLM, and returns result archives.
It performs no local inference.

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

- **Standalone binaries** — CPU, CUDA, and Metal builds of `mineru-mistralrs`,
  see [Standalone binaries](#standalone-binaries).
- **Docker** — CUDA image, see [Docker](#docker).
- **crates.io** — install the remote-only command-line tools (requires Rust
  1.89+):

  ```sh
  cargo install mineru
  cargo install mineru --features office   # also installs the Office helper
  ```

  The published crate intentionally does not include the local model backend;
  build `mineru-mistralrs` from source (below), use the CUDA image, or download
  a standalone binary.
  Note: `cargo install mineru --features mistralrs` would compile but silently
  use the unpatched upstream core — do not use that flag on the published crate.
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

Build the optional local `mineru-mistralrs` backend:

```sh
cargo build --release --locked --features mistralrs --bin mineru-mistralrs
```

As a library in your own project: `cargo add mineru`.

## Command-line usage

### Options

The local `mineru-mistralrs` backend parses PDFs with these options:

| Option | Default | Description |
| --- | --- | --- |
| `input` | required | PDF to parse (positional argument). |
| `--output <dir>` | `output` | Directory for the results. |
| `--page-start <n>` | 0 | First page to parse (zero-based). |
| `--page-end <n>` | last page | Last page to parse (inclusive). |
| `--no-formula` | off | Skip formula recognition. |
| `--no-table` | off | Skip table recognition. |
| `--no-image-analysis` | off | Skip image/figure analysis. |

The canonical `mineru` command reads the service address and model from the
`MINERU_VL_SERVER` and `MINERU_VL_MODEL_NAME` environment variables:

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"
mineru -p input.pdf -o output
```

Process pages 0–2 with the local backend, disabling formulas and image analysis:

```sh
mineru-mistralrs input.pdf --page-start 0 --page-end 2 \
  --no-formula --no-image-analysis --output output
```

### Output

- `mineru` (remote) writes to `output/` directly: `document.md`,
  `document.json`, `middle.json`, `content_list.json`, cropped `assets/`, and
  a layout preview `{stem}_layout.pdf`.
- `mineru-mistralrs` (local) writes MinerU official-shape output to
  `output/<stem>/vlm/`: `{stem}.md`, `{stem}_middle.json`, `{stem}_model.json`,
  `{stem}_content_list.json`, `{stem}_content_list_v2.json`,
  `{stem}_layout.pdf`, and `images/`.

`{stem}` is the input filename without its extension.

### API server

`mineru-api` and `mineru-vlm-api` are two names for the same HTTP service: it
accepts documents, calls the configured VLM, and returns result archives. It
performs no local inference. See [CLI and API server](#cli-and-api-server) for
the API-mode submission flow.

## Local model and Hugging Face settings

The local `mineru-mistralrs` backend loads a Qwen2-VL MinerU model from one of
two sources, controlled by CLI options:

| Option | Default | Effect |
| --- | --- | --- |
| `--model-path <dir>` | none | Use a local model directory. Always takes priority and never falls back to downloading. |
| `--allow-download[=<bool>]` | `true` | Download `opendatalab/MinerU2.5-2509-1.2B` (~2.3 GB) on first use into the Hugging Face cache when `--model-path` is absent. |

`--allow-download` accepts an explicit boolean value
(`--allow-download=false` disables downloading; then `--model-path` is
required). The legacy `MINERU_VL_MODEL_DIR` and `MINERU_VL_AUTO_DOWNLOAD`
environment variables are still honored, with the CLI options taking
precedence.

A local model directory must contain `config.json` (Qwen2-VL architecture),
`tokenizer.json`, `preprocessor_config.json`, and `model.safetensors`; it is
validated before anything else happens.

Hugging Face settings that also apply to downloads:

| Variable | Effect |
| --- | --- |
| `HF_HOME` | Where the model cache lives. |
| `HF_TOKEN` | Hugging Face access token, for gated or private repositories. |
| `HF_HUB_OFFLINE=1` | Force fully offline operation from the local cache. |

## The five binaries

| Binary | Purpose |
| --- | --- |
| `mineru` | Canonical CLI: PDF, image, and Office documents, either directly against a VLM or through a `mineru-api` server. |
| `mineru-api` | HTTP API server (see above). |
| `mineru-vlm-api` | The same server under a second name. |
| `mineru-office-convert` | Office (.docx/.pptx/.xlsx) → PDF conversion helper used by `mineru`; built with `--features office`. |
| `mineru-mistralrs` | Local PDF parsing with the Qwen2-VL MinerU model; built from source with `--features mistralrs`, or shipped in the CUDA image and standalone binaries. |

## Docker

### CUDA image (local inference)

`ghcr.io/agentsyaml/mineru-rs-cuda:latest-sm80` ships `mineru-mistralrs` for
local parsing on NVIDIA GPUs. The tag targets compute capability 8.0
(Ampere-class) GPUs; it is not a universal all-GPU image. The image ENTRYPOINT
is `mineru-mistralrs`, so arguments after the image name go straight to the
CLI:

```sh
mkdir -p output .hf-cache
docker run --rm --gpus all --user "$(id -u):$(id -g)" \
  -v "$(pwd):/work" -w /work \
  -e HF_HOME=/work/.hf-cache \
  ghcr.io/agentsyaml/mineru-rs-cuda:latest-sm80 \
  input.pdf --output output
```

For a different GPU, build your own image with your GPU's compute capability
(for example, `89` for RTX 40-series):

```sh
docker build --platform linux/amd64 -f Dockerfile.cuda --build-arg CUDA_COMPUTE_CAP=89 -t mineru-rs-cuda .
docker run --rm --gpus all --user "$(id -u):$(id -g)" \
  -v "$(pwd):/work" -w /work \
  -e HF_HOME=/work/.hf-cache \
  mineru-rs-cuda input.pdf --output output
```

The `.hf-cache` bind mount keeps downloaded weights across runs. You may use a
named volume mounted at `/data` instead; the CUDA image defaults `HF_HOME` to
that path.

There is no CPU Docker image; the CPU backend ships as a standalone binary
instead (see below).

## Standalone binaries

Prebuilt `mineru-mistralrs` executables are attached to every GitHub Release
as `mineru-mistralrs-{variant}-{rust-target}.tar.gz`; each tarball contains the
single executable and needs no npm or Python wrapper.

| Variant | Rust target | Platform | Notes |
| --- | --- | --- | --- |
| `cpu` | `x86_64-unknown-linux-gnu` | Linux, amd64 | CPU inference. |
| `cpu` | `aarch64-unknown-linux-gnu` | Linux, arm64 | CPU inference. |
| `cpu` | `aarch64-apple-darwin` | macOS, Apple Silicon | CPU inference. |
| `metal` | `aarch64-apple-darwin` | macOS, Apple Silicon | Metal-accelerated inference; requires macOS 13+ and Apple Silicon. |
| `cuda` | `x86_64-unknown-linux-gnu` | Linux, amd64 | CUDA inference; requires an NVIDIA driver (`libcuda.so.1`) and a compute capability 8.0 (Ampere-class) or newer GPU. |

Download, extract, and run:

```sh
curl -LO https://github.com/agentsyaml/mineru-rs/releases/latest/download/mineru-mistralrs-cpu-x86_64-unknown-linux-gnu.tar.gz
tar xzf mineru-mistralrs-cpu-x86_64-unknown-linux-gnu.tar.gz
./mineru-mistralrs input.pdf --output output
```

Each Release also attaches a `SHA256SUMS` file covering its artifacts, so the
tarball can be verified against it before use.

## Examples

- [examples/python-uv](examples/python-uv) — Python with `uv`
- [examples/node-pnpm](examples/node-pnpm) — Node.js with `pnpm`

## Build and test

```sh
cargo build --release
cargo test

# local backend unit tests (the real-model test is ignored unless
# MINERU_VL_MODEL_DIR points at local weights)
cargo test --features mistralrs --lib
```

## License

MIT OR Apache-2.0. Model weights downloaded from Hugging Face are subject to
the license shown on their model card.
