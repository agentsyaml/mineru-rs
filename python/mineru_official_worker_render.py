"""Exact, callable-free renderer option mapping for MinerU 4.0.12."""

from __future__ import annotations

from datetime import datetime
import json
from typing import Any, cast

_OPTIONS = {
    "markdown": {"mode", "asset_base_url", "latex_delimiters"},
    "html": {"mode", "asset_base_url", "standalone", "document_title"},
    "latex": {"asset_base_path", "document_title"}, "docx": set(),
    "epub": {"title", "authors", "language", "identifier", "modified_at"},
    "structured_content": {"asset_base_url", "latex_delimiters"},
    "content_list": {"asset_base_url", "latex_delimiters"},
    "content_list_v2": {"asset_base_url", "latex_delimiters"},
    "pdf": {"document_title", "layout"},
}


def validate_render_options(fmt: str, raw_options: object) -> dict[str, object]:
    if not isinstance(raw_options, dict) or fmt not in _OPTIONS:
        raise ValueError("render options or format are invalid")
    options = cast(dict[str, object], raw_options)
    if any(not isinstance(key,str) or (key not in _OPTIONS[fmt] and value is not None) for key, value in options.items()):
        raise ValueError("render option is unsupported for this format")
    return {key: value for key, value in options.items() if value is not None}


def render_document(middle: Any, assets: Any, fmt: str, raw_options: object) -> object:
    options = validate_render_options(fmt, raw_options)
    from docvortex.options import LatexDelimitersConfig
    from mineru.render import (
        ContentListRenderOptions, ContentListV2RenderOptions, DocxRenderOptions,
        EpubRenderOptions, HtmlRenderOptions, LatexRenderOptions, MarkdownRenderOptions,
        PdfLayout, PdfRenderOptions, RenderFormat, RenderMode, StructuredContentRenderOptions,
        render,
    )
    if "mode" in options:
        options["mode"] = RenderMode(options["mode"])
    if "latex_delimiters" in options:
        options["latex_delimiters"] = LatexDelimitersConfig.model_validate(options["latex_delimiters"])
    if "layout" in options:
        options["layout"] = PdfLayout(options["layout"])
    if "authors" in options:
        options["authors"] = tuple(options["authors"])
    if "modified_at" in options and options["modified_at"] is not None:
        options["modified_at"] = datetime.fromisoformat(options["modified_at"])
    classes = {
        "markdown": MarkdownRenderOptions, "html": HtmlRenderOptions,
        "latex": LatexRenderOptions, "docx": DocxRenderOptions,
        "epub": EpubRenderOptions, "pdf": PdfRenderOptions,
        "structured_content": StructuredContentRenderOptions,
        "content_list": ContentListRenderOptions,
        "content_list_v2": ContentListV2RenderOptions,
    }
    if fmt in {"docx", "epub", "pdf"}:
        options["asset_resolver"] = assets.__getitem__
    resolved = classes[fmt](**options)
    return render(middle, RenderFormat(fmt), options=resolved)


def validate_assets(result: Any, assets: Any) -> tuple[Any, Any]:
    from docvortex.export import materialize_middle, validate_materialized_assets

    middle, owned = materialize_middle(result.middle_json, assets, asset_resolver=assets.__getitem__)
    validate_materialized_assets(middle, owned)
    return middle, owned


def encode_rendered(value: object, writer: Any, assets: Any) -> None:
    for name, payload in assets.items():
        writer.write(name, payload)
    if isinstance(value, str):
        writer.write_string("render.json", json.dumps({"kind": "text", "content": value}, ensure_ascii=False))
    elif isinstance(value, (dict, list)):
        writer.write_string("render.json", json.dumps({"kind": "json", "content": value}, ensure_ascii=False))
    elif isinstance(value, bytes):
        writer.write("render.bin", value)
        writer.write_string("render.json", json.dumps({"kind": "binary"}))
    else:
        raise TypeError("upstream renderer returned an unsupported result")
