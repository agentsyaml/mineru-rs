"""Model-free restore, serialization, and render operations on official results."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Callable

def dispatch(request: dict[str, object], writer_type: Callable[..., Any]) -> None:
    from docvortex.assets import AssetStore
    from docvortex.export import materialize_middle, validate_materialized_assets
    from mineru.parser import ParseResult
    from mineru_official_worker_bundle import read_operation
    from mineru_official_worker_render import encode_rendered, render_document

    operation = request.get("operation")
    payload, assets = read_operation(request)
    writer = writer_type(
        Path(str(request["bundle_path"])),
        int(request["max_bundle_bytes"]),
        typed_references=True,
    )
    if operation == "serialize":
        result = _parse_payload(payload)
        value = serialize_result(result, payload.get("skip_defaults", True), payload.get("json") is True)
        writer.write_string("operation.json", json.dumps({"value": value}, ensure_ascii=False))
        return
    result = _parse_payload(payload)
    if operation == "restore":
        # Reading/serialization may retain references whose bytes were not supplied.
        middle, owned_assets = result.middle_json, assets
        try:
            candidate, candidate_assets = materialize_middle(middle, assets, asset_resolver=assets.__getitem__)
            validate_materialized_assets(candidate, candidate_assets)
            middle, owned_assets = candidate, candidate_assets
        except KeyError:
            middle, owned_assets = result.middle_json, assets
        except ValueError as error:
            if "Missing materialized" not in str(error):
                raise
            middle, owned_assets = result.middle_json, assets
        restored = ParseResult(middle_json=middle)
        writer.write_string("markdown.md", restored.markdown())
        writer.write_string("middle_json.json", restored.to_json())
        writer.write_string("structured_content.json", json.dumps(restored.structured_content(), ensure_ascii=False, indent=2))
        for path, data in owned_assets.items():
            writer.write(path, data)
        return
    if operation == "render":
        middle, owned_assets = materialize_middle(result.middle_json, assets, asset_resolver=assets.__getitem__)
        validate_materialized_assets(middle, owned_assets)
        rendered = render_document(middle, owned_assets, str(payload.get("format", "")), payload.get("options", {}))
        encode_rendered(rendered, writer, owned_assets)
        return
    raise ValueError("unsupported official worker operation")


def _parse_payload(payload: dict[str, Any]) -> Any:
    from mineru.parser import ParseResult
    if "middle_json" not in payload:
        raise ValueError("middle_json is required")
    if "json_input" in payload and not isinstance(payload["json_input"], bool):
        raise ValueError("json_input must be boolean")
    if payload.get("json_input") is True:
        if not isinstance(payload["middle_json"], str):
            raise ValueError("JSON input must be a string")
        return ParseResult.from_json(payload["middle_json"])
    if not isinstance(payload["middle_json"], dict):
        raise ValueError("middle_json must be an object")
    return ParseResult.from_dict(payload["middle_json"])


def serialize_result(result: Any, skip_defaults: object, as_json: bool) -> object:
    if not isinstance(skip_defaults, bool):
        raise ValueError("skip_defaults must be boolean")
    if as_json:
        return result.to_json() if skip_defaults else json.dumps(result.to_dict(skip_defaults=False), ensure_ascii=False, indent=2)
    return result.to_dict(skip_defaults=skip_defaults)
