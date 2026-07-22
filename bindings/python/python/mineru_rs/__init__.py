from __future__ import annotations

import os
import stat
from dataclasses import dataclass
from importlib import resources
from pathlib import Path
from typing import List

from . import _native

__all__ = ["RunReport", "canonical_stem", "run", "validate_pdf_options"]


@dataclass(frozen=True)
class RunReport:
    warnings: List[str]


def canonical_stem(value: str) -> str:
    return _native.canonical_stem(value)


def validate_pdf_options(
    start_page: int,
    end_page,
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> bool:
    return _native.validate_pdf_options(
        start_page, end_page, formula_enable, table_enable, image_analysis
    )


def _helper_path() -> Path:
    name = "mineru-office-convert.exe" if os.name == "nt" else "mineru-office-convert"
    resource = resources.files(__package__).joinpath(name)
    try:
        path = Path(os.fspath(resource))
        info = path.lstat()
    except (FileNotFoundError, OSError, TypeError) as error:
        raise RuntimeError(f"packaged Office helper is unavailable: {name}") from error
    if not path.is_absolute() or path.name != name or not stat.S_ISREG(info.st_mode):
        raise RuntimeError(f"packaged Office helper is invalid: {name}")
    if os.name != "nt" and info.st_mode & 0o111 != 0o111:
        raise RuntimeError(f"packaged Office helper is not executable: {name}")
    return path


async def run(
    path,
    output,
    *,
    api_url=None,
    method="auto",
    backend="vlm-http-client",
    effort="medium",
    lang="ch",
    url=None,
    start=0,
    end=None,
    formula=True,
    table=True,
    image_analysis=True,
    client_side_output_generation=False,
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
