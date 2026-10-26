#!/usr/bin/env python3
"""Offline smoke contract for the pinned upstream Python oracle.

No downloads or installs are performed. Run under a prepared Python 3.11-3.14
environment with the exact distributions installed, or pass --oracle-path for
unpacked, hash-verified wheels that contain their .dist-info directories.
"""

from __future__ import annotations

import argparse
import asyncio
import importlib
import importlib.metadata
import json
import os
from pathlib import Path
import platform
import sys
import tempfile
import traceback
from typing import Any

sys.dont_write_bytecode = True

EXPECTED = {
    "mineru": "4.0.12",
    "docvortex": "0.6.0",
    "mineru-vl-utils": "2.0.7",
}
PYTHON_DIR = Path(__file__).resolve().parents[2] / "python"
PROJECT_ROOT = Path(__file__).resolve().parents[2]


def _arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--oracle-path", action="append", type=Path, default=[],
        help="site-packages or unpacked official wheel root; may be repeated",
    )
    parser.add_argument(
        "--self-test", action="store_true",
        help="check the worker argument mapping without importing upstream packages",
    )
    return parser.parse_args()


def _self_test() -> None:
    sys.path.insert(0, str(PYTHON_DIR))
    from mineru_official_worker_protocol import _document_parse_kwargs

    request = {"tier": "flash", "ocr_mode": "txt", "image_analysis": False}
    expected = {"tier": "flash", "ocr_mode": "txt", "image_analysis": False}
    if _document_parse_kwargs(request) != expected:
        raise AssertionError("worker parse argument mapping changed")


def _prepare_oracle(paths: list[Path]) -> dict[str, Any]:
    resolved = [path.resolve(strict=True) for path in paths]
    if any(not path.is_dir() for path in resolved):
        raise ValueError("every --oracle-path must be a directory")
    sys.path[:0] = [str(path) for path in resolved]
    if str(PYTHON_DIR) not in sys.path:
        sys.path.insert(0, str(PYTHON_DIR))
    versions = {name: importlib.metadata.version(name) for name in EXPECTED}
    if versions != EXPECTED:
        raise RuntimeError(f"oracle versions differ: expected {EXPECTED}, found {versions}")
    if not ((3, 11) <= sys.version_info[:2] < (3, 15)):
        raise RuntimeError(f"DocVortex 0.6 oracle requires Python 3.11-3.14, found {sys.version}")
    modules = {
        "mineru": "mineru.parser",
        "docvortex": "docvortex",
        "mineru-vl-utils": "mineru_vl_utils",
    }
    origins = {}
    for distribution, module_name in modules.items():
        module = importlib.import_module(module_name)
        origin = Path(module.__file__).resolve()
        if resolved and not any(origin.is_relative_to(root) for root in resolved):
            raise RuntimeError(f"{module_name} loaded outside --oracle-path: {origin}")
        origins[distribution] = str(origin)
    return {"versions": versions, "origins": origins, "oracle_paths": [str(p) for p in resolved]}


async def _parse(path: Path, mode: str) -> Any:
    from mineru.parser import parse_async

    return await parse_async(
        path, tier="flash", ocr_mode=mode, image_analysis=False,
    )


def _check_result(result: Any, expected_text: str) -> dict[str, object]:
    from docvortex.schema import MiddleJson

    wire = result.middle_json.model_dump(mode="json", by_alias=True)
    MiddleJson.model_validate(wire)
    markdown = result.markdown()
    if expected_text not in markdown:
        raise AssertionError(f"markdown lacks expected text {expected_text!r}: {markdown!r}")
    structured = result.structured_content()
    if not isinstance(structured, dict) or not isinstance(structured.get("pages"), list):
        raise AssertionError("structured_content is not a page document")
    return {"markdown_bytes": len(markdown.encode("utf-8")), "pages": len(wire["pages"])}


def _check_worker_operations(result: Any, scratch: Path) -> list[str]:
    from mineru_official_worker import BundleWriter
    from mineru_official_worker_operations import dispatch

    scratch.mkdir(parents=True, exist_ok=True)
    middle = result.middle_json.model_dump(mode="json", by_alias=True)
    completed = []
    for operation in ("serialize", "restore", "render"):
        case = scratch / operation
        case.mkdir()
        payload: dict[str, object] = {"middle_json": middle, "assets": []}
        if operation == "render":
            payload.update(format="markdown", options={})
        request_path = case / "request.json"
        request_path.write_text(json.dumps(payload), encoding="utf-8")
        output = case / "bundle"
        dispatch({
            "input_path": str(request_path), "operation_assets": str(case),
            "bundle_path": str(output), "max_bundle_bytes": 64 * 1024 * 1024,
            "operation": operation,
        }, BundleWriter)
        if operation == "restore":
            if (output / "markdown.md").read_text(encoding="utf-8") != result.markdown():
                raise AssertionError("worker restore Markdown differs from MinerU ParseResult")
            restored = json.loads((output / "structured_content.json").read_text(encoding="utf-8"))
            if restored != result.structured_content():
                raise AssertionError("worker restore structured content differs from ParseResult")
        elif operation == "render":
            rendered = json.loads((output / "render.json").read_text(encoding="utf-8"))
            if rendered.get("content") != result.markdown():
                raise AssertionError("worker Markdown render differs from ParseResult")
        elif not (output / "operation.json").is_file():
            raise AssertionError("worker serialize operation produced no result")
        completed.append(operation)
    return completed


def main() -> int:
    args = _arguments()
    if args.self_test:
        _self_test()
        print(json.dumps({"status": "ok", "checks": ["worker argument mapping"]}))
        return 0
    if not args.oracle_path:
        raise ValueError("pass one or more --oracle-path directories; installs/downloads are not performed")

    evidence = _prepare_oracle(args.oracle_path)
    from mineru.parser import ParseResult

    checks: dict[str, object] = {}
    with tempfile.TemporaryDirectory(prefix="mineru-upstream-compat-") as directory:
        scratch = Path(directory)
        csv_path = scratch / "input.csv"
        csv_path.write_text("name,value\nrow,7\n", encoding="utf-8")
        csv_result = asyncio.run(_parse(csv_path, "auto"))
        checks["csv"] = _check_result(csv_result, "row")
        roundtrip = ParseResult.from_dict(csv_result.to_dict())
        if roundtrip.markdown() != csv_result.markdown():
            raise AssertionError("ParseResult serialization roundtrip changed Markdown")
        checks["parse_result_roundtrip"] = "ok"
        checks["worker_operations"] = _check_worker_operations(csv_result, scratch / "operations")

        mhtml_path = scratch / "input.mhtml"
        mhtml_path.write_bytes(
            b'MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary="b"\r\n\r\n'
            b"--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n"
            b"<html><body><p>mhtml-check</p></body></html>\r\n--b--\r\n"
        )
        mhtml_result = asyncio.run(_parse(mhtml_path, "auto"))
        checks["mhtml"] = _check_result(mhtml_result, "mhtml-check")

        pdf_path = PROJECT_ROOT / "tests" / "fixtures" / "pdf" / "minimal.pdf"
        if not pdf_path.is_file():
            raise FileNotFoundError(f"required project PDF fixture missing: {pdf_path}")
        pdf_result = asyncio.run(_parse(pdf_path, "txt"))
        checks["pdf_flash_txt"] = _check_result(pdf_result, "minimal")

    evidence.update(
        status="ok", python=sys.version.split()[0], platform=platform.platform(), checks=checks
    )
    print(json.dumps(evidence, ensure_ascii=False, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({
            "status": "failed", "error": f"{type(error).__name__}: {error}",
            "traceback": traceback.format_exc(),
        }, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(1)
