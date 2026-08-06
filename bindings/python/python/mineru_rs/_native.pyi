from typing import List, Optional, Union

from os import PathLike

# Native extension module `mineru_rs._native` (PyO3). Private surface used by
# `mineru_rs.run` / `mineru_rs._cli`; not part of the public API contract.


def canonical_stem(value: str) -> str: ...


def validate_pdf_options(
    start_page: int,
    end_page: Optional[int],
    formula_enable: bool,
    table_enable: bool,
    image_analysis: bool,
) -> bool: ...


async def _run(
    path: Union[str, PathLike[str]],
    output: Union[str, PathLike[str]],
    api_url: Optional[str],
    method: str,
    backend: str,
    effort: str,
    lang: str,
    url: Optional[str],
    start: int,
    end: Optional[int],
    formula: bool,
    table: bool,
    image_analysis: bool,
    client_side_output_generation: bool,
    helper: Union[str, PathLike[str]],
) -> List[str]: ...


async def _run_cli(argv: List[bytes], helper: Union[str, PathLike[str]]) -> int: ...
