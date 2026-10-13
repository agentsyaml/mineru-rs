from typing import List, Optional, Union

from os import PathLike
from pathlib import Path

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


def locate_markdown(
    output: Union[str, PathLike[str]],
    stem: str,
) -> Optional[Path]: ...


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
    tier: Optional[str] = ...,
) -> List[str]: ...


async def _run_cli(argv: List[bytes], helper: Union[str, PathLike[str]]) -> int: ...


class _OfficialRuntime:
    def __init__(self, python: Optional[Union[str, PathLike[str]]] = ..., config: Optional[Union[str, PathLike[str]]] = ..., model_dir: Optional[Union[str, PathLike[str]]] = ..., mode: str = ..., timeout: int = ...) -> None: ...

class _OfficialParser:
    def __init__(self, runtime: _OfficialRuntime) -> None: ...
    async def parse_rich(self, path: Optional[Union[str, PathLike[str]]] = ..., data: Optional[bytes] = ..., filename: Optional[str] = ..., options: Optional[dict] = ...) -> _OfficialResult: ...
    async def parse_batch_rich(self, items: List[tuple]) -> List[tuple]: ...
    async def shutdown(self) -> None: ...

class _OfficialResult:
    middle_json: str
    model_json: Optional[str]
    markdown: str
    structured_content: str
    pages: str
    assets: List[tuple[str, bytes]]
    warnings: List[str]
    version_metadata: tuple[str, str, str]
    def export_pages(self) -> str: ...
    async def to_dict(self, skip_defaults: bool = ...) -> str: ...
    async def to_json(self, skip_defaults: bool = ...) -> str: ...
    async def render(self, format: str, options: Optional[dict] = ...) -> tuple: ...
    def save(self, root: Union[str, PathLike[str]], stem: str) -> None: ...
    def save_with_writer(self, writer: object) -> None: ...

async def _restore(runtime: _OfficialRuntime, middle_json: str, assets: Optional[List[tuple[str, bytes]]] = ...) -> _OfficialResult: ...
