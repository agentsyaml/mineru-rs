from __future__ import annotations

import os
import stat
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import List, Literal, cast

from . import _native

__all__ = ["RunReport", "canonical_stem", "run", "validate_pdf_options"]

Method = Literal["auto", "txt", "ocr"]
Backend = Literal["vlm-http-client"]
Effort = Literal["medium", "high"]


@dataclass(frozen=True)
class RunReport:
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
    name = "mineru-office-convert.exe" if os.name == "nt" else "mineru-office-convert"
    resource = resources.files(__package__ or "mineru_rs").joinpath(name)
    try:
        path = Path(cast("os.PathLike[str]", resource))
        info = path.lstat()
    except (FileNotFoundError, OSError, TypeError) as error:
        raise RuntimeError(f"packaged Office helper is unavailable: {name}") from error
    if not path.is_absolute() or path.name != name or not stat.S_ISREG(info.st_mode):
        raise RuntimeError(f"packaged Office helper is invalid: {name}")
    if os.name != "nt" and info.st_mode & 0o111 != 0o111:
        raise RuntimeError(f"packaged Office helper is not executable: {name}")
    return path


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
