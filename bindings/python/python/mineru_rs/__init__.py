from __future__ import annotations

import os
import tempfile
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import List, Literal, cast

from . import _native

__all__ = [
    "ParseResult",
    "RunReport",
    "canonical_stem",
    "parse",
    "run",
    "validate_pdf_options",
]

Method = Literal["auto", "txt", "ocr"]
Backend = Literal["vlm-http-client"]
Effort = Literal["medium", "high"]


@dataclass(frozen=True)
class RunReport:
    warnings: List[str]


@dataclass(frozen=True)
class ParseResult:
    markdown: str
    warnings: List[str]


def canonical_stem(value: str) -> str:
    return _native.canonical_stem(value)


def validate_pdf_options(
    start_page: int,
    end_page: int | None,
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> bool:
    return _native.validate_pdf_options(
        start_page, end_page, formula_enable, table_enable, image_analysis
    )


def _helper_path() -> Path:
    # Office conversion is not bundled in the Python package; the core maps
    # spawn failure to an "office conversion is unavailable" error when the
    # helper path does not exist.
    name = "mineru-office-convert.exe" if os.name == "nt" else "mineru-office-convert"
    resource = resources.files(__package__ or "mineru_rs").joinpath(name)
    return Path(cast("os.PathLike[str]", resource))


async def run(
    path: str | os.PathLike[str],
    output: str | os.PathLike[str],
    *,
    api_url: str | None = None,
    method: Method = "auto",
    backend: Backend = "vlm-http-client",
    effort: Effort = "medium",
    lang: str = "ch",
    url: str | None = None,
    start: int = 0,
    end: int | None = None,
    formula: bool = True,
    table: bool = True,
    image_analysis: bool = True,
    client_side_output_generation: bool = False,
) -> RunReport:
    warnings = await _native._run(
        path,
        output,
        api_url,
        method,
        backend,
        effort,
        lang,
        url,
        start,
        end,
        formula,
        table,
        image_analysis,
        client_side_output_generation,
        _helper_path(),
    )
    return RunReport(list(warnings))


def _read_markdown(root: Path, stem: str) -> str:
    found: list[Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        if len(Path(dirpath).relative_to(root).parts) >= 2:
            # max depth 2: look at files here but do not descend further
            dirnames[:] = []
        for name in filenames:
            if name.endswith(".md"):
                found.append(Path(dirpath) / name)
    if not found:
        raise RuntimeError("parse: no markdown output produced")
    expected = f"{stem}.md"
    for candidate in found:
        if candidate.name == expected:
            return candidate.read_text(encoding="utf-8")
    return max(found, key=lambda candidate: candidate.stat().st_size).read_text(
        encoding="utf-8"
    )


async def parse(
    path: str | os.PathLike[str],
    *,
    api_url: str | None = None,
    method: Method = "auto",
    backend: Backend = "vlm-http-client",
    effort: Effort = "medium",
    lang: str = "ch",
    url: str | None = None,
    start: int = 0,
    end: int | None = None,
    formula: bool = True,
    table: bool = True,
    image_analysis: bool = True,
    client_side_output_generation: bool = False,
) -> ParseResult:
    with tempfile.TemporaryDirectory() as tmp:
        warnings = await _native._run(
            path,
            tmp,
            api_url,
            method,
            backend,
            effort,
            lang,
            url,
            start,
            end,
            formula,
            table,
            image_analysis,
            client_side_output_generation,
            _helper_path(),
        )
        # The CLI writes `{file_stem}/vlm/{file_stem}.md`; derive the stem the same way
        # (strip the extension from the basename) so the exact-match branch is the live path.
        stem = canonical_stem(os.path.splitext(os.path.basename(os.fspath(path)))[0])
        markdown = _read_markdown(Path(tmp), stem)
    return ParseResult(markdown, list(warnings))
