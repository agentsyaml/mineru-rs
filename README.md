# MinerU Rust

[English](README.md) | [简体中文](README.zh-CN.md)

Rust client library, command-line tools, and local API server for parsing PDF,
image, and Office documents with an external OpenAI-compatible MinerU VLM
service. PDF rendering uses pure Rust; no model is downloaded or run locally.

Within the MinerU 3.4.4 VLM scope, MinerU Rust is a drop-in replacement for the
MinerU Python SDK's `vlm-http-client` path and can replace that VLM workflow
completely. It does not implement or claim compatibility with non-VLM backends.
See [the compatibility contract](docs/compatibility.md), the
[Chinese usage guide](docs/usage.md), and the [English usage guide](docs/usage.en.md).
Document-limit controls and their CLI/API applicability are summarized in the usage guides.

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

Your markdown appears in `out/`.

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
    Path("out.md").write_text(result.markdown, encoding="utf-8")


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

```js
const fs = require('fs')
const mineru = require('@alexsun-top/mineru')

async function main() {
  const { markdown } = await mineru.parse({ path: 'input.pdf' })
  fs.writeFileSync('out.md', markdown)
}

main()
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

`--api-key` can pass a Bearer token, but prefer `MINERU_VL_API_KEY`: a key on
the command line is visible in the process list.

See the [Chinese usage guide](docs/usage.md) or [English usage guide](docs/usage.en.md)
for service configuration and complete options.

## Examples

- [examples/python-uv](examples/python-uv) — Python with `uv`
- [examples/node-pnpm](examples/node-pnpm) — Node.js with `pnpm`

## License

Licensed under either Apache-2.0 or MIT, at your option.
