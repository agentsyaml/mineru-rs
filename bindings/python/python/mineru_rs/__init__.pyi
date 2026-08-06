from dataclasses import dataclass
from os import PathLike
from pathlib import Path
from typing import List, Literal, Union

Method = Literal["auto", "txt", "ocr"]
Backend = Literal["vlm-http-client"]
Effort = Literal["medium", "high"]


def _helper_path() -> Path: ...


@dataclass(frozen=True)
class RunReport:
    warnings: List[str]


def canonical_stem(value: str) -> str: ...


def validate_pdf_options(
    start_page: int,
    end_page: Union[int, None],
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> bool: ...


async def run(
    path: Union[str, PathLike[str]],
    output: Union[str, PathLike[str]],
    *,
    api_url: Union[str, None] = None,
    method: Method = "auto",
    backend: Backend = "vlm-http-client",
    effort: Effort = "medium",
    lang: str = "ch",
    url: Union[str, None] = None,
    start: int = 0,
    end: Union[int, None] = None,
    formula: bool = True,
    table: bool = True,
    image_analysis: bool = True,
    client_side_output_generation: bool = False,
) -> RunReport: ...
