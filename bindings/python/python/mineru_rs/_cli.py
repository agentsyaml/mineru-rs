from __future__ import annotations

import asyncio
import os
import sys

from . import _helper_path, _native


def _argv():
    if os.name == "nt":
        return [arg.encode("utf-16-le", "surrogatepass") for arg in sys.argv[1:]]
    return [os.fsencode(arg) for arg in sys.argv[1:]]


def _restore_terminal() -> None:
    try:
        if sys.stderr.isatty():
            sys.stderr.write("\x1b[0m\x1b[?25h")
            sys.stderr.flush()
    except Exception:
        pass


async def _invoke() -> int:
    return await _native._run_cli(_argv(), _helper_path())


def main() -> int:
    try:
        return asyncio.run(_invoke())
    except (KeyboardInterrupt, asyncio.CancelledError):
        _restore_terminal()
        return 130 if os.name != "nt" else 1
    except RuntimeError as error:
        print(f"mineru: {error}", file=sys.stderr)
        return 1
