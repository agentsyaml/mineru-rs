# Opt-in official-v1 HTTP API

This guide describes the opt-in MinerU 4.2.3 task API in `mineru-api`. It
replicates the route set, request bodies, response bodies, status codes, and
error envelope of the upstream parse server. The
[compatibility matrix](compatibility.md#route-and-format-matrix) compares it with
the CLI and SDK. This API returns requested artifacts, not an in-memory SDK
`OfficialResult`.

Tasks use the separately installed MinerU 4.2.3 / DocVortex 0.6.24 Python
worker (Python 3.11–3.14); the server never performs local model inference. The
exact upstream tuple, including wheel digests, is recorded in
[`upstream-baseline.json`](upstream-baseline.json).

## Enablement and trusted runtime

`--official-api` is required to register the task/files service. The default
listener remains `127.0.0.1:8000`; the existing `--host` and `--port` select its
listener. Example, using actual startup flags:

```sh
mineru-api --host 127.0.0.1 --port 8000 --official-api \
  --official-python /opt/mineru-4.2.3/bin/python \
  --official-worker-mode persistent --official-timeout-secs 600
```

`--official-worker-mode` accepts `per-document` or `persistent` and defaults to
`persistent`; `--official-timeout-secs` defaults to 600 and must be positive.
`--official-python`, when supplied, must be an absolute path to an existing
executable file. When omitted, the runtime uses its default Python lookup
through `PATH`. `--official-config` must name an existing absolute file;
`--official-model-dir` an existing absolute directory. These are trusted server
startup settings, never request or task options. Supplying any `--official-*`
setting without `--official-api` is rejected. The runtime uses external
Python/MinerU/model assets; no weights are downloaded or bundled.

When disabled, the legacy server remains available and `/v1/health` advertises
empty source and output-format capabilities. When enabled, the task router is
mounted after the discovery router.

## Routes

| Method and path | Purpose |
| --- | --- |
| `POST /v1/tasks` | Create a task from a JSON manifest. Returns `202`, the task id, and per-file upload links. |
| `GET /v1/tasks/{task_id}` | Task detail: status, progress, per-file state, parse metadata, and result URLs. |
| `PUT /v1/tasks/{task_id}/uploads/{file_id}` | Send one file's raw bytes with `Content-Type: application/octet-stream`. |
| `POST /v1/tasks/{task_id}/cancel` | Cancel a task and return the terminal per-file snapshot. |
| `GET /v1/health` | Status, version, advertised output formats and sources, device. |
| `GET /v1/models`, `GET /v1/models/{model}` | Single virtual model id; `404` `model_not_found` otherwise. |
| `GET /v1/tiers` | Enabled quality tiers and their current model. |
| `GET /v1/files/{file_id}` | Retained file object metadata. |
| `GET /v1/files/{file_id}/content` | Download an artifact. |
| `GET /v1/usage` | Service usage counters. |

There is no `/v1/uploads` family, no `GET /v1/tasks/{task_id}/result`, no
`POST /v1/file_parse`, no file listing, and no file deletion. A file is uploaded
against the task that owns it, and a finished task exposes its artifact through a
per-file `result_url`.

## Create a task

`POST /v1/tasks` accepts exactly `application/json`. The media type may carry
parameters (`application/json; charset=utf-8`), but any query string is rejected
with `400` `invalid_request`: parameters belong in the body. Unknown fields are
rejected at every level.

```json
{
  "files": [
    {"file_name": "report.pdf", "bytes": 5242880, "page_range": "1-40"},
    {"file_name": "scan.png", "url": "https://example.com/scan.png"}
  ],
  "tier": "standard",
  "ocr_mode": "auto",
  "page_range": "",
  "output_formats": ["default"]
}
```

`files` holds 1–100 entries. Each entry supplies **either** `bytes` **or** `url`:

- `bytes` is the positive declared length. It is a consistency check against the
  upload, not an upload cap.
- `url` must be `http` or `https` and must resolve only to public unicast
  addresses; see [Security](#security).
- `sha256sum`, when present, is 64 lowercase hex characters and is verified
  against the received bytes.
- `page_range` on a file overrides the task-level value.

`file_name` must be a logical basename: 1–1024 characters with no `/`, `\`, `:`,
or Unicode control characters, and it may not be `.` or `..`. Spaces and leading
dots are allowed.

`tier` is `flash`, `basic`, `standard`, or `advanced`. When omitted, the server
resolves one: a task whose inputs are all flash-only extensions uses `flash`,
otherwise the first available task tier. `ocr_mode` is `auto` (default), `txt`,
or `ocr`. `output_formats` is a non-empty list of `default`, `docx`, `html`, and
`latex`; duplicates are removed preserving order, and the default is
`["default"]`. These four formats are the HTTP API's vocabulary, not the nine
local SDK render targets.

The `202` response carries the task id, the task status, and one entry per
input:

```json
{
  "task_id": "job_7f3c1a9e2b40",
  "status": "waiting-file",
  "files": [
    {
      "file_id": "job_7f3c1a9e2b40-1",
      "file_name": "report.pdf",
      "status": "waiting-file",
      "upload": {
        "url": "http://127.0.0.1:8000/v1/tasks/job_7f3c1a9e2b40/uploads/job_7f3c1a9e2b40-1",
        "headers": {"Content-Type": "application/octet-stream"},
        "expires_at": 1794000000
      }
    },
    {
      "file_id": "job_7f3c1a9e2b40-2",
      "file_name": "scan.png",
      "status": "pending"
    }
  ]
}
```

A file that supplies `url` is already `pending` and has no `upload` link. A file
that supplies `bytes` starts in `waiting-file` and gets a link valid for the
upload TTL. URLs are absolute when the request carries a `Host` header.

## Upload and poll

Send the raw bytes to the returned `upload` URL with exactly the returned
headers. Do not send multipart or JSON. Repeating the same `PUT` is idempotent:
the stored bytes and digest are kept and the response is `200`. Uploading more
or fewer bytes than declared fails with `400` `upload_size_mismatch`; an empty
body fails with `400`; a digest mismatch fails with `400` `file_hash_mismatch`;
and an upload against a canceled task fails with `409` `task_already_terminal`.

```json
{"task_id": "job_7f3c1a9e2b40", "file_id": "job_7f3c1a9e2b40-1", "status": "pending"}
```

Poll `GET /v1/tasks/{task_id}`. Task status is `waiting-file`, `pending`,
`running`, `completed`, `partial`, `failed`, or `canceled`; a file never reports
`partial`. Each file entry adds `bytes`, `parse`, `result_url`, and `error` as
they become available:

- `parse` is `{tier, duration_ms?, parser_version?}` and appears once parsing
  finished.
- `result_url` appears only for a `completed` file and points at that file's
  single artifact.
- `progress` counts `completed`, `failed`, and `total` files.
- `started_at` is the earliest per-file start, or absent before any file starts.
- `error` is an envelope body (see below) describing the first failure.

`POST /v1/tasks/{task_id}/cancel` returns `200` with the terminal snapshot:
task status `canceled`, `finished_at`, and per-file `status`, `parse`,
`result_url`, and `error`. Canceling an already terminal task is a no-op that
returns its current snapshot. Canceling a queued task takes effect immediately;
canceling a running task sets a flag and waits for the active parse to return,
then discards its output and skips the remaining files.

## Outputs

Each input file produces exactly one artifact, chosen by `output_formats`:

| Selection | Artifact |
| --- | --- |
| `default` | `result.zip` with `markdown.md`, `middle_json.json`, `structured_content.json`, plus `images/` when the document has assets. |
| `docx` only | `document.docx`, returned directly rather than zipped. |
| `html` only | `document.html`, with document images embedded, returned directly. |
| `latex` only | `result.zip` containing `document.tex` and `images/`. |
| Any other combination | `result.zip` containing the selected files, plus `images/` when `default` or `latex` is selected. |

Archives are deflate-compressed, capped at 4,096 entries and 64 MiB of expanded
data, and list assets in sorted order. `GET /v1/files/{file_id}/content` serves
the artifact with a media type chosen by suffix: `.zip` is `application/zip`,
`.docx` is the OOXML document type, `.html` is `text/html`, and anything else is
`application/octet-stream`.

## Errors

Every failure uses one envelope, and there is no `type` field:

```json
{"error": {"code": "upload_size_mismatch", "message": "Declared size does not match uploaded bytes", "param": "files.0.bytes"}}
```

`param` is omitted when the failure is not attributable to one input. Codes
include `invalid_request`, `page_range_invalid`, `request_interrupted`,
`upload_interrupted`, `upload_size_mismatch`, `file_hash_mismatch`,
`task_already_terminal`, `task_not_found`, `upload_not_found`,
`file_not_found`, `unsupported_source`, `url_source_failed`,
`quality_tier_unavailable`, `model_not_found`, and `internal_error`. Validation
failures name the offending path, such as `files.0.page_range`. A malformed
page range reports `page_range_invalid` with the rejected text quoted back, for
example `Invalid page range '5-2': reversed ranges are not supported`.

## Limits and lifecycle

| Resource | Limit |
| --- | --- |
| Task | 1–100 files. |
| Uploaded file | Up to 16 MiB, further reduced by configured file/body caps. |
| URL source | Same per-file cap; the transfer is bounded while streaming. |
| In-memory input pool | 256 MiB per service. |
| Retained artifact pool | 256 MiB per service; one artifact may not exceed 64 MiB. |
| Metadata | 1,024 retained task and file records. |
| Artifact archive | At most 4,096 entries and 64 MiB of expanded data. |
| Parse | At most 1,000 pages per file. |
| Upload link TTL | One hour. An unfinished upload is expired and its reservation released. |
| Result retention | At least one hour after a task reaches a terminal status. |

`GET /v1/tasks/{task_id}` and `PUT .../uploads/{file_id}` accept the optional
`x-mineru-task-lease-seconds` header. It must be an integer in `0..=3600`; a
larger value is `400` with `"Task result lease must be between 0 and 3600
seconds"`. The value extends the record's retention deadline, never shortens it,
and never reclaims a task that is still running.

The task runner processes files sequentially; this API does not create parallel
sibling parses. Both pools are bounded and reject admissions when exhausted, so
exhaustion surfaces as `429`. Resource accounting is charged while bytes are in
memory and released when a record expires. Shutdown stops new admissions, flags
queued and running work, drains the active parse and worker, then clears retained
records. A running cancellation or shutdown can therefore take close to the
configured per-parse timeout.

## Security

Keep the listener on loopback or behind an authenticated, access-controlled
reverse proxy. The service has no built-in user authentication or per-task
ownership checks; anyone who can reach it can operate on retained tasks and
artifacts. Treat uploaded documents and returned artifacts as sensitive. Python,
config, and model paths are trusted server startup inputs and must come from the
operator, never request data.

A `url` source is fetched by the server, so the resolver refuses to become a
request proxy:

- The URL must use `http` or `https`, and the host must not contain an IPv6 zone
  identifier.
- Every address the host resolves to must be public unicast. Loopback, private,
  link-local, carrier-grade NAT, documentation, multicast, reserved ranges, and
  the IPv4-mapped, NAT64, and 6to4 forms of them are rejected with `400`
  `unsupported_source` and `"URL sources must resolve only to public unicast IP
  addresses"`.
- Each hop is validated the same way, redirects are followed up to ten times
  with the address re-resolved at every hop, and the validated address is pinned
  for the connection while `Host`, SNI, and certificate checks still use the
  original name. A host cannot therefore pass validation and then rebind.
- Proxies are bypassed for these fetches, the transfer is bounded by the
  per-file cap, and the whole fetch has a 60-second budget.

These rules mirror the upstream resolver. They do not make an end-user-facing
deployment safe on their own; the listener still needs authentication in front
of it.

## Rust client

`OfficialApiClient` creates a task from owned input bytes, follows each returned
upload link, polls the task, and downloads the per-file artifact. Its remote
options contain only tier, OCR mode, and requested HTTP output formats;
per-file page ranges belong to each input. The result is metadata plus owned
artifact bytes, not a local `official::OfficialResult` restored through Python.

```rust
use mineru::{
    official::{OfficialOcrMode, OfficialTier},
    official_api::{OfficialApiClient, OfficialRemoteParseOptions},
    official_api::client::{OfficialInput, OfficialOutputFormat},
};

let client = OfficialApiClient::new("http://127.0.0.1:8000")?;
let input = OfficialInput::new("input.pdf", std::fs::read("input.pdf")?)
    .with_page_range("1-20");
let options = OfficialRemoteParseOptions {
    tier: Some(OfficialTier::Standard),
    ocr_mode: Some(OfficialOcrMode::Auto),
    output_formats: vec![OfficialOutputFormat::Default],
};
let result = client.parse(input, &options).await?;
if let Some(markdown) = result.markdown() {
    println!("{markdown}");
}
```

`markdown()` reads the `default` archive's `markdown.md` member; `json(kind)`
decodes `middle_json` or `structured_content` when those members are present.
`parse_batch` creates one task for all inputs and returns per-input outcomes in
input order.

The client uses only same-origin follow-up URLs, disables redirects and proxies,
bounds JSON and artifact downloads, and validates downloaded archive paths,
symlinks, entry count, and expanded size. A transport failure during polling
cancels the task. It does not fall back to the legacy API or restore a Python SDK
result.