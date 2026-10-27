# MinerU Rust

[中文](README.zh-CN.md) | English

Parse PDF, image, and Office documents into Markdown with a Rust library, CLI,
and HTTP API server. PDF rendering is implemented in Rust. The legacy CLI/API
and Python/Node `run`/`parse` paths use a remote MinerU-compatible VLM
(MinerU 3.4.5 protocol baseline); rich parsing also includes Rust-native CSV/TSV,
while other formats use a separate Python worker. The checked-in worker contract
is MinerU 4.0.12 / DocVortex 0.5.16; the latest DocVortex 0.6.0 reference has
model-free smoke coverage only. See [compatibility](docs/compatibility.md).

Choose a route:

- **Python or Node**: install the published package and use `parse()` for
  Markdown or `run()` for the complete output tree. See the language examples.
- **Rich official SDK**: use the Rust, Python, or Node SDK; CSV/TSV parse in
  Rust and other formats use a separately prepared worker. See the
  [SDK guide](docs/official-sdk.md) for its runtime contract.
- **Rust CLI / API server**: build from a prepared checkout. The crate is not
  available from crates.io because root workspace patches do not reach consumers.
- **Local conversion**: CLI `--backend local` supports clean text PDFs and
  selected legacy formats through the bundled Rust helper. It produces
  Markdown only, without layout JSON or a VLM call.

This checkout targets 0.4.0. Check the [release guide](docs/releasing.md#current-release-boundary)
for current package and release availability before installing a specific version.

See [usage](docs/usage.md), [compatibility](docs/compatibility.md),
[SDK](docs/official-sdk.md), and [official HTTP API](docs/official-http.md).
For a local Vulkan `llama.cpp` VLM server, see the [Vulkan Compose guide](docs/vulkan.md).

## Quickstart

Install Python (CPython 3.9+) or Node.js 18+, then configure a MinerU-compatible
VLM endpoint. A general-purpose chat endpoint is not sufficient.

| Variable | Meaning | Example |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM service base URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | Model ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer token | `your-key` |

```bash
export MINERU_VL_SERVER="https://host/v1"
export MINERU_VL_MODEL_NAME="model-id"
# If required, enter the key without echoing it or saving it in shell history:
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'
```

For the Python CLI, install the published wheel and run:

```sh
python -m pip install mineru-rs
mineru-rs -p input.pdf -o out/
```

For Node.js, check the registry version and invoke its `mineru-rs` bin:

```bash
version="$(npm view @alexsun-top/mineru version)"
npx --yes --package="@alexsun-top/mineru@$version" -- mineru-rs -p input.pdf -o out/
```

The full output tree is written to `out/`; use `parse()` when you only need
Markdown in memory, or `run()` to write that tree from application code.

Prefer the `MINERU_VL_API_KEY` environment variable over `--api-key` so the
key does not end up in shell history.

## Python

Install `mineru-rs` with `uv` or pip on CPython 3.9+. Use `parse()` for
Markdown or `run()` for the complete output tree. The published wheel does not
include the Rust Office conversion helper. See the [uv example](examples/python-uv/README.md)
and [SDK guide](docs/official-sdk.md).

## Node.js

Install `@alexsun-top/mineru` with npm or pnpm on Node.js 18+. Use `parse()`
for Markdown or `run()` for the complete output tree. The published package
does not include the Rust Office conversion helper. See the [pnpm example](examples/node-pnpm/README.md)
and [SDK guide](docs/official-sdk.md).

## CLI and API server

Install Rust CLI binaries from a prepared source checkout as shown below.

A source build produces the `mineru`, `mineru-api`, and
`mineru-office-convert` binaries. The conversion capabilities are opt-in
features. Run the preparation commands under [Build from source](#build-from-source) first;
they are required before any Cargo command in this checkout.

```sh
cargo install --path . --locked --features office          # docx/pptx/xlsx → PDF + VLM
cargo install --path . --locked --features legacy-office   # legacy → bounded text PDF + VLM; local/native PDF → bundled-helper Markdown
cargo install --path . --locked --features office,legacy-office
```

The `legacy-office` feature adds isolated local Markdown conversion for selected
legacy formats and clean text PDFs. It is not a local inference backend; uncertain
PDFs fail rather than generating layout output. See [usage](docs/usage.md) and
[compatibility](docs/compatibility.md) for supported inputs and backend details.

`mineru-api` is the HTTP API server: it accepts documents, calls the
configured VLM, and returns result archives. It performs no local inference.

```bash
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
# If required, enter the key without echoing it or saving it in shell history:
read -rs -p "VLM API key: " MINERU_VL_API_KEY; export MINERU_VL_API_KEY
printf '\n'

mineru-api --port 8000
```

Keep the server bound to loopback or behind an authenticated reverse proxy:
the task API has no built-in authentication or task ownership. See the
[usage guide](docs/usage.md) for endpoints and complete options.

## Build from source

Requires Rust 1.89+ and Python 3.9+. Prepare dependencies before any Cargo
command in this checkout:

```sh
git clone https://github.com/agentsyaml/mineru-rs
cd mineru-rs
python3 tools/deps/prepare_office.py prepare
python3 tools/deps/prepare_office.py verify
cargo build --release --locked --features office,legacy-office
./target/release/mineru --help
```

`prepare` fetches
hash-pinned source archives; `verify` checks the generated `.deps/office-xml041/`
tree offline. Do not edit that generated tree by hand. Install with
`cargo install --path . --locked --features office,legacy-office`.

## Examples

- [Python + uv](examples/python-uv/README.md)
- [Node.js + pnpm](examples/node-pnpm/README.md)

## License

MIT OR Apache-2.0. Model weights downloaded from Hugging Face are subject to
the license shown on their model card.
