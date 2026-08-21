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
from pathlib import Path
import sys
import tempfile
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


MAX_BUNDLE_ENTRIES = 8_192
MAX_BUNDLE_PATH_DEPTH = 32
MAX_BUNDLE_COMPONENT_BYTES = 255
MAX_BUNDLE_PATH_BYTES = 4_096
MAX_BUNDLE_NAME_BUDGET = 32 * 1024 * 1024
MAX_TEXT_JSON_BYTES = 64 * 1024 * 1024


def _read_bounded(path: Path, limit: int) -> bytes:
    if path.stat().st_size > limit:
        raise LimitError("official text or JSON file exceeds its limit")
    chunks: list[bytes] = []
    total = 0
    with path.open("rb") as source:
        while True:
            chunk = source.read(min(64 * 1024, limit - total + 1))
            if not chunk:
                return b"".join(chunks)
            total += len(chunk)
            if total > limit:
                raise LimitError("official text or JSON file exceeds its limit")
            chunks.append(chunk)


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
        if isinstance(limit, bool) or not isinstance(limit, int) or limit <= 0:
            raise LimitError("bundle byte limit must be positive")
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)
        self.limit = limit
        self._text_json_limit = min(limit, MAX_TEXT_JSON_BYTES)
        self.total = 0
        self._sizes: dict[str, int] = {}
        self._text_paths: set[str] = set()
        self._image_aliases: dict[str, str] = {}
        self._image_alias_keys: dict[str, str] = {}
        self._directories: set[str] = set()
        self._files: set[str] = set()
        self._directory_names: dict[str, str] = {}
        self._file_names: dict[str, str] = {}
        self._name_bytes = 0
        self._alias_bytes = 0

    def _name(self, name: object) -> str:
        if not isinstance(name, (str, bytes, os.PathLike)):
            raise ValueError("unsafe bundle path")
        try:
            raw = os.fspath(name)
        except TypeError as error:
            raise ValueError("unsafe bundle path") from error
        if isinstance(raw, bytes):
            if len(raw) > MAX_BUNDLE_PATH_BYTES:
                raise LimitError("bundle path exceeds its byte limit")
            try:
                raw = raw.decode("utf-8")
            except UnicodeDecodeError as error:
                raise ValueError("unsafe bundle path") from error
        if not isinstance(raw, str) or "\\" in raw:
            raise ValueError("unsafe bundle path")
        if not raw:
            raise ValueError("bundle path must be relative")
        try:
            raw_bytes = raw.encode("utf-8")
        except UnicodeEncodeError as error:
            raise ValueError("unsafe bundle path") from error
        if len(raw_bytes) > MAX_BUNDLE_PATH_BYTES:
            raise LimitError("bundle path exceeds its byte limit")
        if len(raw) >= 2 and raw[0].isalpha() and raw[1] == ":":
            raise ValueError("unsafe bundle path")
        parts = raw.split("/")
        if len(parts) > MAX_BUNDLE_PATH_DEPTH:
            raise LimitError("bundle path is too deep")
        if not parts or parts[0] == "":
            raise ValueError("bundle path must be relative")
        if any(not _portable_name(part) for part in parts):
            raise ValueError("unsafe bundle path")
        if parts[0] == "images":
            if len(parts) < 2:
                raise ValueError("images must contain a file")
            relative_parts = parts
        elif raw in self._FIXED_FILES:
            relative_parts = parts
        elif len(parts) == 1:
            relative_parts = ["images", parts[0]]
        else:
            raise ValueError("unknown official bundle path")
        if len(relative_parts) > MAX_BUNDLE_PATH_DEPTH:
            raise LimitError("bundle path is too deep")
        component_bytes = []
        for part in relative_parts:
            try:
                size = len(part.encode("utf-8"))
            except UnicodeEncodeError as error:
                raise ValueError("unsafe bundle path") from error
            if size > MAX_BUNDLE_COMPONENT_BYTES:
                raise LimitError("bundle path component exceeds its byte limit")
            component_bytes.append(size)
        path_bytes = sum(component_bytes) + len(relative_parts) - 1
        if path_bytes > MAX_BUNDLE_PATH_BYTES:
            raise LimitError("bundle path exceeds its byte limit")
        return "/".join(relative_parts)

    def _path(self, name: object) -> Path:
        relative = self._name(name)
        return self.root.joinpath(*relative.split("/"))

    def _reserve_path(self, relative: str) -> tuple[tuple[str, ...], bool]:
        parts = relative.split("/")
        parents = tuple("/".join(parts[:index]) for index in range(1, len(parts)))
        for parent in parents:
            key = _lowercase_key(parent)
            if key in self._files:
                raise ValueError("bundle path conflicts with a file")
            if key in self._directories and self._directory_names.get(key, parent) != parent:
                raise ValueError("bundle path collides with a directory")
        relative_key = _lowercase_key(relative)
        if relative_key in self._directories:
            raise ValueError("bundle path conflicts with a directory")
        if (
            relative_key in self._files
            and self._file_names.get(relative_key, relative) != relative
        ):
            raise ValueError("bundle path collides with a file")
        new_directories = tuple(
            parent for parent in parents if _lowercase_key(parent) not in self._directories
        )
        new_file = relative_key not in self._files
        additions = len(new_directories) + int(new_file)
        if len(self._directories) + len(self._files) + additions > MAX_BUNDLE_ENTRIES:
            raise LimitError("official bundle has too many files or directories")
        added_name_bytes = sum(len(name.encode("utf-8")) for name in new_directories)
        if new_file:
            added_name_bytes += len(relative.encode("utf-8"))
        if self._name_bytes + added_name_bytes > MAX_BUNDLE_NAME_BUDGET:
            raise LimitError("official bundle relative-name budget exceeded")
        for directory in new_directories:
            key = _lowercase_key(directory)
            self._directories.add(key)
            self._directory_names[key] = directory
        if new_file:
            self._files.add(relative_key)
            self._file_names[relative_key] = relative
        self._name_bytes += added_name_bytes
        return new_directories, new_file

    def _release_path(self, reservation: tuple[tuple[str, ...], bool], relative: str) -> None:
        new_directories, new_file = reservation
        for directory in new_directories:
            key = _lowercase_key(directory)
            self._directories.remove(key)
            self._directory_names.pop(key, None)
            self._name_bytes -= len(directory.encode("utf-8"))
        if new_file:
            key = _lowercase_key(relative)
            self._files.remove(key)
            self._file_names.pop(key, None)
            self._name_bytes -= len(relative.encode("utf-8"))

    def _image_alias(self, relative: str) -> tuple[str, str] | None:
        image_name = relative.removeprefix("images/")
        if "/" not in image_name:
            return image_name, relative
        return None

    def _check_alias(self, alias: tuple[str, str] | None) -> None:
        if alias is None:
            return
        key = _lowercase_key(alias[0])
        if key in self._image_alias_keys:
            if self._image_alias_keys[key] != alias[0]:
                raise ValueError("bundle image alias collision")
            return
        if max(len(self._image_alias_keys), len(self._image_aliases)) >= MAX_BUNDLE_ENTRIES:
            raise LimitError("official bundle has too many image aliases")
        size = len(alias[0].encode("utf-8")) + len(alias[1].encode("utf-8"))
        if self._alias_bytes + size > MAX_BUNDLE_NAME_BUDGET:
            raise LimitError("official bundle image-alias budget exceeded")

    def _write_raw(self, relative: str, chunks: Iterable[Union[bytes, str]]) -> None:
        path = self.root.joinpath(*relative.split("/"))
        path.parent.mkdir(parents=True, exist_ok=True)
        old_size = self._sizes.get(relative, 0)
        if relative not in self._sizes and len(self._sizes) >= MAX_BUNDLE_ENTRIES:
            raise LimitError("official bundle has too many files")
        if old_size < 0 or old_size > self.total:
            raise LimitError("official bundle accounting is inconsistent")
        base_total = self.total - old_size
        if base_total > self.limit:
            raise LimitError("official bundle accounting exceeds its byte limit")
        file_limit = self._text_json_limit if relative in self._FIXED_FILES else None
        descriptor = None
        temporary: Path | None = None
        size = 0
        try:
            descriptor, temporary_name = tempfile.mkstemp(
                prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent)
            )
            temporary = Path(temporary_name)
            with os.fdopen(descriptor, "wb") as output:
                descriptor = None
                for chunk in chunks:  # type: ignore[union-attr]
                    if isinstance(chunk, str):
                        chunk = chunk.encode("utf-8")
                    if not isinstance(chunk, (bytes, bytearray, memoryview)):
                        raise TypeError("bundle writer received non-bytes")
                    chunk = bytes(chunk)
                    next_size = size + len(chunk)
                    if base_total + next_size > self.limit:
                        raise LimitError("official bundle exceeds configured byte limit")
                    if file_limit is not None and next_size > file_limit:
                        raise LimitError("official text or JSON file exceeds its limit")
                    output.write(chunk)
                    size = next_size
            os.replace(temporary, path)
            self.total = base_total + size
            self._sizes[relative] = size
        finally:
            if descriptor is not None:
                os.close(descriptor)
            if temporary is not None:
                try:
                    temporary.unlink()
                except FileNotFoundError:
                    pass

    def _write(self, name: object, chunks: Iterable[Union[bytes, str]]) -> None:
        relative = self._name(name)
        alias = self._image_alias(relative)
        self._check_alias(alias)
        reservation = self._reserve_path(relative)
        try:
            self._write_raw(relative, chunks)
        except Exception:
            self._release_path(reservation, relative)
            raise
        if relative in self._FIXED_FILES:
            self._text_paths.add(relative)
            self._rewrite_text((relative,))
        else:
            self._record_image(relative)
            self._rewrite_text(self._text_paths)

    def _record_image(self, relative: str) -> None:
        alias = self._image_alias(relative)
        if alias is None:
            return
        key = _lowercase_key(alias[0])
        if key in self._image_alias_keys:
            return
        self._image_aliases[alias[0]] = alias[1]
        self._image_alias_keys[key] = alias[0]
        self._alias_bytes += len(alias[0].encode("utf-8")) + len(alias[1].encode("utf-8"))

    def _rewrite_text(self, relatives: Iterable[str]) -> None:
        for relative in relatives:
            path = self.root.joinpath(*relative.split("/"))
            try:
                original = _read_bounded(path, self._text_json_limit)
            except OSError:
                continue
            rewritten = self._rewrite_text_bytes(relative, original)
            if rewritten != original:
                self._write_raw(relative, (rewritten,))

    def _rewrite_text_bytes(self, relative: str, value: bytes) -> bytes:
        if len(value) > self._text_json_limit:
            raise LimitError("official text or JSON file exceeds its limit")
        try:
            text = value.decode("utf-8")
        except UnicodeDecodeError:
            return value
        if relative == "markdown.md":
            rewritten = _rewrite_reference_tokens(text, self._image_aliases)
        else:
            rewritten = _rewrite_json_references(text, self._image_aliases, self._text_json_limit)
        encoded = rewritten.encode("utf-8")
        if len(encoded) > self._text_json_limit:
            raise LimitError("official text or JSON file exceeds its limit")
        return encoded

    def _arguments(self, first: object, second: object) -> tuple[object, object]:
        try:
            self._name(first)
            return first, second
        except (TypeError, ValueError):
            self._name(second)
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


def _rewrite_json_references(
    value: str, aliases: dict[str, str], limit: int = MAX_TEXT_JSON_BYTES
) -> str:
    if len(value.encode("utf-8")) > limit:
        raise LimitError("official text or JSON file exceeds its limit")
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
    base = name.split(".", 1)[0]
    try:
        encoded = base.encode("utf-8")
    except UnicodeEncodeError:
        return False
    return (
        base.isascii() and base.lower() in {"con", "prn", "aux", "nul"}
    ) or (
        len(encoded) == 4
        and encoded[:3].lower() in {b"com", b"lpt"}
        and encoded[3] in b"123456789"
    )


def _lowercase_key(value: str) -> str:
    return value.lower()


def _portable_name(name: str) -> bool:
    return (
        bool(name)
        and name not in {".", ".."}
        and not name.endswith((".", " "))
        and all(
            not (ord(character) < 32 or 127 <= ord(character) <= 159)
            and character not in "/\\<>:\"|?*"
            for character in name
        )
        and not _windows_device_name(name)
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
        with contextlib.redirect_stdout(capture), contextlib.redirect_stderr(capture):
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
