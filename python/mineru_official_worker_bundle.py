"""Bounded private-file transport for rich worker operations."""

from __future__ import annotations

import json
import os
import re
from pathlib import Path
import tempfile
from typing import TYPE_CHECKING, Any, cast

if TYPE_CHECKING:
    from docvortex.assets import AssetStore

from mineru_official_worker_contract import MAX_OPERATION_BYTES, LimitError, safe_operation_path

_HTML_IMAGE_REFERENCE_RE = re.compile(r'''(?is)<img\b[^>]*?\bsrc\s*=\s*["']?([^"'>\s]+)''')


def rewrite_reference_tokens(value: str, aliases: dict[str, str]) -> str:
    replacements = sorted(((key, target) for key, target in aliases.items() if key != target),
                          key=lambda item: (-len(item[0]), item[0]))
    index, output = 0, []
    while index < len(value):
        for alias, target in replacements:
            end = index + len(alias)
            before = index == 0 or not (value[index - 1].isalnum() or value[index - 1] in "._-/\\")
            after = end >= len(value) or not (value[end].isalnum() or value[end] in "._-/\\")
            if value.startswith(alias, index) and before and after:
                output.append(target)
                index = end
                break
        else:
            output.append(value[index])
            index += 1
    return "".join(output)


def _mask_markdown_code(value: str) -> str:
    chars = list(value)
    offset, fence = 0, None
    for line in value.splitlines(keepends=True):
        body = line.rstrip("\n")
        trimmed = body.lstrip(" ")
        run = len(trimmed) - len(trimmed.lstrip("`~"))
        marker, indent = trimmed[:1], len(body) - len(trimmed)
        coded = fence is not None or (indent <= 3 and run >= 3)
        if fence is None and coded:
            fence = (marker, run)
        elif (fence is not None and indent <= 3 and run >= fence[1]
              and marker == fence[0] and trimmed[run:].strip() == ""):
            fence = None
        elif indent >= 4:
            coded = True
        if coded:
            for index in range(offset, offset + len(line)):
                if chars[index] != "\n":
                    chars[index] = " "
        offset += len(line)
    masked = "".join(chars)
    for pattern in (r"(?is)<pre\b[^>]*>.*?</pre\s*>", r"(?is)<code\b[^>]*>.*?</code\s*>"):
        for match in re.finditer(pattern, masked):
            for index in range(match.start(), match.end()):
                if chars[index] != "\n":
                    chars[index] = " "
        masked = "".join(chars)
    index = 0
    while index < len(chars):
        if chars[index] != "`":
            index += 1
            continue
        start = index
        while index < len(chars) and chars[index] == "`":
            index += 1
        width, close = index - start, index
        while close < len(chars) and chars[close] != "\n":
            if chars[close] == "`":
                end = close
                while end < len(chars) and chars[end] == "`":
                    end += 1
                if end - close == width:
                    for position in range(start, end):
                        chars[position] = " "
                    index = end
                    break
                close = end
            else:
                close += 1
    return "".join(chars)


def rewrite_markdown_image_references(value: str, aliases: dict[str, str]) -> str:
    masked = _mask_markdown_code(value)
    spans = []
    for match in re.finditer(r"!\[[^\]]*\]\(([^)\s]+)", masked):
        path = value[match.start(1):match.end(1)]
        if path in aliases:
            spans.append((match.start(1), match.end(1), aliases[path]))
    for match in _HTML_IMAGE_REFERENCE_RE.finditer(masked):
        path = value[match.start(1):match.end(1)]
        if path in aliases:
            spans.append((match.start(1), match.end(1), aliases[path]))
    for start, end, replacement in sorted(spans, key=lambda span: span[0], reverse=True):
        value = value[:start] + replacement + value[end:]
    return value


def rewrite_typed_json_references(value: str, aliases: dict[str, str], limit: int) -> str:
    if len(value.encode("utf-8")) > limit:
        raise LimitError("official text or JSON file exceeds its limit")
    try:
        parsed = json.loads(value)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return value

    def rewrite_block(block: object) -> bool:
        if not isinstance(block, dict):
            return False
        block = cast(dict[str, object], block)
        kind = block.get("type")
        changed = False
        if kind in {"equation", "image_body", "table_body", "chart_body"}:
            path = block.get("image_path")
            if isinstance(path, str) and path in aliases:
                block["image_path"] = aliases[path]
                changed = True
        if kind in {"equation", "image", "table", "chart"}:
            source = block.get("image_source")
            if isinstance(source, str) and source in aliases:
                block["image_source"] = aliases[source]
                changed = True
        if kind in {"image_body", "table_body", "chart_body", "image", "table", "chart"}:
            content = block.get("content")
            if isinstance(content, str):
                rewritten = rewrite_markdown_image_references(content, aliases)
                if rewritten != content:
                    block["content"] = rewritten
                    changed = True
        children = block.get("content")
        if isinstance(children, list):
            for child in children:
                changed = rewrite_block(child) or changed
        return changed

    pages = parsed.get("pages") if isinstance(parsed, dict) else None
    changed = False
    if isinstance(pages, list):
        for page in pages:
            blocks = page.get("blocks") if isinstance(page, dict) else None
            if isinstance(blocks, list):
                for block in blocks:
                    changed = rewrite_block(block) or changed
    return json.dumps(parsed, ensure_ascii=False, separators=(",", ":")) if changed else value


class OperationWriter:
    """Bounded private-output writer for restored and rendered artifacts."""

    fixed = {"markdown.md", "middle_json.json", "structured_content.json", "model_output.json", "operation.json", "render.json", "render.bin"}

    def __init__(self, root: Path, limit: int):
        if isinstance(limit, bool) or not isinstance(limit, int) or limit <= 0:
            raise ValueError("operation output limit must be positive")
        self.root, self.limit, self.total, self.names = root, limit, 0, set()
        self.root.mkdir(parents=True, exist_ok=True)

    def write_string(self, name: str, text: str) -> None:
        if not isinstance(text, str):
            raise TypeError("operation text output must be a string")
        self._write(name, text.encode("utf-8"))

    def write(self, name: str, value: bytes) -> None:
        if not isinstance(value, (bytes, bytearray, memoryview)):
            raise TypeError("operation binary output must be bytes")
        self._write(name, bytes(value))

    def _write(self, name: str, data: bytes) -> None:
        parts = name.split("/")
        if not parts or len(parts) > 32 or any(not part or part in (".", "..") or len(part.encode("utf-8")) > 255 or part.endswith((".", " ")) or any(ord(c) < 32 or 127 <= ord(c) <= 159 or c in '\\<>:"|?*' for c in part) for part in parts):
            raise ValueError("unsafe operation output path")
        path = safe_operation_path(self.root, name)
        if name not in self.fixed and not name.startswith("images/"):
            raise ValueError("unknown operation output path")
        if name in self.names or len(self.names) >= 8192:
            raise ValueError("duplicate or excessive operation output path")
        limit = min(self.limit, MAX_OPERATION_BYTES)
        if len(data) > limit or self.total + len(data) > self.limit:
            raise ValueError("operation output exceeds its byte limit")
        path.parent.mkdir(parents=True, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".operation-", dir=path.parent)
        try:
            with os.fdopen(fd, "wb") as output:
                output.write(data)
                output.flush()
            os.replace(temporary, path)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        self.names.add(name)
        self.total += len(data)


def read_operation(request: dict[str, object]) -> tuple[dict[str, Any], AssetStore]:
    from docvortex.assets import AssetStore, validate_image_sidecar_path

    source = Path(str(request["input_path"]))
    if source.is_symlink() or not source.is_file() or source.stat().st_size > MAX_OPERATION_BYTES:
        raise ValueError("operation input is missing, unsafe, or oversized")
    payload = json.loads(source.read_text(encoding="utf-8"))
    if not isinstance(payload, dict):
        raise ValueError("operation input must be an object")
    names = payload.get("assets", [])
    if not isinstance(names, list) or len(names) > 8192 or any(not isinstance(name, str) for name in names):
        raise ValueError("operation asset manifest is invalid")
    root_value = request.get("operation_assets")
    assets = AssetStore()
    total = 0
    if names and not isinstance(root_value, str):
        raise ValueError("operation asset directory is unavailable")
    root = Path(root_value) if isinstance(root_value, str) else source.parent
    for raw_name in names:
        name = validate_image_sidecar_path(raw_name)
        path = safe_operation_path(root, name)
        info = path.lstat()
        if path.is_symlink() or not path.is_file():
            raise ValueError("operation asset is not a regular file")
        total += info.st_size
        if total > MAX_OPERATION_BYTES:
            raise ValueError("operation assets exceed their byte limit")
        assets.add(name, path.read_bytes())
    return payload, assets
