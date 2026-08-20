"""Project-owned adapter for the pinned MinerU 4.0.0a6 parser.

This is not an official MinerU worker protocol.  Rust supplies one JSON request
on stdin and owns the process lifetime; this shim only imports MinerU, invokes
parse_async, and writes the official result files.
"""

import asyncio
import contextlib
import importlib.metadata
import json
import os
from pathlib import Path, PurePosixPath
import sys
from typing import Iterable, Union


# The protocol source is prepended by the later Rust embedding lane.  In a
# normal file invocation, import the same exports from the sibling module.
if "WORKER_SOURCE_CONTRACT" not in globals():
    from mineru_official_worker_protocol import (
        BUNDLE_NAME,
        DIAGNOSTIC_CAP,
        PACKAGE_VERSION,
        PROTOCOL,
        PROTOCOL_CAP,
        BoundedCapture,
        LimitError,
        WORKER_SOURCE_CONTRACT,
        _bounded_text,
        _emit,
        _persistent_main,
        _response,
    )


class BundleWriter:
    _FIXED_FILES = frozenset(
        {
            "markdown.md",
            "middle_json.json",
            "content_list.json",
            "structured_content.json",
            "model_output.json",
        }
    )

    def __init__(self, root: Path, limit: int) -> None:
        if limit <= 0:
            raise LimitError("bundle byte limit must be positive")
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)
        self.limit = limit
        self.total = 0
        self._sizes: dict[str, int] = {}
        self._text_paths: set[str] = set()
        self._image_aliases: dict[str, str] = {}

    def _name(self, name: object) -> str:
        if not isinstance(name, (str, bytes, os.PathLike)):
            raise ValueError("unsafe bundle path")
        try:
            raw = os.fspath(name)
        except TypeError as error:
            raise ValueError("unsafe bundle path") from error
        if isinstance(raw, bytes):
            raw = raw.decode("utf-8")
        if not isinstance(raw, str) or "\\" in raw:
            raise ValueError("unsafe bundle path")
        if len(raw) >= 2 and raw[0].isalpha() and raw[1] == ":":
            raise ValueError("unsafe bundle path")
        relative = PurePosixPath(raw)
        if relative.is_absolute() or not relative.parts:
            raise ValueError("bundle path must be relative")
        if any(
            part in ("", ".", "..")
            or part.endswith((".", " "))
            or any(ord(character) < 32 or 127 <= ord(character) <= 159 for character in part)
            or _windows_device_name(part)
            for part in relative.parts
        ):
            raise ValueError("unsafe bundle path")
        if relative.parts[0] == "images":
            if len(relative.parts) < 2:
                raise ValueError("images must contain a file")
            return "/".join(relative.parts)
        if raw in self._FIXED_FILES:
            return raw
        if len(relative.parts) != 1 or raw != relative.parts[0]:
            raise ValueError("unknown official bundle path")
        return f"images/{relative.parts[0]}"

    def _path(self, name: object) -> Path:
        relative = self._name(name)
        path = self.root.joinpath(*relative.split("/"))
        path.parent.mkdir(parents=True, exist_ok=True)
        return path

    def _write_raw(self, relative: str, chunks: Iterable[Union[bytes, str]]) -> None:
        path = self.root.joinpath(*relative.split("/"))
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_name(path.name + ".tmp")
        old_size = self._sizes.get(relative, 0)
        size = 0
        try:
            with temporary.open("wb") as output:
                for chunk in chunks:  # type: ignore[union-attr]
                    if isinstance(chunk, str):
                        chunk = chunk.encode("utf-8")
                    if not isinstance(chunk, (bytes, bytearray, memoryview)):
                        raise TypeError("bundle writer received non-bytes")
                    chunk = bytes(chunk)
                    next_size = size + len(chunk)
                    if self.total - old_size + next_size > self.limit:
                        raise LimitError("official bundle exceeds configured byte limit")
                    output.write(chunk)
                    size = next_size
            os.replace(temporary, path)
            self.total = self.total - old_size + size
            self._sizes[relative] = size
        finally:
            if temporary.exists():
                temporary.unlink()

    def _write(self, name: object, chunks: Iterable[Union[bytes, str]]) -> None:
        relative = self._name(name)
        self._write_raw(relative, chunks)
        if relative in self._FIXED_FILES:
            self._text_paths.add(relative)
            self._rewrite_text((relative,))
        else:
            self._record_image(relative)
            self._rewrite_text(self._text_paths)

    def _record_image(self, relative: str) -> None:
        image_name = relative.removeprefix("images/")
        self._image_aliases[relative] = relative
        if "/" not in image_name:
            self._image_aliases[image_name] = relative

    def _rewrite_text(self, relatives: Iterable[str]) -> None:
        for relative in relatives:
            path = self.root.joinpath(*relative.split("/"))
            try:
                original = path.read_bytes()
            except OSError:
                continue
            rewritten = self._rewrite_text_bytes(relative, original)
            if rewritten != original:
                self._write_raw(relative, (rewritten,))

    def _rewrite_text_bytes(self, relative: str, value: bytes) -> bytes:
        try:
            text = value.decode("utf-8")
        except UnicodeDecodeError:
            return value
        if relative == "markdown.md":
            rewritten = _rewrite_reference_tokens(text, self._image_aliases)
        else:
            rewritten = _rewrite_json_references(text, self._image_aliases)
        return rewritten.encode("utf-8")

    def _arguments(self, first: object, second: object) -> tuple[object, object]:
        try:
            self._path(first)
            return first, second
        except (TypeError, ValueError):
            self._path(second)
            return second, first

    def write(self, first: object, second: object) -> None:
        name, value = self._arguments(first, second)
        if isinstance(value, str):
            self.write_string(name, value)
        elif isinstance(value, (bytes, bytearray, memoryview)):
            self._write(name, (bytes(value),))
        elif hasattr(value, "read"):
            self._write(name, _read_chunks(value))
        else:
            raise TypeError("official ParseResult.save supplied unsupported bytes")

    def write_string(self, first: object, second: str) -> None:
        name, value = self._arguments(first, second)
        if not isinstance(value, str):
            raise TypeError("official ParseResult.save supplied non-text")
        self._write(
            name,
            (value[index : index + 64 * 1024].encode("utf-8") for index in range(0, len(value), 64 * 1024)),
        )


def _reference_boundary(value: str, index: int) -> bool:
    if index < 0 or index >= len(value):
        return True
    character = value[index]
    return not (character.isalnum() or character in "._-/\\")


def _rewrite_reference_tokens(value: str, aliases: dict[str, str]) -> str:
    replacements = sorted(
        ((alias, target) for alias, target in aliases.items() if alias != target),
        key=lambda item: (-len(item[0]), item[0]),
    )
    if not replacements:
        return value
    output: list[str] = []
    index = 0
    while index < len(value):
        for alias, target in replacements:
            end = index + len(alias)
            if (
                value.startswith(alias, index)
                and _reference_boundary(value, index - 1)
                and _reference_boundary(value, end)
            ):
                output.append(target)
                index = end
                break
        else:
            output.append(value[index])
            index += 1
    return "".join(output)


def _rewrite_json_references(value: str, aliases: dict[str, str]) -> str:
    try:
        parsed = json.loads(value)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return value

    def rewrite(item: object) -> tuple[object, bool]:
        if isinstance(item, dict):
            changed = False
            rewritten: dict[object, object] = {}
            for key, child in item.items():
                new_child, child_changed = rewrite(child)
                rewritten[key] = new_child
                changed = changed or child_changed
            return rewritten, changed
        if isinstance(item, list):
            changed = False
            rewritten = []
            for child in item:
                new_child, child_changed = rewrite(child)
                rewritten.append(new_child)
                changed = changed or child_changed
            return rewritten, changed
        if isinstance(item, str):
            replacement = aliases.get(item)
            if replacement is not None and replacement != item:
                return replacement, True
        return item, False

    rewritten, changed = rewrite(parsed)
    if not changed:
        return value
    return json.dumps(rewritten, ensure_ascii=False, separators=(",", ":"))


def _windows_device_name(name: str) -> bool:
    base = name.split(".", 1)[0].lower()
    return base in {"con", "prn", "aux", "nul"} or (
        len(base) == 4 and base[:3] in {"com", "lpt"} and base[3] in "123456789"
    )


def _read_chunks(stream: object) -> Iterable[Union[bytes, str]]:
    read = getattr(stream, "read")
    while True:
        chunk = read(64 * 1024)
        if chunk == b"" or chunk == "":
            return
        yield chunk


def main() -> None:
    raw = sys.stdin.buffer.read(PROTOCOL_CAP + 1)
    if len(raw) > PROTOCOL_CAP:
        _emit(_response({}, "error", "", "request exceeds protocol limit"))
        return
    capture = BoundedCapture(DIAGNOSTIC_CAP)
    try:
        request = json.loads(raw.decode("utf-8"))
        if not isinstance(request, dict) or request.get("protocol") != PROTOCOL:
            raise ValueError("unsupported adapter protocol")
        if request.get("bundle_name") != BUNDLE_NAME:
            raise ValueError("unsupported bundle name")
        package = importlib.metadata.version("mineru")
        if package != PACKAGE_VERSION:
            _emit(_response(request, "error", package, "MinerU package version is not 4.0.0a6"))
            return
        with contextlib.redirect_stdout(capture):
            for key, request_key in (
                ("MINERU_MODEL_STACK", "model_stack"),
                ("MINERU_MODEL_BASE_DIR", "model_base_dir"),
                ("MINERU_CONFIG", "config"),
                ("MINERU_VL_API_KEY", "vl_api_key"),
                ("MINERU_VL_MODEL_NAME", "vl_model_name"),
            ):
                value = request.get(request_key)
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = str(value)
            importlib.import_module("mineru")
            parser = importlib.import_module("mineru.parser")

            page_range = request.get("page_range")
            if page_range is not None and not isinstance(page_range, str):
                raise ValueError("invalid page range")
            kwargs = {
                "backend": request["backend"],
                "effort": request["effort"],
                "server_url": request.get("server_url"),
                "method": request["method"],
                "lang": request["lang"],
                "image_analysis": bool(request["image_analysis"]),
            }
            if page_range:
                kwargs["page_range"] = page_range
            result = asyncio.run(parser.parse_async(request["input_path"], **kwargs))
            result.save(BundleWriter(Path(request["bundle_path"]), int(request["max_bundle_bytes"])))
        _emit(_response(request, "ok", package), capture.getvalue())
    except Exception as error:  # The parent owns the bounded failure surface.
        request = request if "request" in locals() and isinstance(request, dict) else {}
        _emit(
            _response(request, "error", locals().get("package", ""), str(error)),
            capture.getvalue(),
        )


if __name__ == "__main__":
    if "--persistent" in sys.argv[1:]:
        raise SystemExit(_persistent_main(BundleWriter))
    main()
