"""Minimal mineru-rs example: parse one file, save the returned markdown."""

# Env vars for the MinerU service:
#   MINERU_VL_SERVER      e.g. https://host/v1
#   MINERU_VL_MODEL_NAME  the model name to use
#   MINERU_VL_API_KEY     optional API key

import asyncio
import os
import sys
from pathlib import Path

import mineru_rs


async def main() -> None:
    if len(sys.argv) > 1:
        path = sys.argv[1]
    else:
        path = "input.pdf"

    if not os.path.isfile(path):
        print(f"error: '{path}' not found", file=sys.stderr)
        print(f"usage: python main.py input.pdf", file=sys.stderr)
        sys.exit(1)

    result = await mineru_rs.parse(path)

    out = Path("output/document.md")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(result.markdown, encoding="utf-8")

    print(f"parsed {path}: markdown {len(result.markdown)} chars -> {out}")
    if result.warnings:
        print(f"warnings ({len(result.warnings)}):")
        for w in result.warnings:
            print(f"  - {w}")


if __name__ == "__main__":
    asyncio.run(main())
