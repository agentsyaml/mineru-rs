"""Protocol support shared by the MinerU official worker entry point.

When embedded by Rust, this source is concatenated before
``mineru_official_worker.py``.  The entry point imports these exports only when
the protocol source was not already included, so the combined source does not
need a sibling module at runtime.
"""

from __future__ import annotations

import asyncio
import contextlib
import importlib.metadata
import json
import io
import os
from collections import deque
from collections.abc import Callable
from pathlib import Path
import sys
from typing import cast


WORKER_SOURCE_CONTRACT = (
    "embed mineru_official_worker_protocol.py before "
    "mineru_official_worker.py; the combined source is self-contained"
)
PROTOCOL = "mineru-rs-official-worker/1"
PERSISTENT_PROTOCOL = "mineru-rs-official-worker/2"
PACKAGE_VERSION = "4.0.4"
SCHEMA_VERSION = "2.0"
BUNDLE_NAME = "hybrid-v4"
PROTOCOL_CAP = 64 * 1024
DIAGNOSTIC_CAP = 64 * 1024
PERSISTENT_FRAME_CAP = 64 * 1024
PERSISTENT_RECENT_REQUEST_CAPACITY = 64
PERSISTENT_TIERS = ("standard", "advanced")
# Mirror src/official_worker.rs PERSISTENT_OCR_MODES: this lane only ever
# requests "auto", so the handshake must advertise exactly that set.
PERSISTENT_OCR_MODES = ("auto",)
PERSISTENT_INPUT_FORMATS = (
    "pdf", "png", "jpeg", "jpg", "jp2", "webp", "gif", "bmp", "tiff",
)
PERSISTENT_CAPABILITIES = {
    "tiers": list(PERSISTENT_TIERS),
    "ocr_modes": list(PERSISTENT_OCR_MODES),
    "input_formats": list(PERSISTENT_INPUT_FORMATS),
    "bundle_name": BUNDLE_NAME,
    "cancellation": "process-terminate",
}

# Exact per-document request field set (parse_async kwargs plus transport
# metadata).  Optional fields may be omitted entirely or be null.
DOCUMENT_REQUEST_FIELDS = frozenset(
    {"protocol", "request_id", "max_bundle_bytes", "bundle_name", "input_path",
     "bundle_path", "tier", "ocr_mode", "image_analysis"}
)
DOCUMENT_OPTIONAL_FIELDS = frozenset(
    {"page_range", "vlm_server_url", "vlm_api_key", "vlm_model", "model_home", "config"}
)

# Only these variables are injected into the environment.  Legacy MinerU
# variables such as MINERU_VL_API_KEY and MINERU_VL_MODEL_NAME are neither
# injected nor popped: upstream conflicts with an explicit VlmConfig api_key,
# so relying on them would be incorrect.
_REQUEST_ENV_KEYS = (("MINERU_HOME", "model_home"), ("MINERU_CONFIG", "config"))

__all__ = (
    "WORKER_SOURCE_CONTRACT", "PROTOCOL", "PERSISTENT_PROTOCOL", "PACKAGE_VERSION",
    "SCHEMA_VERSION", "BUNDLE_NAME", "PROTOCOL_CAP", "DIAGNOSTIC_CAP", "LimitError",
    "BoundedCapture", "_bounded_text", "_emit", "_persistent_main", "_response",
    "_validate_document_request", "_document_parse_kwargs", "_document_env",
)


class LimitError(RuntimeError):
    pass


def _bounded_text(value: str, limit: int = DIAGNOSTIC_CAP) -> str:
    return value.encode("utf-8", "replace")[:limit].decode("utf-8", "ignore")


class BoundedCapture(io.TextIOBase):
    def __init__(self, limit: int) -> None:
        super().__init__()
        self.limit = limit
        self.parts: list[str] = []
        self.size = 0

    def write(self, text: str) -> int:
        remaining = self.limit - self.size
        if remaining <= 0:
            return len(text)
        encoded = text.encode("utf-8", "replace")
        kept = encoded[:remaining]
        self.parts.append(kept.decode("utf-8", "ignore"))
        self.size += len(kept)
        return len(text)

    def getvalue(self) -> str:
        return "".join(self.parts)


class _PersistentRecentRequests:
    def __init__(self) -> None:
        self.entries: deque[tuple[str, str, str]] = deque()
        self.request_ids: set[str] = set()
        self.paths: set[str] = set()

    def remember(self, request_id: str, input_path: str, bundle_path: str) -> None:
        if len(self.entries) == PERSISTENT_RECENT_REQUEST_CAPACITY:
            old_request_id, old_input_path, old_bundle_path = self.entries.popleft()
            self.request_ids.remove(old_request_id)
            self.paths.remove(old_input_path)
            self.paths.remove(old_bundle_path)
        self.entries.append((request_id, input_path, bundle_path))
        self.request_ids.add(request_id)
        self.paths.update((input_path, bundle_path))


def _response(
    request: dict[str, object], status: str, package: str, error: str | None = None
) -> dict[str, object]:
    response: dict[str, object] = {
        "protocol": PROTOCOL,
        "request_id": request.get("request_id", ""),
        "status": status,
        "package_version": package,
        "schema_version": SCHEMA_VERSION,
        "bundle_name": BUNDLE_NAME,
    }
    if error:
        response["error"] = _bounded_text(error)
    return response


def _validate_document_request(request: dict[str, object]) -> None:
    """Strict field-set validation for one per-document request frame."""
    if request.get("protocol") != PROTOCOL:
        raise ValueError("unsupported adapter protocol")
    if request.get("bundle_name") != BUNDLE_NAME:
        raise ValueError("unsupported bundle name")
    present = set(request)
    if not (DOCUMENT_REQUEST_FIELDS <= present <= DOCUMENT_REQUEST_FIELDS | DOCUMENT_OPTIONAL_FIELDS):
        raise ValueError("request has unknown or missing fields")
    if not isinstance(request["request_id"], str):
        raise ValueError("request_id must be a string")
    if request["tier"] not in PERSISTENT_TIERS:
        raise ValueError("request tier capability mismatch")
    if request["ocr_mode"] not in PERSISTENT_OCR_MODES:
        raise ValueError("request ocr_mode capability mismatch")
    if not isinstance(request["image_analysis"], bool):
        raise ValueError("image_analysis must be boolean")
    for name in ("input_path", "bundle_path"):
        if not isinstance(request[name], str) or not request[name]:
            raise ValueError(f"{name} must be a nonempty string")
    max_bundle_bytes = request["max_bundle_bytes"]
    if isinstance(max_bundle_bytes, bool) or not isinstance(max_bundle_bytes, int) or max_bundle_bytes <= 0:
        raise ValueError("max_bundle_bytes must be a positive integer")
    for name in ("page_range", "vlm_server_url", "vlm_api_key", "vlm_model", "model_home", "config"):
        value = request.get(name)
        if value is not None and (not isinstance(value, str) or (name != "page_range" and not value)):
            raise ValueError(f"{name} must be a string")
    vlm_server_url = request.get("vlm_server_url")
    if isinstance(vlm_server_url, str) and not vlm_server_url.startswith(("http://", "https://")):
        raise ValueError("vlm_server_url must be an HTTP(S) URL")


def _document_parse_kwargs(request: dict[str, object]) -> dict[str, object]:
    kwargs: dict[str, object] = {
        "tier": request["tier"],
        "ocr_mode": request["ocr_mode"],
        "image_analysis": bool(request["image_analysis"]),
    }
    page_range = request.get("page_range")
    if page_range:
        kwargs["page_range"] = page_range
    # Upstream VlmConfig (mineru.config) fields are server_url/api_key/model;
    # request frames carry the vlm_-prefixed transport names.
    vlm_fields = {
        {"vlm_server_url": "server_url", "vlm_api_key": "api_key", "vlm_model": "model"}[name]: request[name]
        for name in ("vlm_server_url", "vlm_api_key", "vlm_model")
        if request.get(name)
    }
    if vlm_fields:
        from mineru.config import VlmConfig

        kwargs["vlm_config"] = VlmConfig(**vlm_fields)
    return kwargs


def _document_env(request: dict[str, object]) -> None:
    """Inject only supported MinerU variables; never MINERU_MODEL_STACK."""
    for key, request_key in _REQUEST_ENV_KEYS:
        value = request.get(request_key)
        if value is None:
            os.environ.pop(key, None)
        else:
            os.environ[key] = str(value)


def _emit(response: dict[str, object], diagnostic: str = "") -> None:
    diagnostic = _bounded_text(diagnostic)
    if diagnostic:
        diagnostic = diagnostic.replace("\x00", " ")
        response = {**response, "diagnostic": diagnostic}
        sys.stderr.write(_bounded_text(diagnostic, DIAGNOSTIC_CAP - 1) + "\n")
    if response.get("protocol") == PERSISTENT_PROTOCOL:
        encoded = _persistent_response_bytes(response)
    else:
        limit = PROTOCOL_CAP - 1
        encoded = json.dumps(response, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
        if len(encoded) > limit and isinstance(response.get("diagnostic"), str):
            full = cast(str, response["diagnostic"])
            low, high, best = 0, len(full), None
            while low <= high:
                middle = (low + high) // 2
                response["diagnostic"] = full[:middle]
                candidate = json.dumps(response, ensure_ascii=True, separators=(",", ":")).encode(
                    "utf-8"
                )
                if len(candidate) <= limit:
                    best, low = candidate, middle + 1
                else:
                    high = middle - 1
            if best is None:
                response.pop("diagnostic")
            else:
                encoded = best
        if len(encoded) > limit:
            encoded = json.dumps(
                {
                    key: response[key]
                    for key in (
                        "protocol",
                        "request_id",
                        "status",
                        "package_version",
                        "schema_version",
                        "bundle_name",
                    )
                    if key in response
                },
                separators=(",", ":"),
            ).encode("utf-8")
    stream = sys.__stdout__
    if stream is None:
        raise RuntimeError("stdout is unavailable")
    stream.buffer.write(encoded + b"\n")
    stream.flush()


def _persistent_read_frame() -> dict[str, object] | None:
    raw = sys.stdin.buffer.readline(PERSISTENT_FRAME_CAP + 1)
    if not raw:
        return None
    if len(raw) > PERSISTENT_FRAME_CAP or not raw.endswith(b"\n"):
        raise ValueError("persistent frame exceeds its limit or has no newline")
    try:
        value = json.loads(raw[:-1].decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError("persistent frame is not valid JSON") from error
    if not isinstance(value, dict):
        raise ValueError("persistent frame must be an object")
    return value


def _persistent_string(value: object, name: str, allow_none: bool = False) -> str | None:
    if value is None and allow_none:
        return None
    if not isinstance(value, str) or not value:
        raise ValueError(f"persistent {name} must be a nonempty string")
    return value


def _persistent_required_string(value: object, name: str) -> str:
    result = _persistent_string(value, name)
    if result is None:
        raise ValueError(f"persistent {name} must be a nonempty string")
    return result


def _persistent_start(frame: dict[str, object]) -> dict[str, object]:
    fields = {
        "type", "protocol", "package_version", "schema_version", "model_home",
        "config", "vlm_api_key", "vlm_model", "capabilities",
    }
    if set(frame) != fields:
        raise ValueError("persistent startup frame has unknown or missing fields")
    if frame["type"] != "start" or frame["protocol"] != PERSISTENT_PROTOCOL:
        raise ValueError("persistent startup frame protocol mismatch")
    if frame["package_version"] != PACKAGE_VERSION or frame["schema_version"] != SCHEMA_VERSION:
        raise ValueError("persistent startup package/schema mismatch")
    if frame["capabilities"] != PERSISTENT_CAPABILITIES:
        raise ValueError("persistent startup capability mismatch")
    for name in ("model_home", "config", "vlm_api_key", "vlm_model"):
        _persistent_string(frame[name], name, allow_none=True)
    return frame


def _persistent_handshake(start: dict[str, object]) -> dict[str, object]:
    return {
        "type": "handshake",
        "protocol": PERSISTENT_PROTOCOL,
        "status": "ready",
        "package_version": PACKAGE_VERSION,
        "schema_version": SCHEMA_VERSION,
        "max_in_flight": 1,
        "capabilities": PERSISTENT_CAPABILITIES,
    }


def _persistent_request(
    frame: dict[str, object],
    start: dict[str, object],
    expected_sequence: int,
    recent_requests: _PersistentRecentRequests,
) -> tuple[str, int, str, str]:
    fields = {
        "type", "protocol", "request_id", "sequence", "package_version",
        "schema_version", "bundle_name", "input_path", "bundle_path",
        "max_bundle_bytes", "tier", "ocr_mode", "image_analysis",
    } | DOCUMENT_OPTIONAL_FIELDS
    present = set(frame)
    if not (fields - DOCUMENT_OPTIONAL_FIELDS <= present <= fields):
        raise ValueError("persistent request has unknown or missing fields")
    if frame["type"] != "request" or frame["protocol"] != PERSISTENT_PROTOCOL:
        raise ValueError("persistent request protocol mismatch")
    if frame["package_version"] != PACKAGE_VERSION or frame["schema_version"] != SCHEMA_VERSION:
        raise ValueError("persistent request package/schema mismatch")
    if frame["bundle_name"] != BUNDLE_NAME:
        raise ValueError("persistent request bundle mismatch")
    request_id = _persistent_required_string(frame["request_id"], "request_id")
    if request_id in recent_requests.request_ids:
        raise ValueError("persistent request id was repeated")
    sequence = frame["sequence"]
    if isinstance(sequence, bool) or not isinstance(sequence, int) or sequence != expected_sequence:
        raise ValueError("persistent request sequence was repeated or out of order")
    if frame["tier"] not in PERSISTENT_TIERS:
        raise ValueError("persistent tier capability mismatch")
    if frame["ocr_mode"] not in PERSISTENT_OCR_MODES:
        raise ValueError("persistent ocr_mode capability mismatch")
    if not isinstance(frame["image_analysis"], bool):
        raise ValueError("persistent image_analysis must be boolean")
    for name in ("vlm_server_url", "vlm_api_key", "vlm_model", "model_home", "config"):
        _persistent_string(frame[name], name, allow_none=True)
    vlm_server_url = frame["vlm_server_url"]
    if isinstance(vlm_server_url, str) and not vlm_server_url.startswith(("http://", "https://")):
        raise ValueError("persistent vlm_server_url must be an HTTP(S) URL")
    input_path = _persistent_required_string(frame["input_path"], "input_path")
    bundle_path = _persistent_required_string(frame["bundle_path"], "bundle_path")
    if (
        input_path == bundle_path
        or input_path in recent_requests.paths
        or bundle_path in recent_requests.paths
    ):
        raise ValueError("persistent request reused a private path")
    max_bundle_bytes = frame["max_bundle_bytes"]
    if isinstance(max_bundle_bytes, bool) or not isinstance(max_bundle_bytes, int) or max_bundle_bytes <= 0:
        raise ValueError("persistent max_bundle_bytes must be positive")
    assert isinstance(sequence, int) and not isinstance(sequence, bool)
    assert isinstance(max_bundle_bytes, int)
    if frame.get("page_range") is not None and (
        not isinstance(frame["page_range"], str) or not frame["page_range"]
    ):
        raise ValueError("persistent page_range must be a nonempty string when supplied")
    recent_requests.remember(request_id, input_path, bundle_path)
    return request_id, sequence, input_path, bundle_path


def _persistent_result(
    request: dict[str, object], status: str, error: str | None = None
) -> dict[str, object]:
    response: dict[str, object] = {
        "type": "result",
        "protocol": PERSISTENT_PROTOCOL,
        "request_id": request["request_id"],
        "sequence": request["sequence"],
        "status": status,
        "package_version": PACKAGE_VERSION,
        "schema_version": SCHEMA_VERSION,
        "bundle_name": BUNDLE_NAME,
    }
    if error:
        response["error"] = _bounded_text(error)
    return response


def _persistent_response_bytes(response: dict[str, object]) -> bytes:
    limit = min(PROTOCOL_CAP, PERSISTENT_FRAME_CAP) - 1
    if (
        response.get("type") == "result"
        and response.get("status") == "error"
        and not response.get("error")
    ):
        response = {**response, "error": "official persistent document failed"}

    def encode(value: dict[str, object]) -> bytes:
        return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode("utf-8")

    encoded = encode(response)
    if len(encoded) <= limit:
        return encoded

    fitted = {key: value for key, value in response.items() if key != "diagnostic"}
    encoded = encode(fitted)
    if len(encoded) <= limit:
        return encoded

    error = fitted.get("error")
    if isinstance(error, str) and error:
        low, high = 1, len(error)
        best = ""
        while low <= high:
            middle = (low + high) // 2
            fitted["error"] = error[:middle]
            if len(encode(fitted)) <= limit:
                best = fitted["error"]
                low = middle + 1
            else:
                high = middle - 1
        if best:
            fitted["error"] = best
            return encode(fitted)
    return encoded


def _persistent_main(bundle_writer: Callable[[Path, int], object]) -> int:
    startup_capture = BoundedCapture(DIAGNOSTIC_CAP)
    try:
        startup = _persistent_read_frame()
        if startup is None:
            raise ValueError("persistent startup frame is missing")
        _persistent_start(startup)
        package = importlib.metadata.version("mineru")
        if package != PACKAGE_VERSION:
            raise ValueError("MinerU package version is not 4.0.4")
        with contextlib.redirect_stdout(startup_capture), contextlib.redirect_stderr(startup_capture):
            _document_env(startup)
            parser = importlib.import_module("mineru.parser")
        _emit(_persistent_handshake(startup), startup_capture.getvalue())
    except Exception as error:
        _emit(
            {
                "type": "error",
                "protocol": PERSISTENT_PROTOCOL,
                "status": "error",
                "package_version": locals().get("package", ""),
                "schema_version": SCHEMA_VERSION,
                "bundle_name": BUNDLE_NAME,
                "error": _bounded_text(str(error)),
            },
            startup_capture.getvalue(),
        )
        return 1

    recent_requests = _PersistentRecentRequests()
    expected_sequence = 1
    while True:
        try:
            request = _persistent_read_frame()
            if request is None:
                return 0
            _persistent_request(
                request,
                startup,
                expected_sequence,
                recent_requests,
            )
        except Exception as error:
            _emit(
                {
                    "type": "error",
                    "protocol": PERSISTENT_PROTOCOL,
                    "status": "error",
                    "package_version": PACKAGE_VERSION,
                    "schema_version": SCHEMA_VERSION,
                    "bundle_name": BUNDLE_NAME,
                    "error": _bounded_text(str(error)),
                }
            )
            return 1

        capture = BoundedCapture(DIAGNOSTIC_CAP)
        try:
            kwargs = _document_parse_kwargs(request)
            with contextlib.redirect_stdout(capture), contextlib.redirect_stderr(capture):
                result = asyncio.run(parser.parse_async(request["input_path"], **kwargs))
                result.save(
                    bundle_writer(
                        Path(cast(str, request["bundle_path"])),
                        int(cast(int, request["max_bundle_bytes"])),
                    )
                )
            _emit(_persistent_result(request, "ok"), capture.getvalue())
        except Exception as error:
            _emit(
                _persistent_result(request, "error", _bounded_text(str(error))),
                capture.getvalue(),
            )
        expected_sequence += 1
