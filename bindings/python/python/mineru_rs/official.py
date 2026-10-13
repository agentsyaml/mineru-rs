"""Typed wrappers for the additive official MinerU SDK."""
from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from enum import Enum
from pathlib import Path
from typing import Any, Dict, List, Mapping, Optional, Sequence, Union

from . import _native

PathArg = Union[str, os.PathLike]


class OfficialTier(str, Enum):
    FLASH = "flash"
    BASIC = "basic"
    STANDARD = "standard"
    ADVANCED = "advanced"


class OfficialOcrMode(str, Enum):
    AUTO = "auto"
    TXT = "txt"
    OCR = "ocr"


class OfficialWorkerMode(str, Enum):
    PER_DOCUMENT = "per-document"
    PERSISTENT = "persistent"


class OfficialRenderFormat(str, Enum):
    MARKDOWN = "markdown"
    HTML = "html"
    LATEX = "latex"
    DOCX = "docx"
    EPUB = "epub"
    STRUCTURED_CONTENT = "structured_content"
    CONTENT_LIST = "content_list"
    CONTENT_LIST_V2 = "content_list_v2"
    PDF = "pdf"


class OfficialRenderMode(str, Enum):
    DEFAULT = "default"
    FULL = "full"


class OfficialPdfLayout(str, Enum):
    AUTO = "auto"
    ORIGINAL = "original"
    REFLOW = "reflow"


@dataclass(frozen=True, repr=False)
class OfficialRuntime:
    python: Optional[PathArg] = None
    config: Optional[PathArg] = None
    model_dir: Optional[PathArg] = None
    worker_mode: OfficialWorkerMode = OfficialWorkerMode.PER_DOCUMENT
    timeout: int = 86400

    def _native_runtime(self):
        return _native._OfficialRuntime(
            None if self.python is None else os.fspath(self.python),
            None if self.config is None else os.fspath(self.config),
            None if self.model_dir is None else os.fspath(self.model_dir),
            OfficialWorkerMode(self.worker_mode).value,
            self.timeout,
        )


@dataclass(frozen=True)
class HtmlSourceContext:
    source_uri: Optional[str] = field(default=None, repr=False)
    local_resource_root: Optional[PathArg] = None
    transport_encoding: Optional[str] = None

    def _dict(self) -> Dict[str, Any]:
        return {k: (os.fspath(v) if k == "local_resource_root" and v is not None else v)
                for k, v in vars(self).items() if v is not None}


@dataclass(frozen=True, repr=False)
class OfficialVlmConfig:
    engine: str = "auto"
    server_url: str = field(default="", repr=False)
    api_key: str = field(default="", repr=False)
    model: str = ""
    http_timeout: int = 600
    max_concurrency: int = 100

    def _dict(self) -> Dict[str, Any]:
        return {"engine": self.engine, "server_url": self.server_url, "api_key": self.api_key,
                "model": self.model, "http_timeout": self.http_timeout,
                "max_concurrency": self.max_concurrency}


@dataclass(frozen=True)
class OfficialParseOptions:
    tier: Optional[OfficialTier] = None
    ocr_mode: Optional[OfficialOcrMode] = None
    image_analysis: Optional[bool] = None
    page_range: Optional[str] = None
    source_context: Optional[HtmlSourceContext] = None
    vlm_config: Optional[OfficialVlmConfig] = None
    vlm_server_url: Optional[str] = field(default=None, repr=False)
    vlm_api_key: Optional[str] = field(default=None, repr=False)
    vlm_model: Optional[str] = None

    def __post_init__(self) -> None:
        if self.tier is not None:
            OfficialTier(self.tier)
        if self.ocr_mode is not None:
            OfficialOcrMode(self.ocr_mode)
        if self.image_analysis is not None and not isinstance(self.image_analysis, bool):
            raise TypeError("image_analysis must be bool or None")

    def _dict(self) -> Dict[str, Any]:
        result: Dict[str, Any] = {}
        for name in ("tier", "ocr_mode", "image_analysis", "page_range", "vlm_server_url", "vlm_api_key", "vlm_model"):
            value = getattr(self, name)
            if value is not None:
                result[name] = value.value if isinstance(value, Enum) else value
        if self.source_context is not None:
            result["source_context"] = self.source_context._dict()
        if self.vlm_config is not None:
            result["vlm_config"] = self.vlm_config._dict()
        return result


@dataclass(frozen=True)
class OfficialLatexDelimiter:
    left: str = "$"
    right: str = "$"


@dataclass(frozen=True)
class OfficialLatexDelimiters:
    display: OfficialLatexDelimiter = field(default_factory=lambda: OfficialLatexDelimiter("$$", "$$"))
    inline: OfficialLatexDelimiter = field(default_factory=OfficialLatexDelimiter)


@dataclass(frozen=True)
class OfficialRenderOptions:
    mode: Optional[OfficialRenderMode] = None
    asset_base_url: Optional[str] = None
    standalone: Optional[bool] = None
    document_title: Optional[str] = None
    asset_base_path: Optional[str] = None
    title: Optional[str] = None
    authors: Optional[List[str]] = None
    language: Optional[str] = None
    identifier: Optional[str] = None
    modified_at: Optional[str] = None
    layout: Optional[OfficialPdfLayout] = None
    latex_delimiters: Optional[OfficialLatexDelimiters] = None

    def __post_init__(self) -> None:
        if self.mode is not None:
            OfficialRenderMode(self.mode)
        if self.layout is not None:
            OfficialPdfLayout(self.layout)

    def _dict(self) -> Dict[str, Any]:
        result: Dict[str, Any] = {}
        for name, value in vars(self).items():
            if value is None:
                continue
            if isinstance(value, Enum):
                value = value.value
            elif name == "latex_delimiters":
                value = {key: vars(item) for key, item in vars(value).items()}
            result[name] = value
        return result


@dataclass(frozen=True)
class OfficialVersionMetadata:
    mineru_package: str
    schema_version: str
    bundle_name: str


@dataclass(frozen=True)
class OfficialText:
    content: str
    sidecars: Dict[str, bytes]
    kind: str = "text"


@dataclass(frozen=True)
class OfficialJson:
    value: Any
    sidecars: Dict[str, bytes]
    kind: str = "json"


@dataclass(frozen=True)
class OfficialBinary:
    data: bytes
    sidecars: Dict[str, bytes]
    kind: str = "binary"


OfficialRendered = Union[OfficialText, OfficialJson, OfficialBinary]


@dataclass(frozen=True)
class OfficialBatchItem:
    input: Union[PathArg, bytes]
    filename: Optional[str] = None
    options: OfficialParseOptions = field(default_factory=OfficialParseOptions)


@dataclass(frozen=True)
class OfficialBatchResult:
    index: int
    filename: str
    result: Optional["OfficialResult"] = None
    error: Optional[str] = None


class OfficialResult:
    def __init__(self, native: Any, runtime: OfficialRuntime):
        self._native = native
        self._runtime = runtime

    @property
    def middle_json(self) -> Any:
        return json.loads(self._native.middle_json)

    @property
    def model_json(self) -> Optional[Any]:
        value = self._native.model_json
        return None if value is None else json.loads(value)

    @property
    def markdown(self) -> str:
        return self._native.markdown

    @property
    def structured_content(self) -> Any:
        return json.loads(self._native.structured_content)

    @property
    def pages(self) -> List[Any]:
        return json.loads(self._native.pages)

    @property
    def assets(self) -> Dict[str, bytes]:
        return {name: bytes(data) for name, data in self._native.assets}

    @property
    def warnings(self) -> List[str]:
        return list(self._native.warnings)

    @property
    def version_metadata(self) -> OfficialVersionMetadata:
        return OfficialVersionMetadata(*self._native.version_metadata)

    def export_pages(self) -> List[Any]:
        return json.loads(self._native.export_pages())

    async def to_dict(self, skip_defaults: bool = False) -> Any:
        return json.loads(await self._native.to_dict(skip_defaults))

    async def to_json(self, skip_defaults: bool = False) -> str:
        return await self._native.to_json(skip_defaults)

    @classmethod
    async def from_dict(cls, runtime: OfficialRuntime, middle: Any, assets: Optional[Mapping[str, bytes]] = None) -> "OfficialResult":
        return await cls.from_json(runtime, json.dumps(middle), assets)

    @classmethod
    async def from_json(cls, runtime: OfficialRuntime, middle: str, assets: Optional[Mapping[str, bytes]] = None) -> "OfficialResult":
        native = await _native._restore(runtime._native_runtime(), middle,
                                        [(name, bytes(data)) for name, data in (assets or {}).items()])
        return cls(native, runtime)

    async def render(self, format: Union[OfficialRenderFormat, str], options: Optional[OfficialRenderOptions] = None) -> OfficialRendered:
        target = OfficialRenderFormat(format).value
        kind, text, binary, sidecars = await self._native.render(target, None if options is None else options._dict())
        files = {name: bytes(data) for name, data in sidecars}
        if kind == "text":
            return OfficialText(text or "", files)
        if kind == "json":
            return OfficialJson(json.loads(text or "null"), files)
        return OfficialBinary(bytes(binary or b""), files)

    def save(self, root: PathArg, stem: str) -> None:
        self._native.save(os.fspath(root), stem)

    def save_with_writer(self, writer: Any) -> None:
        """Run local writer callbacks after core validation; callback effects are not rolled back."""
        self._native.save_with_writer(writer)


class OfficialParser:
    def __init__(self, runtime: Optional[OfficialRuntime] = None):
        self.runtime = runtime or OfficialRuntime()
        self._native = _native._OfficialParser(self.runtime._native_runtime())

    async def parse_rich(self, source: Union[PathArg, bytes], *, filename: Optional[str] = None,
                         options: Optional[OfficialParseOptions] = None) -> OfficialResult:
        opts = (options or OfficialParseOptions())._dict()
        if isinstance(source, bytes):
            if filename is None:
                raise ValueError("filename is required for byte input")
            result = await self._native.parse_rich(data=source, filename=filename, options=opts)
        else:
            if filename is not None:
                raise ValueError("filename applies only to byte input")
            result = await self._native.parse_rich(path=os.fspath(source), options=opts)
        return OfficialResult(result, self.runtime)

    async def parse_batch_rich(self, items: Sequence[OfficialBatchItem]) -> List[OfficialBatchResult]:
        native_items = []
        for item in items:
            opts = item.options._dict()
            if isinstance(item.input, bytes):
                if item.filename is None:
                    raise ValueError("filename is required for byte input")
                native_items.append((None, item.input, item.filename, opts))
            else:
                if item.filename is not None:
                    raise ValueError("filename applies only to byte input")
                native_items.append((os.fspath(item.input), None, None, opts))
        rows = await self._native.parse_batch_rich(native_items)
        return [OfficialBatchResult(index, filename,
                                    None if native is None else OfficialResult(native, self.runtime),
                                    error)
                for index, filename, native, error in rows]

    async def shutdown(self) -> None:
        await self._native.shutdown()


async def parse_rich(source: Union[PathArg, bytes], *, filename: Optional[str] = None,
                     options: Optional[OfficialParseOptions] = None,
                     runtime: Optional[OfficialRuntime] = None) -> OfficialResult:
    return await OfficialParser(runtime).parse_rich(source, filename=filename, options=options)


async def parse_batch_rich(items: Sequence[OfficialBatchItem], *,
                           runtime: Optional[OfficialRuntime] = None) -> List[OfficialBatchResult]:
    return await OfficialParser(runtime).parse_batch_rich(items)
