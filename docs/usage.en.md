# Usage Guide

[简体中文](usage.md) | [English](usage.en.md)

`mineru-vlm` renders PDFs locally to page images at **200 DPI** using pure-Rust Hayro, then calls an external, OpenAI-compatible MinerU VLM service to generate layout and content results. It does not perform local model inference, download models, include `mineru-api`, and accepts PDFs only.

### Rust extension: official-shape output

`mineru-vlm --official-output` is a Rust-only low-level direct route: it accepts PDF directories (processed recursively) and writes six official-shape artifacts and a preview to `<output>/<stem>/vlm`. In this mode, `--base-url` and `--model` may be supplied by `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, or single-model discovery; the default compatibility mode still requires both. `--batch-size` is available only with this switch, defaults to `1`, and is used only for local document grouping/progress; it is **not** MinerU's 64-page processing window.

See [compatibility.md](compatibility.md) for the compatibility baseline, reference suite, and reproducible installation. This statement covers only the `vlm-http-client` PDF flow; it is not a full MinerU 3.4.4 compatibility statement.

## Build and prerequisites

Rust 1.89 is required:

```sh
cargo build --release
./target/release/mineru-vlm --help
```

The executable is `target/release/mineru-vlm`. Rendering does not depend on PDFium or another local/native PDF runtime.

## Service and model

Query the service for models first; choose a value from `data[].id` in the returned JSON as `--model`:

```sh
curl -H "Authorization: Bearer $MINERU_VL_API_KEY" \
  "https://<server>/v1/models"
```

`--base-url` may be the service root (for example, `https://<server>/`) or the `/v1` prefix (for example, `https://<server>/v1`); the program accesses the corresponding `/v1/models` and `/v1/chat/completions`. `--model` is required and must not be empty.

Authentication preferentially uses the `MINERU_VL_API_KEY` environment variable; `--api-key` can override it. Avoid putting keys directly on the command line: they may enter shell history or logs.

```sh
export MINERU_VL_API_KEY='<your-key>'
./target/release/mineru-vlm "input.pdf" \
  --base-url "https://<server>/v1" \
  --model "<model-id>" \
  --output "output"
```

If a key must be passed temporarily:

```sh
./target/release/mineru-vlm "input.pdf" --base-url "https://<server>/" \
  --model "<model-id>" --api-key "<your-key>"
```

## Command-line options

| Option | Default | Description |
| --- | --- | --- |
| `input` | Required | Input PDF path (positional argument). |
| `--base-url` | Required | OpenAI-compatible service root address or `/v1` prefix. |
| `--model` | Required | Model ID returned by `GET /v1/models`. |
| `--output <directory>` | `output` | Output directory. |
| `--api-key <key>` | None | Bearer token; takes precedence over `MINERU_VL_API_KEY`. |
| `--page-start <n>` | None (starts at 0) | Start page, **zero-based**. |
| `--page-end <n>` | None (through the last page) | End page, **inclusive**. |
| `--no-formula` | Off | Do not process formulas. |
| `--no-table` | Off | Do not process tables. |
| `--no-image-analysis` | Off | Do not analyze images. |

Process only pages 0 through 2, disabling formula and image analysis:

```sh
./target/release/mineru-vlm "input.pdf" --base-url "https://<server>/v1" \
  --model "<model-id>" --page-start 0 --page-end 2 \
  --no-formula --no-image-analysis --output "result"
```

When only `--page-end` is given, processing starts at page 0; when only `--page-start` is given, it continues through the last page. A start page greater than the end page, or a range outside the PDF page count, fails. Any error is written to stderr and the process exits with status 1.

---

## Canonical `mineru` command (PDF / images / Office)

`mineru` is the canonical product binary, supporting PDF, image, and Office input and an optional `--api-url` remote API server mode. It exposes no local ML backend; `--backend` accepts only `vlm-http-client`.

Office-format conversion requires the `mineru-office-convert` helper, which depends on the optional `office` feature:

```sh
cargo build --release --features office
```

### Office helper containment

Before conversion, the helper performs mandatory complete preflight validation of OOXML and limits input to 32 MiB and output to 64 MiB.

| Platform | Hard memory limit | Other helper limits |
| --- | --- | --- |
| Linux | `RLIMIT_AS` 1 GiB | CPU 120 seconds, `NOFILE` 256, managed 180-second wall deadline, process-group cleanup |
| Windows | Job Object 1 GiB | CPU 120 seconds, managed 180-second wall deadline, Job tree cleanup |
| macOS | No native hard RSS limit | Mandatory preflight validation, CPU 120 seconds, `NOFILE` 256, managed 180-second wall deadline, process-group cleanup |

Native macOS APIs have no reliable process RSS/address-space hard limit that does not require an entitlement. Deployments accepting untrusted Office documents from the Internet on native macOS must use an external VM or container memory boundary to provide hard memory isolation.

### Direct VLM mode (default)

Without `--api-url`, `mineru` calls the external VLM service directly. The service address and model are supplied by the `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and `MINERU_VL_API_KEY` environment variables or overridden by `--url`.

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

./target/release/mineru -p input.pdf -o output
```

### Remote API server mode

With `--api-url`, `mineru` submits documents to a running `mineru-api` server; the server calls the VLM and returns a result archive. `--url` overrides the server-side model address for an individual task.

```sh
./target/release/mineru -p input.pdf -o output --api-url "http://127.0.0.1:8000"
```

### Command-line options

| Option | Default | Description |
| --- | --- | --- |
| `-p, --path <path>` | Required | Input file or directory (processed recursively). |
| `-o, --output <directory>` | Required | Output directory. |
| `--api-url <URL>` | None | Remote API server address; without it, direct VLM mode is used. |
| `-m, --method <auto\|txt\|ocr>` | `auto` | Parsing method (ignored in direct mode). |
| `-b, --backend <vlm-http-client>` | `vlm-http-client` | Backend (the only one available). |
| `--effort <medium\|high>` | `medium` | Parsing effort (ignored in direct mode). |
| `-l, --lang <language>` | `ch` | Language code. |
| `-u, --url <URL>` | None | VLM service-address override in direct mode; per-task model-server override in API mode. |
| `-s, --start <n>` | `0` | Start page, **zero-based**. |
| `-e, --end <n>` | None (through the last page) | End page, **inclusive**. |
| `-f, --formula <true\|false>` | `true` | Formula recognition. |
| `-t, --table <true\|false>` | `true` | Table recognition. |
| `--image-analysis <true\|false>` | `true` | Image analysis. |

In direct mode, non-default values for `--method`, `--effort`, and `--lang` produce a warning and are ignored. `--client-side-output-generation=true` is rejected in API mode.

---

## API server

`mineru-api` and `mineru-vlm-api` are two executable names for the same service and behave identically. The service itself performs no local inference: it accepts documents, calls an external VLM service, then returns archived results.

### Container

The stable image supports `amd64` and `arm64`:

```sh
docker pull ghcr.io/agentsyaml/mineru-rs:latest
docker volume create mineru-output
docker run --rm -p 8000:8000 -v mineru-output:/app/output \
  -e MINERU_VL_SERVER="https://<server>" \
  -e MINERU_VL_MODEL_NAME="<model-id>" \
  -e MINERU_VL_API_KEY="<your-key>" \
  -e MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true \
  ghcr.io/agentsyaml/mineru-rs:latest
```

The default command starts the API, listens on `8000`, and writes task output to `/app/output`; it uses a named volume by default to retain the directory permissions required by the non-root image. When bind-mounting a host directory on native Linux, create the directory and run as the host user with `--user "$(id -u):$(id -g)"`. The image binds the API publicly, but to avoid unauthenticated public parsing, POST parsing must be explicitly enabled by the operator: `-e MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true`. The default command can be replaced to run the CLI:

```sh
docker run --rm ghcr.io/agentsyaml/mineru-rs:latest mineru --version
mkdir -p output
docker run --rm --user "$(id -u):$(id -g)" -v "$(pwd):/work" -w /work \
  -e MINERU_VL_SERVER="https://<server>" -e MINERU_VL_MODEL_NAME="<model-id>" \
  ghcr.io/agentsyaml/mineru-rs:latest mineru -p input.pdf -o output
```

### Startup

The service requires a usable VLM service address and model, supplied by `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and `MINERU_VL_API_KEY`:

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY='<your-key>'

./target/release/mineru-api --port 8000
```

After successful startup, stderr prints copyable service and health-check addresses:

```text
server started: http://127.0.0.1:8000: health=http://127.0.0.1:8000/health
```

### Command-line options

| Option | Default | Description |
| --- | --- | --- |
| `--host <IP>` | `127.0.0.1` | Bind address. A non-loopback address also requires `MINERU_API_PUBLIC_BIND_EXPOSED`, otherwise startup fails. |
| `--port <port>` | `8000` | Listening port. |
| `--output-root <directory>` | `./output` | Root directory for task output and temporary files. |
| `--concurrency <n>` | `3` outside macOS, `1` on macOS | Number of tasks processed concurrently. |
| `--shutdown-on-stdin-eof` | Off | Gracefully exit when stdin closes; suitable for parent-process management. |

`--output-root`, `--concurrency`, and `--shutdown-on-stdin-eof` override their corresponding environment variables; when omitted, the environment-variable value or the table default remains in effect. When `--concurrency` is explicitly supplied, macOS is no longer forced to 1.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `MINERU_API_OUTPUT_ROOT` | `./output` | Output root directory. |
| `MINERU_API_MAX_CONCURRENT_REQUESTS` | `3` outside macOS, `1` on macOS | Number of concurrent tasks; non-positive or invalid values cause startup to fail. |
| `MINERU_API_TASK_RETENTION_SECONDS` | `86400` | Retention period for terminal task records. |
| `MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS` | `300` | Cleanup scan interval. |
| `MINERU_API_PUBLIC_BIND_EXPOSED` | Off | Allow binding a non-loopback address. |
| `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` | Off | Allow POST parsing requests when publicly bound. |
| `MINERU_API_SHUTDOWN_ON_STDIN_EOF` | Off | Equivalent to `--shutdown-on-stdin-eof`. |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | Page processing window. |
| `MINERU_PDF_RENDER_THREADS` | `3` | Number of rendering workers. |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | Timeout in seconds for a single render. |
| `MINERU_FORMULA_ENABLE` | On | Default for formula recognition. |
| `MINERU_TABLE_ENABLE` | On | Default for table recognition. |

Boolean variables accept `1`, `true`, `yes`, and `on` (case-insensitive); other values are treated as off. Invalid numeric values other than concurrency fall back to their defaults.

### HTTP interface

`GET /health` returns service capacity and the number of registered tasks:

```sh
curl "http://127.0.0.1:8000/health"
```

```json
{"status":"healthy","protocol_version":2,"max_concurrent_requests":3,"processing_window_size":64,"task_count":0}
```

Asynchronous mode: `POST /tasks` returns `202` and a task snapshot immediately after submission; then poll its status and retrieve the result archive.

```sh
curl -X POST "http://127.0.0.1:8000/tasks" \
  -F "files=@input.pdf" \
  -F "backend=vlm-http-client" \
  -F "response_format_zip=true"
```

```json
{"task_id":"local-0","status":"pending","backend":"vlm-http-client","file_names":["input.pdf"],"queued_ahead":0,"status_url":"http://127.0.0.1:8000/tasks/local-0","result_url":"http://127.0.0.1:8000/tasks/local-0/result","message":"Task submitted successfully"}
```

```sh
curl "http://127.0.0.1:8000/tasks/local-0"
curl -o result.zip "http://127.0.0.1:8000/tasks/local-0/result"
```

Statuses are `pending`, `processing`, `completed`, or `failed`. If the result is not ready, `GET /tasks/{id}/result` returns `202`; a failed task returns `409`; an unknown task returns `404`.

Synchronous mode: `POST /file_parse` completes parsing within the same request and streams the result archive directly, without creating a queryable task record.

```sh
curl -X POST "http://127.0.0.1:8000/file_parse" \
  -F "files=@input.pdf" \
  -F "backend=vlm-http-client" \
  -F "response_format_zip=true" \
  -o result.zip
```

Selection guidance: use `/tasks` for batches, long documents, or when progress visibility is needed; use `/file_parse` for a single small document or a one-off call in a script.

The form accepts `files` file parts and the text fields `lang_list`, `backend`, `effort`, `parse_method`, `formula_enable`, `table_enable`, `image_analysis`, `start_page_id`, `end_page_id`, `server_url`, `response_format_zip`, `return_md`, `return_middle_json`, `return_model_output`, `return_content_list`, `return_images`, and `return_original_file`. Duplicate fields, too many fields, or invalid values are rejected.

Common status codes:

| Status code | Meaning |
| --- | --- |
| `400` | Invalid multipart data, duplicate or excessive fields, unsupported values, invalid request Host, or parsing not enabled for a public bind. |
| `408` | The request exceeded its deadline. |
| `413` | Request body, file, or text field exceeds its limit. |
| `422` | Unsupported file type or invalid filename. |
| `503` | Task capacity is full or the service is shutting down. |
| `409` | The task failed or its worker terminated abnormally. |

Uploads, queuing, and processing share one request deadline, taken from the total parsing timeout (24 hours by default). This differs from the `MINERU_PDF_RENDER_TIMEOUT` timeout for a single render; the server has no separate environment variable to adjust it. Timeouts consistently return `408` and release the concurrency slot and temporary directory, so slow uploads do not occupy a slot indefinitely.

### Security

- By default, the service listens only on loopback. Binding a non-loopback address requires explicitly setting `MINERU_API_PUBLIC_BIND_EXPOSED`; once publicly bound, `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` is also required before parsing requests are handled.
- The service **provides neither authentication nor task-ownership isolation**: task IDs are sequential `local-N`, and any party that can reach the service can read every task's status and result. Public Internet deployments must be placed behind an authenticated reverse proxy.
- For the built-in `mineru --api-url https://...` client, the reverse proxy must preserve the canonical `Host` authority actually sent by the client: even when `--api-url` contains `:443`, `:0443`, or an empty port, the URL/reqwest omits the HTTPS default port; a non-default port is canonicalized as decimal, and the proxy must not add, remove, or rewrite that canonical port. The backend deliberately ignores `Forwarded` and `X-Forwarded-*` because no trusted-proxy boundary exists. Only at the submission-response boundary, if the backend returns HTTP task links matching that canonical authority, the client locally upgrades them to HTTPS and reruns strict same-origin checks; direct polling/downloads and redirects do not use this compatibility rule, and cross-host/port targets, userinfo, and downgrades still fail closed. If the proxy rewrites `Host`/port or a public path prefix, this narrow compatibility rule does not apply; such deployments require external canonical-base configuration, which is not currently provided.
- A request-level `server_url` override does not carry the server's API key or forward any `Authorization` header.
- `status_url` / `result_url` returned by asynchronous tasks must be same-origin with the configured API; redirects are checked for same origin hop by hop, and no request is sent to a cross-origin target.

## Output

On success, the specified directory contains:

```text
output/
├── document.json          # Complete document result (does not embed asset binary data)
├── document.md            # Markdown
├── middle.json            # Intermediate structure
├── content_list.json      # Content list
├── assets/                # Cropped assets such as recognized figures, tables, formulas, and charts (present according to actual results)
└── {stem}_layout.pdf      # Preview of the original PDF with layout-block annotations
```

`{stem}` is the safe stem of the path input filename after removing its extension; it is `document` when there is no safe stem. The library API also uses `document_layout.pdf` when bytes are passed as `PdfInput::Bytes`. Output is first written to a sibling temporary staging directory; on completion, a rename replaces the target directory. An existing directory is first retained as a backup and the backup is removed after successful replacement, avoiding partially written results.

## Library API (minimal example)

The following example uses only the public API and can be placed in your own Tokio async program:

```rust
use mineru::{ClientConfig, MinerUClient, ParseOptions, PdfInput, write_outputs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ClientConfig::new("https://<server>/v1", "<model-id>")?;
    let client = MinerUClient::new(config)?;

    client.check_model().await?;
    let document = client
        .parse_pdf(PdfInput::Path("input.pdf".into()), ParseOptions::default())
        .await?;
    let outputs = write_outputs(&document, "output")?;
    println!("{}", outputs.markdown.display());
    Ok(())
}
```

When authentication is required, set `BearerToken::new(...)` on the public `bearer_token` of a mutable `ClientConfig` before constructing `MinerUClient`. `check_model()` requests the model list and confirms that it contains the configured model.

## Default resource limits

| Item | Default |
| --- | ---: |
| PDF size / page count | 512 MiB / 10,000 pages |
| Per-page pixels / rendered RGB image | 100,000,000 / 64 MiB |
| Response body / all assets | 10 MiB / 1 GiB |
| Layout blocks per page / page window | 256 / 64 pages |
| Concurrent in-flight rendered images | 128 MiB |
| Request concurrency / rendering workers | 100 / 3 (rendering is actually at most 3) |
| Connection / per-request / total parsing timeout | 10 seconds / 600 seconds / 24 hours |

### Sources of defaults and capacity

- **Upstream-locked**: 200 DPI, 64-page window, 3 rendering workers, VLM HTTP maximum concurrency 100, HTTP request timeout 600 seconds.
- **Rust safeguards**: 10-second connection timeout, 24-hour total timeout, and limits for page count, PDF, assets, responses, rendered images, pixels, in-flight images, and layout blocks.

Support for 10,000 pages is only best effort with high memory: input bytes, final page results, and assets are all retained in memory; it is not an unbounded guarantee. Library callers can adjust the public `ClientConfig.limits`, `timeouts`, `request_concurrency`, and `render_workers`, then call `validate()` (`MinerUClient::new` also validates); configure them for available RAM and service-endpoint capacity. All limits, concurrency values, and worker counts must be greater than zero; all timeouts must be nonzero, and the per-request timeout must not exceed the total timeout.

## Limitations and troubleshooting

- Hayro does not support encrypted PDFs; rendering of complex/advanced PDF effects may differ from other renderers. Invalid PDFs, inconsistent page mappings, size limits, or rendering errors fail explicitly and are not silently skipped.
- The preview supports page rotations `0/90/180/270`. Its goal is usable visual and semantic alignment; because annotations are written and PDF serialization changes, the preview file's bytes are not identical to the original PDF. Other rotations fail.
- `401` usually means a missing or invalid API key; `404` usually means an incorrect `--base-url` path. Confirm the service actually exposes `/v1/models` and `/v1/chat/completions`.
- If `check_model` or model checking fails, confirm that `data` returned by `GET /v1/models` contains the selected ID, and check authentication and the base URL.
- `no valid layout tokens` means the service response does not contain the layout tokens required by MinerU; choose a compatible MinerU VLM model/service rather than a general chat model.
- `limit exceeded` means a resource limit from the table above was exceeded; reduce the input or adjust and validate the configuration in a library caller. PDFs unsupported by Hayro must be processed first using a file/rendering workflow that supports the relevant PDF features, then retried.
