# Opt-in official-v1 HTTP API

This guide describes the declared source interface for the additive official
MinerU 4.0.10 API in `mineru-api`. It is opt-in and separate from the existing
legacy multipart/task routes. Concentrated build/test checks for this
implementation are pending, as is independent review. This is not a
release-readiness or compatibility claim.

| Surface | Input/options | Result |
| --- | --- | --- |
| Legacy Rust `run` and Python/Node `run`/`parse` | Existing 3.4.5 behavior; no rich result controls | Existing output tree or Markdown/warnings |
| Rich local SDK | Six parse controls; 28 recognized extensions; external 4.0.10 worker | Owned result/assets and nine render targets |
| Official-v1 HTTP API | Upload/job workflow; tier, OCR mode, output formats, per-file PDF range only | Four outputs: Markdown, middle JSON, structured content, ZIP |

These surfaces are additive and have separate input/options/result contracts.
The HTTP API does not expose rich local rendering or assets as an in-memory
`OfficialResult`. This service is not a WebUI, router, or model-serving runtime.

The server does not perform local model inference. Jobs use the separately
installed MinerU 4.0.10 / DocVortex 0.5.4 Python worker (Python 3.10 through
3.14). The current checkout version is `0.3.1`; this version-only change does
not publish a release. Existing routes are not replaced by `/v1`.

## Enablement and trusted runtime

`--official-api` is required to register the upload/files/jobs service. The
default listener remains `127.0.0.1:8000`; the existing `--host` and `--port`
select its listener. Example, using actual startup flags:

```sh
mineru-api --host 127.0.0.1 --port 8000 --official-api \
  --official-python /opt/mineru-4.0.10/bin/python \
  --official-worker-mode persistent --official-timeout-secs 600
```

`--official-worker-mode` accepts `per-document` or `persistent` and defaults to
`persistent`; `--official-timeout-secs` defaults to 600 and must be positive.
`--official-python`, when supplied, must be an absolute path to an existing
executable file. When omitted, the runtime uses its default Python lookup
through `PATH`. `--official-config` must name an existing
absolute file; `--official-model-dir` an existing absolute directory. These are
trusted server startup settings, never request/job options. Supplying any
`--official-*` setting without `--official-api` is rejected. The runtime uses
external Python/MinerU/model assets; no weights are downloaded or bundled.

When disabled, the legacy server remains available and `/v1/health` advertises
empty source and output-format capabilities. When enabled, the official router
is merged alongside the existing routes. Its operational discovery endpoints
include `GET /v1/health`, `/v1/tiers`, `/v1/models`, `/v1/models/{id}`, and
`/v1/usage`. `/v1/models` is intentionally empty and a model-id lookup returns
not found; these endpoints do not promise cloud model discovery or availability.

## Endpoints and upload flow

| Method and path | Purpose |
| --- | --- |
| `POST /v1/uploads` | Reserve an upload; returns pending status, relative `upload_url`, `upload_method: "PUT"`, and required upload headers. |
| `PUT /v1/uploads/{id}/content` | Send exactly the declared raw bytes, not multipart or JSON. |
| `POST /v1/uploads/{id}/complete` | Complete and verify the upload; response contains its `file.id`. |
| `GET /v1/uploads/{id}`, `POST /v1/uploads/{id}/cancel` | Inspect or cancel an incomplete upload. |
| `POST /v1/parse/jobs` | Create a parse job; returns `202 Accepted` and a queued job. |
| `GET /v1/parse/jobs/{id}`, `GET /v1/parse/jobs` | Poll a job or list jobs. |
| `DELETE /v1/parse/jobs/{id}` | Cancel a queued/running job. |
| `GET /v1/files`, `GET /v1/files/{id}`, `DELETE /v1/files/{id}` | List, inspect, or delete retained file metadata/content records. |
| `GET /v1/files/{id}/content` | Download an output artifact. Source-file content is forbidden. |

Create upload JSON has strict fields `filename`, positive `bytes`, required
`mime_type`, optional `purpose` (defaults to `parse`; allowed values are
`parse` and `input_image`), optional lowercase 64-hex `sha256sum`, and optional
`expires_after: {"anchor":"created_at","seconds":...}`. The server computes
the digest even when the caller omits it. Uploads default to one-hour expiry;
requested expiry must be at least one hour and cannot exceed server retention.
The file-store retention must itself be at least one hour.

For a pending upload, use the returned `upload_url`, `upload_method`, and
`upload_headers` unchanged: send a single raw `PUT` with that content type and
the exact declared length, then post an optional digest body:

```json
{"sha256sum":"lowercase-64-character-hex-digest"}
```

The completed response carries `file: {"id":"...", ...}`. Use that id in a
job's `file_id` source. Completion may be repeated for the same upload; a
completed upload cannot be canceled. Do not assume content-hash deduplication.

Job creation accepts only `files`, optional `tier`, optional `ocr_mode`, and
`output_formats`. Each file contains `source` and optional `page_range`. The
only source variants are:

```json
{"type":"file_id","file_id":"file_..."}
```

or inline data:

```json
{"type":"inline","name":"input.pdf","data":"<base64>"}
```

Inline data is base64 and defaults to an explicit HTML context with no local
resource root. There are no URL, local-path, callback, per-job Python/config/
model-path, source-context, VLM-config, or image-analysis fields. Unknown
fields are rejected. A file-id source must refer to a retained `parse` upload.

`tier` is one of `flash`, `basic`, `standard`, `advanced`; `ocr_mode` is
`auto`, `txt`, or `ocr` (default `auto`). When omitted, PDF/images use
`standard`, native filename formats use `flash`; a job with any native file
reports `flash` as its job tier. Native formats require `flash`. `page_range`
is per-file and PDF-only. It accepts `all`, comma-separated page numbers/ranges,
or relative `rN` endpoints (for example `1-5`, `r3-r1`, or `all`); surrounding
whitespace is trimmed, as in `" r3-r1, 5 "`. Mixed absolute/relative range
endpoints (for example, `3-r1` or `r3-5`) are accepted; ordering is deferred to
the document's page count. `output_formats` must be a
nonempty unique subset of `markdown`, `middle_json`, `structured_content`, and
`zip`; when omitted it defaults to `markdown`. These are the HTTP API's four
outputs, not the nine local SDK render targets.

For example, after completing an upload as above:

```json
{
  "files": [{
    "source": {"type":"file_id","file_id":"file_..."},
    "page_range": "all"
  }],
  "tier": "standard",
  "ocr_mode": "auto",
  "output_formats": ["markdown", "middle_json"]
}
```

Poll the returned job id with `GET /v1/parse/jobs/{job_id}`. Job states are
`queued`, `running`, `completed`, `partial`, `failed`, or `canceled`; each
file has its own status/error. Completed `output_files` is a map keyed by
requested format, with references shaped as `{ "file_id": "file_...",
"bytes": 123 }`. Download each artifact's bytes from
`GET /v1/files/{file_id}/content`.

## Limits and lifecycle

| Resource | Declared limit/behavior |
| --- | --- |
| Uploaded file | Up to 16 MiB, further reduced by configured file/body caps. |
| Inline source | Up to 1 MiB decoded bytes. |
| Job | 1–100 files and 64 MiB aggregate input. |
| In-memory input pool | 256 MiB per service. |
| Retained artifact pool | 256 MiB per service; individual outputs and aggregate job artifacts are capped at 64 MiB. |
| Metadata | 1,024 retained metadata entries. |
| ZIP artifact | At most 4,096 entries and 64 MiB expanded data; clients also bound downloaded artifact bytes. |
| Parse | At most 1,000 pages per file. |
| Retention | At least one hour; upload default expiry is one hour and cannot exceed configured retention. |

The file/input and artifact pools are distinct bounded service pools and may
reject admissions when exhausted. These are HTTP-service limits, not the rich
local SDK's 64 MiB retained-result / 1 GiB worker-bundle limits.

The official job runner shares the legacy service's job-slot/page-gate
admission and retention policy. It processes jobs and files sequentially; this
API does not create parallel sibling parses. Canceling a queued job marks it
canceled immediately. Canceling a running job sets a flag and waits for the
current parse to return; its outputs are discarded and remaining files are
skipped. It does not kill a sibling process. Shutdown stops new admissions,
flags queued/running work, drains the active parse and worker, then clears
retained files/jobs. A running cancellation or shutdown can therefore take
close to the configured per-parse timeout.

## Rust client

`OfficialApiClient` accepts owned input bytes, uploads them, submits a
restricted job, polls it, and downloads the returned artifacts. Its remote
options contain only tier, OCR mode, and requested HTTP outputs; per-file page
ranges belong to each input. This client result is metadata plus owned artifact
bytes, not a local `official::OfficialResult` restored through Python.

```rust
use mineru::{
    official::{OfficialOcrMode, OfficialTier},
    official_api::{OfficialApiClient, OfficialRemoteParseOptions},
    official_api::client::{OfficialInput, OfficialOutputFormat},
};

let client = OfficialApiClient::new("http://127.0.0.1:8000")?;
let input = OfficialInput::new("input.pdf", std::fs::read("input.pdf")?)
    .with_page_range("all");
let options = OfficialRemoteParseOptions {
    tier: Some(OfficialTier::Flash),
    ocr_mode: Some(OfficialOcrMode::Auto),
    output_formats: vec![OfficialOutputFormat::Markdown,
                         OfficialOutputFormat::MiddleJson],
};
let result = client.parse(input, &options).await?;
if let Some(markdown) = result.markdown() {
    println!("{markdown}");
}
```

`parse_batch` submits one job and returns per-input outcomes in input order.
The client uses only same-origin follow-up URLs, disables redirects and proxy
use, bounds JSON/artifact downloads, and validates downloaded ZIP paths,
symlinks, entry count, and expanded size. It does not fall back to the legacy
API or restore a Python SDK result.
