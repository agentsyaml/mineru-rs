"""Shared private-file worker contracts and untrusted input validation."""

from __future__ import annotations

import re
from pathlib import Path


class LimitError(RuntimeError):
    pass

PACKAGE_VERSION = "4.0.12"
SCHEMA_VERSION = "2.0"
BUNDLE_NAME = "hybrid-v4"
PROTOCOL = "mineru-rs-official-worker/1"
PERSISTENT_PROTOCOL = "mineru-rs-official-worker/2"
PROTOCOL_CAP = 64 * 1024
DIAGNOSTIC_CAP = 64 * 1024
PERSISTENT_FRAME_CAP = PROTOCOL_CAP
MAX_OPERATION_BYTES = 64 * 1024 * 1024
TIERS = ("flash", "basic", "standard", "advanced")
OCR_MODES = ("auto", "txt", "ocr")
PARSEABLE_FORMATS = ("pdf", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "jp2", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "rtf", "odt", "ods", "odp", "html", "htm", "shtml", "mhtml", "mht", "csv", "tsv", "epub", "ofd")
PARSEABLE_EXTENSIONS = frozenset(PARSEABLE_FORMATS)
TIERED_EXTENSIONS = frozenset("pdf png jpg jpeg webp gif bmp tiff jp2".split())
PERSISTENT_CAPABILITIES = {
    "tiers": list(TIERS), "ocr_modes": list(OCR_MODES),
    "input_formats": list(PARSEABLE_FORMATS), "bundle_name": BUNDLE_NAME,
    "cancellation": "process-terminate",
}
PERSISTENT_LEGACY_CAPABILITIES = {
    **PERSISTENT_CAPABILITIES,
    "input_formats": ["pdf", "png", "jpeg", "jpg", "jp2", "webp", "gif", "bmp", "tiff"],
}
DOCUMENT_REQUIRED_FIELDS = frozenset({
    "protocol", "request_id", "max_bundle_bytes", "bundle_name", "input_path",
    "bundle_path", "tier", "ocr_mode", "image_analysis",
})
DOCUMENT_OPTIONAL_FIELDS = frozenset({
    "page_range", "vlm_server_url", "vlm_api_key", "vlm_model", "model_base_dir",
    "config", "source_context", "official_vlm_config",
})
DOCUMENT_OPERATION_FIELDS = frozenset({"operation", "operation_assets"})
_SECRET_RE = re.compile(r"(?i)(api[_-]?key|token|authorization)(\s*[:=]\s*)([^\s,;}]+)")
_URL_SECRET_RE = re.compile(r"(?i)([?&](?:api[_-]?key|access_token|token)=)[^&#\s]+")
_URL_USERINFO_RE = re.compile(r"(?i)(https?://)[^/@\s]+@")


def safe_operation_path(root: Path, value: object) -> Path:
    if not isinstance(value, str) or not value:
        raise ValueError("operation file path is required")
    relative = Path(value)
    if relative.is_absolute() or any(part in ("", ".", "..") for part in relative.parts):
        raise ValueError("unsafe operation file path")
    base = root.resolve()
    target = root.joinpath(relative)
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise ValueError("operation path contains a symlink")
    if not target.resolve().is_relative_to(base):
        raise ValueError("operation file escapes private root")
    return target


def redact_error(value: object, limit: int = DIAGNOSTIC_CAP) -> str:
    text = _SECRET_RE.sub(r"\1\2[redacted]", str(value))
    text = _URL_SECRET_RE.sub(r"\1[redacted]", text)
    text = _URL_USERINFO_RE.sub(r"\1[redacted]@", text).replace("\x00", " ")
    return text.encode("utf-8", "replace")[:limit].decode("utf-8", "ignore")


def validate_document_request(request: dict[str, object]) -> None:
    if request.get("protocol") != PROTOCOL or request.get("bundle_name") != BUNDLE_NAME:
        raise ValueError("unsupported adapter protocol or bundle")
    keys = set(request)
    if not DOCUMENT_REQUIRED_FIELDS <= keys <= DOCUMENT_REQUIRED_FIELDS | DOCUMENT_OPTIONAL_FIELDS | DOCUMENT_OPERATION_FIELDS:
        raise ValueError("request has unknown or missing fields")
    if request["tier"] not in TIERS or request["ocr_mode"] not in OCR_MODES:
        raise ValueError("unsupported tier or OCR mode")
    if not isinstance(request["image_analysis"], bool):
        raise ValueError("image_analysis must be boolean")
    if isinstance(request["max_bundle_bytes"], bool) or not isinstance(request["max_bundle_bytes"], int) or request["max_bundle_bytes"] <= 0:
        raise ValueError("max_bundle_bytes must be positive")
    for name in ("input_path", "bundle_path"):
        if not isinstance(request[name], str) or not request[name]:
            raise ValueError(f"{name} must be a nonempty string")
    for name in ("page_range", "vlm_server_url", "vlm_api_key", "vlm_model", "model_base_dir", "config"):
        value = request.get(name)
        if value is not None and not isinstance(value, str):
            raise ValueError(f"{name} must be a string")
    if request.get("operation") not in (None, "restore", "serialize", "render"):
        raise ValueError("unsupported official worker operation")
    if request.get("source_context") is not None and not isinstance(request["source_context"], dict):
        raise ValueError("source_context must be an object")
    if request.get("official_vlm_config") is not None and not isinstance(request["official_vlm_config"], dict):
        raise ValueError("official_vlm_config must be an object")
    if request.get("operation_assets") is not None and not isinstance(request["operation_assets"], str):
        raise ValueError("operation_assets must be a string")
    if request.get("operation") is None and request.get("operation_assets") is not None:
        raise ValueError("operation_assets requires an operation")
