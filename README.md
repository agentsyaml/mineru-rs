# MinerU Rust

Rust client library, command-line tools, and local API server for parsing PDF,
image, and Office documents with an external OpenAI-compatible MinerU VLM
service. PDF rendering uses pure Rust; no model is downloaded or run locally.

Compatibility is pinned to the MinerU 3.4.4 `vlm-http-client` transport
baseline. See [the compatibility contract](docs/compatibility.md) and
[usage guide](docs/usage.md). This is not a full MinerU compatibility claim.

Requires Rust 1.89 or newer.

## Rust library

```sh
cargo add mineru
```

```rust
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = MinerUClient::new(ClientConfig::new(
        "https://example.test/v1",
        "model-id",
    )?)?;
    let document = client
        .parse_pdf(PdfInput::Path("input.pdf".into()), ParseOptions::default())
        .await?;
    println!("{} pages", document.pages.len());
    Ok(())
}
```

## CLI and API server

```sh
cargo install mineru
mineru --help
```

The package installs the existing `mineru`, `mineru-api`, `mineru-vlm`,
`mineru-vlm-api`, and `mineru-office-convert` binaries. See
[docs/usage.md](docs/usage.md) for service configuration and complete options.

## Python

Wheels support CPython 3.9 and newer:

```sh
pip install mineru-rs
```

```python
import mineru_rs

print(mineru_rs.canonical_stem("a bad/pdf"))
mineru_rs.validate_pdf_options(0, None, True, True, True)
```

Python currently exposes only canonical stem handling and PDF-option
validation. It does not expose asynchronous document parsing. Releases are
wheels only; there is no sdist or source fallback for unsupported platforms or
PyPy.

## Node.js

```sh
npm install @alexsun-top/mineru
```

```js
const mineru = require('@alexsun-top/mineru')

console.log(mineru.canonicalStem('a bad/pdf'))
mineru.validatePdfOptions(0, null, true, true, true)
```

Node.js currently exposes only canonical stem handling and PDF-option
validation. It does not expose asynchronous document parsing. Node.js 18 or
newer is required.

## License

Licensed under either Apache-2.0 or MIT, at your option.
