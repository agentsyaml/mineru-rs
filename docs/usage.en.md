# Usage Guide

[简体中文](usage.md) | [English](usage.en.md)

See [compatibility.md](compatibility.md) for the compatibility baseline and reproducible installation. This statement covers only the `vlm-http-client` PDF flow; it is not a full MinerU 3.4.5 compatibility statement.

## Build and prerequisites

Rust 1.89 is required:

```sh
cargo build --release
./target/release/mineru --help
```

The executable is `target/release/mineru`. Rendering does not depend on PDFium or another local/native PDF runtime.

## Quickstart

Configure the VLM service with three environment variables:

| Variable | Meaning | Example |
| --- | --- | --- |
| `MINERU_VL_SERVER` | VLM service base URL | `https://host/v1` |
| `MINERU_VL_MODEL_NAME` | Model ID | `model-id` |
| `MINERU_VL_API_KEY` | Bearer token | `your-key` |

Then parse a PDF:

```sh
mineru -p input.pdf -o out/
```

Your markdown appears in `out/`. `--api-key` can also pass the Bearer token,
but prefer the environment variable: a key on the command line is visible in
the process list. These variables also appear in the [environment table](#environment-variables)
below.

## Service and model

Query the service for models first; choose a value from `data[].id` in the returned JSON and set it as `MINERU_VL_MODEL_NAME`:

```sh
curl -H "Authorization: Bearer $MINERU_VL_API_KEY" \
  "https://<server>/v1/models"
```

In direct mode, `mineru`'s service address and model ID are supplied by the `MINERU_VL_SERVER` and `MINERU_VL_MODEL_NAME` environment variables; `--url` overrides the service address. The program accesses the corresponding `/v1/models` and `/v1/chat/completions`.

Authentication preferentially uses the `MINERU_VL_API_KEY` environment variable; `--api-key` can override it. Avoid putting keys directly on the command line: they may enter shell history or logs.

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY='<your-key>'

./target/release/mineru -p "input.pdf" -o output
```

If a key must be passed temporarily:

```sh
export MINERU_VL_MODEL_NAME="<model-id>"
./target/release/mineru -p "input.pdf" -u "https://<server>/v1" \
  --api-key "<your-key>"
```

---

## Canonical `mineru` command (PDF / images / Office)

`mineru` is the canonical product binary, supporting PDF, image, and Office input and an optional `--api-url` remote API server mode. `--backend=local` is a project-private AnyDoc native-Markdown lane for supported legacy formats and conservative clean text PDFs, isolated in the bundled Rust `mineru-office-convert` helper; it is not a local ML model, `llama-server`, or the official `hybrid-engine`. The helper invokes no Python, Microsoft Office/LibreOffice, model, or network. Direct `hybrid-http-client` now uses a separate Python worker boundary for official MinerU 4.0.0a6; it never enters the 3.4.5 VLM or Office routes.

OOXML conversion requires the `mineru-office-convert` helper. Local legacy and native-PDF extraction also uses this bundled Rust helper; those routes invoke no Python, Microsoft Office/LibreOffice, model, or network. Capabilities depend on two optional features:

```sh
# docx/pptx/xlsx → PDF (via office2pdf, then VLM layout parsing)
cargo build --release --features office
# legacy formats → best-effort text PDF for the non-local VLM lane; local uses the bundled helper for AnyDoc Markdown
cargo build --release --features legacy-office
# both
cargo build --release --features office,legacy-office
```

`mineru` routes by extension: `.docx`/`.pptx`/`.xlsx` keep their existing helper PDF conversion and VLM route. For `.doc`/`.ppt`/`.xls`/`.odt`/`.rtf`/`.epub`/`.ods`/`.odp`/`.csv`, the non-local direct VLM lane first uses the isolated helper to obtain a best-effort, valid text PDF from AnyDoc Markdown, then sends that PDF through the existing PDF/VLM route. This is a text-only fallback and does not preserve Office layout: the original layout, images, tables, formulas, and macros may be lost, and non-ASCII characters may be replaced with `?`. Every successful conversion emits a warning; a batch emits the Office/LibreOffice recommendation once. If conversion cannot produce a valid PDF, the error recommends converting the file with Microsoft Office or LibreOffice to DOCX/XLSX/PPTX first. Explicit `--backend local` runs both legacy AnyDoc Markdown and clean native-PDF extraction in the isolated bundled Rust helper, with no Python, Office application, model, or network. Non-local legacy output is written under `{out}/{stem}/vlm/` with the original legacy input preserved as an origin; local legacy output is `{out}/{stem}/office/{stem}.md`, and local native PDF output is `{out}/{stem}/native/{stem}.md`. The native profile contains only Markdown, not official JSON or assets. Direct Hybrid rejects Office and legacy inputs before spawning; the API path remains fail-closed and does not accept this worker boundary. The `mineru-api` backend semantics otherwise remain unchanged and do not accept `local`.

### Office helper containment

Before conversion, the helper performs mandatory complete preflight validation of OOXML and legacy signatures, and limits input to 32 MiB and output to 64 MiB. Legacy PDF output is a bounded text fallback and does not preserve Office layout.

| Platform | Hard memory limit | Other helper limits |
| --- | --- | --- |
| Linux | `RLIMIT_AS` 1 GiB | CPU 120 seconds, `NOFILE` 256, managed 180-second wall deadline, process-group cleanup |
| Windows | Job Object 1 GiB | CPU 120 seconds, managed 180-second wall deadline, Job tree cleanup |
| macOS | No native hard RSS limit | Mandatory preflight validation, CPU 120 seconds, `NOFILE` 256, managed 180-second wall deadline, process-group cleanup |

Native macOS APIs have no reliable process RSS/address-space hard limit that does not require an entitlement. Deployments accepting untrusted Office documents from the Internet on native macOS must use an external VM or container memory boundary to provide hard memory isolation.

### Direct VLM mode (default)

Without `--api-url`, `mineru` calls the external VLM service directly. The service address and model are supplied by the `MINERU_VL_SERVER`, `MINERU_VL_MODEL_NAME`, and `MINERU_VL_API_KEY` environment variables or overridden by `--url`.

### Direct official Hybrid 4.0.0a6

Direct `-b hybrid-http-client` requires an installed Python environment containing
exactly `mineru==4.0.0a6`. The Rust binary embeds its narrow adapter shim; it does
not bundle Python, MinerU, or model assets. When no worker mode is specified, selection
is automatic after input preflight: one runnable document uses `per-document`, while
multiple runnable documents use `persistent`. The `per-document` mode launches one fresh
subprocess per document, with parent-owned timeout, cancellation, pipe limits, and
descendant cleanup. Accepted inputs are PDF and official image kinds only; OOXML and
legacy Office are rejected before the worker starts.

You may explicitly select `--official-worker-mode per-document` or
`--official-worker-mode persistent`, or set `MINERU_OFFICIAL_WORKER_MODE` to either value.
The persistent setting reuses one worker and its loaded model within one direct CLI run.
This performance mode still admits one active request only: documents
remain sequential and use independent private snapshots and bundles. Startup and
handshake happen once; cancellation or a crash makes the next document create a new
session. A committed request is never automatically retried, and this mode provides no
hard RSS/GPU isolation. An explicit CLI value wins over the environment; with neither
override, the automatic document-count selection applies. The environment setting
applies only to direct `hybrid-http-client`.

On Windows, worker assignment to a `KILL_ON_JOB_CLOSE` Job Object is fail-closed,
but Tokio spawns first and `WindowsJob::attach` runs afterward. A very fast
descendant created before assignment can therefore escape the job; cleanup is
best effort for that narrow race. The official worker has no hard RSS or GPU
quota.

`medium` keeps the official `hybrid-http-client` backend and is local-only, so it
does not require a VLM URL. `high` and `xhigh` use the same official
`hybrid-http-client` backend and require an
explicit HTTP(S) `--url` or `MINERU_VL_SERVER`. `model_stack` is `auto`, `light`,
or `full`; model assets and any `MINERU_MODEL_BASE_DIR`/`MINERU_CONFIG` paths are
user-supplied. Formula/table disable switches are unsupported by the pinned
parser. Results are published separately under `{out}/{stem}/hybrid-v4/` with
`markdown.md`, `middle_json.json`, `content_list.json`, `structured_content.json`,
optional `model_output.json`, and `images/`; they are never passed through the
3.4.5 builders. Direct Hybrid rejects the v3-only VLM transport controls
(`--http-*`, `--max-remote-image-*`, `--max-decoded-pixels`,
`--max-images-per-request`, `--max-redirects`, `--vlm-debug`,
`--temperature-retry`, and their environment spellings), as well as
`--client-side-output-generation`, instead of silently ignoring them. The
official parser fields and project-owned input/output caps remain available.

The project-owned adapter envelope is `mineru-rs-official-worker/1` in `per-document`
mode, or internal persistent `mineru-rs-official-worker/2`; neither is an official
MinerU stdin/stdout protocol.
Configure the interpreter and official paths only with the fields below. Supplied
executable, model, and config paths must be absolute.

Selecting `-b hybrid-http-client` with `--api-url` remains fail-closed:

```text
failed: backend=hybrid-http-client is direct-only; API mode does not support Hybrid
```

The default `vlm-http-client` always uses the existing 3.4.5 VLM route. No
custom AnyDoc/native shortcut, 4.0a effort setting, or fake worker configuration
is attached to `hybrid-http-client`.

```sh
export MINERU_VL_SERVER="https://<server>"
export MINERU_VL_MODEL_NAME="<model-id>"
export MINERU_VL_API_KEY="<your-key>"

./target/release/mineru -p input.pdf -o output
```

### Local AnyDoc mode (`backend=local`)

`local` runs AnyDoc text extraction in the isolated bundled Rust `mineru-office-convert` helper, not in the CLI process and not as a local model. It supports `.doc`, `.ppt`, `.xls`, `.odt`, `.rtf`, `.epub`, `.ods`, `.odp`, `.csv`, and the PDF native Markdown API; build with `legacy-office`:

```sh
cargo build --release --features legacy-office
./target/release/mineru -p old.doc -o output --backend local
```

The legacy output remains `output/old/office/old.md`; a clean native PDF is
written as `output/old/native/old.md`. Native output is Markdown-only and does
not create `document.json`, `middle.json`, `content-list`, or assets. The
assessment rejects scanned, mixed, garbled, empty, low-quality, complex, or
uncertain PDFs with a clear error; it never calls the VLM or silently falls
back. The bundled local helper invokes no Python, Microsoft Office/LibreOffice,
model, or network. Local mode does not use or validate `--url`, `--api-key`, or VLM
connection environment values for AnyDoc. VLM transport flags such as
`--http-*`, `--max-remote-image-*`, and `--vlm-debug` are likewise ignored. It
uses the helper's bounded default policy and currently supports only the input
and output byte limits, including `--office-input-bytes` and
`--office-output-bytes`; helper-only stderr, wall, CPU, NOFILE, memory, and
process-isolation controls are rejected before input work when supplied by flag
or environment unless explicitly supported. Page selection is not supported by
the native local PDF API.

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
| `-m, --method <auto\|txt\|ocr>` | `auto` | Parsing method; ignored by direct `vlm-http-client`, forwarded by official direct Hybrid. |
| `-b, --backend <vlm-http-client\|hybrid-http-client\|local>` | `vlm-http-client` | Backend. `local` invokes the project-private AnyDoc lane in the bundled Rust helper for legacy formats and conservative clean PDFs, rejecting unsupported/uncertain inputs. The local helper invokes no Python, Office application, model, or network. Direct `hybrid-http-client` is the official 4.0.0a6 worker; API Hybrid remains unsupported. |
| `--effort <medium\|high\|xhigh>` | `medium` | Official direct Hybrid effort. `medium` is local-only; `high`/`xhigh` require an explicit HTTP(S) VLM URL. Other direct backends accept only `medium`/`high`. |
| `--model-stack <auto\|light\|full>` | `auto` | Official direct Hybrid model stack. An explicitly supplied value, including `auto`, overrides `MINERU_MODEL_STACK`; when omitted, the environment value is used. |
| `--official-worker-mode <per-document\|persistent>` | automatic (by runnable document count) | Official Hybrid worker lifecycle. One runnable document uses `per-document`; multiple runnable documents use `persistent`. An explicit value overrides `MINERU_OFFICIAL_WORKER_MODE`. |
| `--official-python <absolute-path>` | Python `python3`/`python` | Official direct Hybrid interpreter. Overrides `MINERU_OFFICIAL_PYTHON`; the executable is not bundled. |
| `--official-model-dir <absolute-path>` | None | Official direct Hybrid model root; overrides `MINERU_MODEL_BASE_DIR`. |
| `--official-config <absolute-path>` | None | Official direct Hybrid config; overrides `MINERU_CONFIG`. |
| `-l, --lang <language>` | `ch` | Language code. |
| `-u, --url <URL>` | None | VLM service-address override in direct mode; per-task model-server override in API mode. |
| `-s, --start <n>` | `0` | Start page, **zero-based**. |
| `-e, --end <n>` | None (through the last page) | End page, **inclusive**. |
| `-f, --formula <true\|false>` | `true` | Formula recognition. Explicit boolean; precedence `CLI > MINERU_FORMULA_ENABLE > default`. |
| `-t, --table <true\|false>` | `true` | Table recognition. Explicit boolean; precedence `CLI > MINERU_TABLE_ENABLE > default`. |
| `--image-analysis <true\|false>` | `true` | Image analysis. Explicit boolean; precedence `CLI > MINERU_IMAGE_ANALYSIS_ENABLE > default`. |
| `--log-level <level>` | `info` | Log verbosity: `trace`, `debug`, `info`, `success`, `warning`, `error`, `critical`. Overrides `MINERU_LOG_LEVEL`. |
| `--processing-window-size <n>` | `64` | Page processing window. Overrides `MINERU_PROCESSING_WINDOW_SIZE`. |
| `--page-concurrency <n>` | `64` | Page-pipeline concurrency cap (any positive value); bounds only the number of simultaneously running page pipelines. Actual request-level concurrency is governed by `--http-max-concurrency`/`MINERU_VLM_HTTP_CONCURRENCY`. Overrides `MINERU_OFFICIAL_PAGE_CONCURRENCY`. |
| `--concurrency-model <classic\|two-phase>` | `classic` | Concurrency model: `classic` (long-standing single-encoder pipeline; default) or `two-phase` (per-page semantic work split into encode-all → request-all stages). Overrides `MINERU_OFFICIAL_CONCURRENCY_MODEL`. |
| `--render-workers <n>` | `min(cpu, 8)` | Rendering workers; the effective count is also capped by selected pages. Overrides `MINERU_PDF_RENDER_THREADS`. |
| `--render-timeout-seconds <n>` | `300` | Per-render timeout. Overrides `MINERU_PDF_RENDER_TIMEOUT`. |
| `--batch-size <n>` | `64` | Per-page semantic inference request admission (inference batching), distinct from page concurrency and the processing window; under two-phase it caps per-page request-stage concurrency (additionally bounded by the global request-level semaphore). Overrides `MINERU_BATCH_SIZE`. |
| `--total-deadline-seconds <n>` | `86400` | Per-document total deadline. Overrides `MINERU_TOTAL_DEADLINE_SECONDS`. |
| `--max-pdf-bytes <n>` | `1073741824` | Resident source-PDF cap. Overrides `MINERU_MAX_PDF_BYTES`. |
| `--max-pages <n>` | `10000` | Maximum selected pages per document. Overrides `MINERU_MAX_PAGES`. |
| `--max-page-pixels <n>` | `100000000` | Per-page pixel cap. Overrides `MINERU_MAX_PAGE_PIXELS`. |
| `--max-rendered-image-bytes <n>` | `67108864` | Per-render RGB cap. Overrides `MINERU_MAX_RENDERED_IMAGE_BYTES`. |
| `--max-in-flight-image-bytes <n>` | `536870912` | In-flight RGB budget. Overrides `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES`. |
| `--max-raw-output-bytes <n>` | `134217728` | Per-document raw output budget. Overrides `MINERU_MAX_RAW_OUTPUT_BYTES`. |
| `--max-layout-blocks-per-page <n>` | `256` | Layout block cap per page. Overrides `MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE`. |
| `--max-semantic-requests-per-page <n>` | `128` | Semantic request cap per page. Overrides `MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE`. |
| `--max-encoded-request-bytes <n>` | `16777216` | Encoded request cap. Overrides `MINERU_MAX_ENCODED_REQUEST_BYTES`. |
| `--max-encoded-batch-bytes <n>` | `67108864` | Encoded batch cap. Overrides `MINERU_MAX_ENCODED_BATCH_BYTES`. |
| `--max-total-asset-bytes <n>` | `1073741824` | Total asset cap. Overrides `MINERU_MAX_TOTAL_ASSET_BYTES`. |
| `--max-staged-text-bytes <n>` | `268435456` | Staged text cap. Overrides `MINERU_MAX_STAGED_TEXT_BYTES`. |

All numeric flags are strict: malformed, non-finite, zero-where-invalid, overflowing, or platform-unrepresentable values fail before any network or output work. Precedence is `CLI > environment > compiled default` for every knob.

VLM transport knobs (each also has an environment spelling):

| Flag | Default | Overrides |
| --- | ---: | --- |
| `--http-max-concurrency <n>` | `100` | Global request-level admission semaphore (shared by layout and semantic requests), governing the depth of in-flight requests at the server; consider ≤ the server's vLLM `max_num_seqs`. Overrides `MINERU_VLM_HTTP_CONCURRENCY`. |
| `--http-timeout-seconds <n>` | `600` | `MINERU_VLM_HTTP_TIMEOUT` |
| `--connect-timeout-seconds <n>` | `10` | `MINERU_VLM_CONNECT_TIMEOUT` |
| `--http-max-keepalive-connections <n>` | `20` | `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` |
| `--http-keepalive-expiry-seconds <n>` | `30` | `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` |
| `--http-max-retries <n>` | `3` | `MINERU_VLM_HTTP_MAX_RETRIES` |
| `--http-retry-backoff-factor <f>` | `0.5` | `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` |
| `--max-remote-image-bytes <n>` | `33554432` | `MINERU_VLM_MAX_IMAGE_BYTES` |
| `--max-decoded-pixels <n>` | `100000000` | `MINERU_VLM_MAX_DECODED_PIXELS` |
| `--max-images-per-request <n>` | `64` | `MINERU_VLM_MAX_IMAGES_PER_REQUEST` |
| `--max-redirects <n>` | `3` | `MINERU_VLM_MAX_REDIRECTS` |
| `--http-max-response-bytes <n>` | `10485760` | `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` |
| `--temperature-retry[=<true\|false>]` | Off | Opt-in quality retry for buffered official PDF layout/semantic requests: keeps the base temperature first, then adds `0.2` per retry up to `1.0`. Retry bodies only widen existing positive `top_k` to at least `40` and `top_p` to at least `0.9`; omitted fields and `top_k<=0` unlimited values are left untouched. Bare `--temperature-retry` means `true`, explicit `=false` overrides `MINERU_VLM_TEMPERATURE_RETRY`, and an omitted CLI flag preserves the environment value. It does not affect ordinary `predict`, batch, streaming, `backend=local`, or API-form requests. |
| `--vlm-debug <true\|false>` | `false` | Sends `vllm_xargs.debug` in the VLM request body. Overrides `MINERU_VL_DEBUG_ENABLE`. |

Diagnostic/human-output truncation caps remain compiled and are not configurable. The existing `--max-input-bytes`, `--max-encoded-document-bytes`, and `--max-output-bytes` pairs are unchanged.

For the existing direct `vlm-http-client` lane, non-default values for `--method`, `--effort`, and `--lang` produce a warning and are ignored. Official direct Hybrid forwards these fields to MinerU 4.0.0a6. `--client-side-output-generation` is rejected in both direct Hybrid and API mode.

In API mode, the local VLM transport knobs (`--page-concurrency`, `--concurrency-model`, `--processing-window-size`, `--render-*`, `--batch-size`, all `--http-*`/`--max-remote-image-bytes`/`--max-decoded-pixels`/`--max-images-per-request`/`--max-redirects`/`--http-max-response-bytes`/`--temperature-retry`/`--vlm-debug` and their environment spellings) fail explicitly, because the remote server performs parsing and those controls would have no consumer; `MINERU_VL_SERVER` is submitted as the per-task `server_url` when `--url` is absent.

---

## API server

`mineru-api` is the HTTP API server. The service itself performs no local inference: it accepts documents, calls an external VLM service, then returns archived results.

### Published GHCR image

The published Rust API image is `ghcr.io/agentsyaml/mineru-cli`. It listens on
container port `8000`, serves `GET /health`, writes task output below
`/app/output`, and runs its default command as a non-root user. The release
binaries include the `office,legacy-office` feature set, but the image bundles
Rust binaries only: it contains no Python, `mineru==4.0.0a6`, or model assets.

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
non-root user. The stock image cannot run official Hybrid without a separately
prepared environment explicitly supplied to it; API Hybrid remains fail-closed.
`MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true` is an explicit, per-container opt-in
to the unauthenticated task API; keep it out of the Dockerfile and global image
environment. Publish to loopback first as shown. Broader exposure requires a
private network or an authenticated reverse proxy because the API has no
built-in authentication or task-ownership isolation.

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
| `--concurrency <n>` | `3` | Number of tasks processed concurrently. |
| `--shutdown-on-stdin-eof` | Off | Gracefully exit when stdin closes; suitable for parent-process management. |

`--output-root`, `--concurrency`, and `--shutdown-on-stdin-eof` override their corresponding environment variables; when omitted, the environment-variable value or the table default remains in effect. Explicit values are honored on every platform; there is no macOS concurrency floor.

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `MINERU_API_OUTPUT_ROOT` | `./output` | Output root directory. |
| `MINERU_API_MAX_CONCURRENT_REQUESTS` | `3` | Number of concurrent tasks; non-positive or invalid values cause startup to fail. |
| `MINERU_API_TASK_RETENTION_SECONDS` | `86400` | Retention period for terminal task records. |
| `MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS` | `300` | Cleanup scan interval. |
| `MINERU_API_PUBLIC_BIND_EXPOSED` | Off | Allow binding a non-loopback address. |
| `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` | Off | Allow POST parsing requests when publicly bound. |
| `MINERU_API_SHUTDOWN_ON_STDIN_EOF` | Off | Equivalent to `--shutdown-on-stdin-eof`. |
| `MINERU_API_RECORD_CAP` | `32` | Max concurrent task records. |
| `MINERU_API_FILE_CAP` | `1073741824` | Per-upload file byte cap. |
| `MINERU_API_BODY_CAP` | `1074790400` | Multipart request body byte cap. |
| `MINERU_API_TEXT_CAP` | `65536` | Per-form text field byte cap. |
| `MINERU_API_TEXT_TOTAL_CAP` | `262144` | Aggregate form text byte cap. |
| `MINERU_API_FORM_FIELDS_CAP` | `32` | Max multipart form fields. |
| `MINERU_TASK_RESULT_TIMEOUT_SECONDS` | `3600` | `mineru --api-url` client: task-result timeout in seconds. |
| `MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS` | `600` | Client: result-download timeout in seconds. |
| `MINERU_API_CONNECT_TIMEOUT_SECONDS` | `10` | Client: API connect timeout in seconds. |
| `MINERU_API_ACQUISITION_TIMEOUT_SECONDS` | `60` | Client: task submission/status-acquisition timeout in seconds. |
| `MINERU_API_SEND_TIMEOUT_SECONDS` | `300` | Client: upload timeout in seconds. |
| `MINERU_API_POLL_INTERVAL_SECONDS` | `1` | Client: polling interval in seconds. |
| `MINERU_OFFICE_INPUT_BYTES` | `33554432` | Office helper input byte cap (child-environment). |
| `MINERU_OFFICE_OUTPUT_BYTES` | `67108864` | Office helper output PDF byte cap. |
| `MINERU_OFFICE_STDERR_BYTES` | `4096` | Office helper stderr diagnostic cap. |
| `MINERU_OFFICE_WALL_SECONDS` | `180` | Office helper managed wall-time cap. |
| `MINERU_OFFICE_CPU_SECONDS` | `120` | Office helper CPU-seconds rlimit. |
| `MINERU_OFFICE_NOFILE` | `256` | Office helper NOFILE rlimit. |
| `MINERU_OFFICE_ADDRESS_SPACE_BYTES` | `1073741824` | Office helper address-space rlimit (Linux). |
| `MINERU_OFFICE_ACTIVE_PROCESS_LIMIT` | `8` | Office helper Windows job active-process limit. |
| `MINERU_OFFICE_PROCESS_MEMORY_BYTES` | `1073741824` | Office helper Windows per-process memory limit. |
| `MINERU_OFFICE_JOB_MEMORY_BYTES` | `1073741824` | Office helper Windows job memory limit. |
| `MINERU_OFFICE_PROCESS_TIME_SECONDS` | `120` | Office helper Windows per-process user time. |
| `MINERU_OFFICE_JOB_TIME_SECONDS` | `120` | Office helper Windows per-job user time. |
| `MINERU_OOXML_ARCHIVE_BYTES` | `1073741824` | OOXML preflight archive byte cap. |
| `MINERU_OOXML_EXPANDED_BYTES` | `268435456` | OOXML preflight expanded byte cap. |
| `MINERU_OOXML_XML_ENTRY_BYTES` | `8388608` | OOXML per-XML-entry byte cap. |
| `MINERU_OOXML_XML_TOTAL_BYTES` | `33554432` | OOXML aggregate XML byte cap. |
| `MINERU_OOXML_RATIO` | `500` | OOXML entry compression ratio cap. |
| `MINERU_OOXML_XML_DEPTH` | `128` | OOXML XML depth cap. |
| `MINERU_OOXML_XML_EVENTS` | `100000` | OOXML XML event cap. |
| `MINERU_OOXML_XML_ATTRIBUTES` | `256` | OOXML per-element attribute cap. |
| `MINERU_OOXML_XML_NAMESPACES` | `256` | OOXML per-element namespace cap. |
| `MINERU_ARCHIVE_MAX_ENTRIES` | `100000` | Maximum archive entries (ZIP scan). |
| `MINERU_ARCHIVE_MAX_RATIO` | `1000` | Archive entry compression-ratio cap. |
| `MINERU_ZIP_SCAN_CENTRAL_CAP` | `67108864` | ZIP central-directory scan byte cap (64 MiB). |
| `MINERU_ZIP_SCAN_NAME_CAP` | `4096` | ZIP per-entry name length cap (bytes). |
| `MINERU_ZIP_SCAN_DEPTH_CAP` | `64` | ZIP entry path depth cap. |
| `MINERU_ZIP_SCAN_TOTAL_NAME_CAP` | `33554432` | ZIP aggregate entry-name byte cap (32 MiB). |
| `MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP` | `1000000` | ZIP aggregate path-component cap. |
| `MINERU_PROCESSING_WINDOW_SIZE` | `64` | Page processing window. |
| `MINERU_OFFICIAL_PAGE_CONCURRENCY` | `64` | Page-pipeline concurrency cap (any positive value), bounding only the number of simultaneously running page pipelines; request-level concurrency is governed by `MINERU_VLM_HTTP_CONCURRENCY`. |
| `MINERU_OFFICIAL_CONCURRENCY_MODEL` | `classic` | Concurrency model, one of `classic\|two-phase`. `classic`: the long-standing single-encoder pipeline (default); `two-phase`: splits each page's semantic work into an encode-all → request-all two-stage flow, removing the CPU-encode serialization bottleneck in front of request dispatch (opt-in). |
| `MINERU_MODEL_STACK` | `auto` | Official direct Hybrid stack: `auto\|light\|full`. |
| `MINERU_OFFICIAL_WORKER_MODE` | automatic (by runnable document count) | Official Hybrid worker mode: `per-document\|persistent`. Applies only to direct Hybrid; an explicit CLI value wins. |
| `MINERU_OFFICIAL_PYTHON` | Python `python3`/`python` | Absolute official Hybrid interpreter path. |
| `MINERU_MODEL_BASE_DIR` | None | Absolute official Hybrid model root. |
| `MINERU_CONFIG` | None | Absolute official Hybrid config path. |
| `MINERU_PDF_RENDER_THREADS` | `min(cpu, 8)` | Number of rendering workers. |
| `MINERU_PDF_RENDER_TIMEOUT` | `300` | Timeout in seconds for a single render. |
| `MINERU_FORMULA_ENABLE` | On | Default for formula recognition (strict `true`/`false`, case-insensitive). |
| `MINERU_TABLE_ENABLE` | On | Default for table recognition (strict `true`/`false`). |
| `MINERU_IMAGE_ANALYSIS_ENABLE` | On | Default for image analysis (strict `true`/`false`). |
| `MINERU_LOG_LEVEL` | `info` | Log verbosity; `critical` silences progress. |
| `MINERU_BATCH_SIZE` | `64` | Per-page semantic inference request admission (per-page request-stage cap under two-phase). |
| `MINERU_TOTAL_DEADLINE_SECONDS` | `86400` | Per-document total deadline. |
| `MINERU_MAX_PDF_BYTES` | `1073741824` | Resident source-PDF cap. |
| `MINERU_MAX_PAGES` | `10000` | Maximum selected pages per document. |
| `MINERU_MAX_PAGE_PIXELS` | `100000000` | Per-page pixel cap. |
| `MINERU_MAX_RENDERED_IMAGE_BYTES` | `67108864` | Per-render RGB cap. |
| `MINERU_MAX_IN_FLIGHT_IMAGE_BYTES` | `536870912` | In-flight RGB budget. |
| `MINERU_MAX_RAW_OUTPUT_BYTES` | `134217728` | Per-document raw output budget. |
| `MINERU_MAX_LAYOUT_BLOCKS_PER_PAGE` | `256` | Layout block cap per page. |
| `MINERU_MAX_SEMANTIC_REQUESTS_PER_PAGE` | `128` | Semantic request cap per page. |
| `MINERU_MAX_ENCODED_REQUEST_BYTES` | `16777216` | Encoded request cap. |
| `MINERU_MAX_ENCODED_BATCH_BYTES` | `67108864` | Encoded batch cap. |
| `MINERU_MAX_TOTAL_ASSET_BYTES` | `1073741824` | Total asset cap. |
| `MINERU_MAX_STAGED_TEXT_BYTES` | `268435456` | Staged text cap. |
| `MINERU_VLM_HTTP_CONCURRENCY` | `100` | Global request-level admission semaphore (shared by layout and semantic requests); consider ≤ the server's vLLM `max_num_seqs`. |
| `MINERU_VLM_HTTP_TIMEOUT` | `600` | VLM HTTP request timeout in seconds. |
| `MINERU_VLM_CONNECT_TIMEOUT` | `10` | Connect timeout in seconds. |
| `MINERU_VLM_HTTP_MAX_KEEPALIVE_CONNECTIONS` | `20` | Keepalive pool size. |
| `MINERU_VLM_HTTP_KEEPALIVE_EXPIRY` | `30` | Keepalive expiry in seconds. |
| `MINERU_VLM_HTTP_MAX_RETRIES` | `3` | HTTP retry count. |
| `MINERU_VLM_HTTP_RETRY_BACKOFF_FACTOR` | `0.5` | Retry backoff factor. |
| `MINERU_VLM_MAX_IMAGE_BYTES` | `33554432` | Remote image byte cap. |
| `MINERU_VLM_MAX_DECODED_PIXELS` | `100000000` | Decoded-pixel cap. |
| `MINERU_VLM_MAX_IMAGES_PER_REQUEST` | `64` | Images per request cap. |
| `MINERU_VLM_MAX_REDIRECTS` | `3` | Redirect cap. |
| `MINERU_VLM_HTTP_MAX_RESPONSE_BYTES` | `10485760` | VLM HTTP response cap. |
| `MINERU_VLM_TEMPERATURE_RETRY` | Off | Accepts `1`/`true` to enable (or `0`/`false` to disable) buffered official PDF layout/semantic quality retries. The base temperature is sent first, then retries use `+0.2` up to `1.0`; the setting has its own retry budget and shares the official deadline and response-byte budget. |
| `MINERU_VLM_TEXT_BEFORE_IMAGE` | Off | Place text before the image in the request. |
| `MINERU_VLM_ALLOW_TRUNCATED_CONTENT` | Off | Accept truncated VLM response content. |
| `MINERU_VLM_ALLOW_REMOTE_IMAGES` | Off | Allow fetching images by remote URL. |
| `MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES` | Off | Allow remote images from private/loopback URLs. |
| `MINERU_VLM_END_TOKEN` | `<|im_end|>` | End token for VLM responses. |
| `MINERU_VL_DEBUG_ENABLE` | Off | VLM request debug flag (strict `true`/`false`). |
| `MINERU_VL_SERVER` | None | VLM service base URL (for example, `https://host/v1`); required by `mineru` direct mode and `mineru-api`. |
| `MINERU_VL_MODEL_NAME` | None | Model ID; required by `mineru` direct mode and `mineru-api`. |
| `MINERU_VL_API_KEY` | None | Bearer token for the VLM service. |

Prefix note: `MINERU_VL_*` is the legacy prefix (core VLM service-connection
settings such as the server URL, model ID, and API key); new transport knobs
uniformly use the `MINERU_VLM_*` prefix.

For the canonical CLI, every numeric and boolean variable is strict: booleans accept only case-insensitive `true`/`false` (`1`, `yes`, `on` now fail instead of silently meaning off); `MINERU_VLM_TEMPERATURE_RETRY` additionally accepts `0`/`1` for this opt-in switch. Malformed, non-finite, zero-where-invalid, overflowing, or unrepresentable numeric values fail before any network or output work rather than falling back. (Exception: the three server booleans `MINERU_API_PUBLIC_BIND_EXPOSED` / `MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT` / `MINERU_API_SHUTDOWN_ON_STDIN_EOF` still accept `1`/`true`/`yes`/`on`.)

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

The form accepts `files` file parts and the text fields `lang_list`, `backend`, `effort`, `parse_method`, `formula_enable`, `table_enable`, `image_analysis`, `start_page_id`, `end_page_id`, `server_url`, `response_format_zip`, `return_md`, `return_middle_json`, `return_model_output`, `return_content_list`, `return_images`, `return_original_file`, and `client_side_output_generation`. Duplicate fields, too many fields, or invalid values are rejected.

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

`{stem}` is the safe stem of the path input filename after removing its extension; it is `document` when there is no safe stem. When bytes are passed as `PdfInput::Bytes` without a safe stem, the library API also uses `document_layout.pdf`. Output is first written to a sibling temporary staging directory; on completion, a rename replaces the target directory. An existing directory is first retained as a backup and the backup is removed after successful replacement, avoiding partially written results.

The direct CLI native profile is intentionally separate from the official output
tree:

```text
output/{stem}/native/{stem}.md
```

It contains only native Markdown. It does not provide `document.json`,
`middle.json`, `content-list`, layout previews, or cropped assets; consumers
must not treat it as an official MinerU result archive.

## Library API (minimal example)

The following example uses only the public API and can be placed in your own Tokio async program:

```rust
use mineru::{RunOptions, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(RunOptions::new("input.pdf", "out/")).await?;
    Ok(())
}
```

Authentication comes from `MINERU_VL_API_KEY` (or `MINERU_VL_SERVER` /
`MINERU_VL_MODEL_NAME` for the service endpoint and model). To override the
service endpoint, model, or authentication in code, set the corresponding
public fields on `RunOptions` (`api_url`, `api_key`) before calling `run`.

## Python and Node.js bindings

The `mineru-rs` Python package and the `@alexsun-top/mineru` Node.js package
wrap the same parser. Both expose a `parse()` that returns markdown in memory,
and a `run()` that writes the full output tree.

> The binding packages do not bundle the `mineru-office-convert` helper and do
> not yet support Office-format (`.docx`/`.pptx`/`.xlsx`) input conversion;
> passing an Office document fails with "office conversion is unavailable".
> PDF and image input are unaffected. For Office conversion, use the
> `cargo install mineru --features office` CLI or the `mineru-api` server.

### Python

```sh
uv add mineru-rs   # or: pip install mineru-rs
```

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

`parse()` returns the markdown string in memory, so the caller decides how to
persist it. `run()` writes the full output tree (markdown, JSON, assets) to an
output directory:

```python
import asyncio

import mineru_rs


async def main() -> None:
    await mineru_rs.run("input.pdf", "out/")


asyncio.run(main())
```

Both accept the same VLM keyword options as their binding API; `backend=local`
currently belongs only to the canonical `mineru` CLI and does not change binding
or API backend semantics.

### Node.js

```sh
pnpm add @alexsun-top/mineru   # or: npm install @alexsun-top/mineru
```

```ts
import { writeFile } from 'node:fs/promises'
import mineru from '@alexsun-top/mineru'

const { markdown } = await mineru.parse({ path: 'input.pdf' })
await writeFile('out.md', markdown)
```

`parse()` resolves to `{ markdown, warnings }`; the markdown string is returned
in memory, so the caller decides how to persist it. `run()` writes the full
output tree and resolves to `{ warnings }`:

```ts
import mineru from '@alexsun-top/mineru'

const { warnings } = await mineru.run({ path: 'input.pdf', output: 'out/' })
if (warnings.length) console.warn(warnings)
```

Options mirror the CLI using camelCase names: `apiUrl`, `method`, `backend`,
`effort`, `lang`, `url`, `start`, `end`, `formula`, `table`, `imageAnalysis`,
and `clientSideOutputGeneration`.

## Default resource limits

### Document-limit controls

`--max-input-bytes` / `MINERU_MAX_INPUT_BYTES`, `--max-encoded-document-bytes` / `MINERU_MAX_ENCODED_DOCUMENT_BYTES`, and `--max-output-bytes` / `MINERU_MAX_OUTPUT_BYTES` accept unsigned decimal bytes (whitespace and `_` are allowed). CLI overrides environment, then the compiled default: 4_293_918_719 input bytes, 8 GiB encoded document bytes, and 8 GiB output bytes. Explicit invalid, zero, overflowing, or platform-unrepresentable values fail; there are no arbitrary hard ceilings — a configured value is used as policy rather than clamped to another constant.

These are disk/document totals, not resident allocations: parsed PDFs and the current PDF compactor reject source PDFs above the resident cap (`--max-pdf-bytes` / `MINERU_MAX_PDF_BYTES`, default 1 GiB) before `lopdf` loads them, and one VLM response remains capped at 10 MiB (`--http-max-response-bytes`). Configure encoded policy on `mineru-api`; canonical remote mode rejects its encoded override.

| Item | Default |
| --- | ---: |
| PDF size / page count | 1 GiB / 10,000 pages |
| Per-page pixels / rendered RGB image | 100,000,000 / 64 MiB |
| Response body / all assets | 10 MiB / 1 GiB |
| Layout blocks per page / page window | 256 / 64 pages |
| Semantic requests per page / inference batch | 128 / 64 |
| Concurrent in-flight rendered images | 512 MiB |
| Request concurrency / rendering workers | 100 / min(cpu, 8) (overrides are still bounded by CPU and selected pages) |
| Official page admission concurrency | 64 (no fixed ceiling) |
| Connection / per-request / total parsing timeout | 10 seconds / 600 seconds / 24 hours |

Memory usage scales with the in-flight image budget: an A4 document at the default 512 MiB budget measures roughly 2.4-2.5 GB RSS, dominated by the resident parsed PDF and per-window rendered RGB (larger documents lean higher). Raising the budget trades memory for speed: 1 GiB costs roughly 4-5 GB RSS for about 10% faster wall time on large documents. In API-server mode, each concurrent task carries this budget, so `MINERU_API_MAX_CONCURRENT_REQUESTS` (default 3) multiplies the footprint; reduce the in-flight budget (`MINERU_MAX_IN_FLIGHT_IMAGE_BYTES` / `--max-in-flight-image-bytes`) on memory-constrained hosts.

### Sources of defaults and capacity

- **Upstream-locked**: 200 DPI, 64-page window, VLM HTTP maximum concurrency 100, HTTP request timeout 600 seconds. Rendering workers are no longer upstream-locked: the default is min(CPU, 8).
- **Rust safeguards**: 10-second connection timeout, 24-hour total timeout, and limits for page count, PDF, assets, responses, rendered images, pixels, in-flight images, and layout blocks.

Support for 10,000 pages is only best effort with high memory: input bytes, final page results, and assets are all retained in memory; it is not an unbounded guarantee. Configure the CLI for available RAM and service-endpoint capacity via environment variables (`MINERU_MAX_*`, `MINERU_VLM_*`, and so on) and command-line options such as `--page-concurrency`, `--render-workers`, and `--total-deadline-seconds`. All limits, concurrency values, and worker counts must be greater than zero; all timeouts must be nonzero, and the per-request timeout must not exceed the total timeout.

## Input limits and how to raise them

The pipeline enforces size limits at several independent stages. When a limit is hit, the error message names the file, its size, the limit value, and the knob (flag or environment variable) that raises it; a single failing document does not abort the batch — remaining documents continue. Local parsing of a large file consumes memory roughly proportional to the file size (the disk/document totals above are separate from the resident cap below).

| Limit | Default | Flag | Environment | Enforced at |
| --- | ---: | --- | --- | --- |
| Resident source-PDF cap `max_pdf_bytes` | 1 GiB | `--max-pdf-bytes` | `MINERU_MAX_PDF_BYTES` | File read and local PDF parsing (including PDFs produced by Office conversion) |
| Input transfer cap `max_input_bytes` | 4_293_918_719 (≈4 GiB) | `--max-input-bytes` | `MINERU_MAX_INPUT_BYTES` | Input ingestion / transfer |
| Output cap `max_output_bytes` | 8 GiB | `--max-output-bytes` | `MINERU_MAX_OUTPUT_BYTES` | Output generation |
| OOXML archive cap | 1 GiB | `--ooxml-archive-bytes` | `MINERU_OOXML_ARCHIVE_BYTES` | Office document preflight |
| Office conversion input cap | 32 MiB | `--office-input-bytes` | `MINERU_OFFICE_INPUT_BYTES` | Office conversion |
| Server-side file cap (with `--api-url`) | 1 GiB | `--file-cap` (server: `mineru-api`) | `MINERU_API_FILE_CAP` (server) | Upload at the server |

## Limitations and troubleshooting

- Hayro does not support encrypted PDFs; rendering of complex/advanced PDF effects may differ from other renderers. Invalid PDFs, inconsistent page mappings, size limits, or rendering errors fail explicitly and are not silently skipped.
- The preview supports page rotations `0/90/180/270`. Its goal is usable visual and semantic alignment; because annotations are written and PDF serialization changes, the preview file's bytes are not identical to the original PDF. Other rotations fail.
- `401` usually means a missing or invalid API key; `404` usually means an incorrect `--url` / `MINERU_VL_SERVER` path. Confirm the service actually exposes `/v1/models` and `/v1/chat/completions`.
- If model checking fails (the configured model was not returned by `GET /v1/models`, or no model is configured and the endpoint returns more than one), confirm that `data` returned by `GET /v1/models` contains the selected ID, and check authentication and the base URL.
- `no valid layout tokens` means the service response does not contain the layout tokens required by MinerU; choose a compatible MinerU VLM model/service rather than a general chat model.
- `limit exceeded` means a resource limit from the table above was exceeded; reduce the input or adjust and validate the configuration in a library caller. PDFs unsupported by Hayro must be processed first using a file/rendering workflow that supports the relevant PDF features, then retried.
