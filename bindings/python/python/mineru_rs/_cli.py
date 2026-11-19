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


def _exit(code: int) -> int:
    # Interpreter teardown has segfaulted after a complete run on some manylinux
    # runners, turning a successful command into status 139. The CLI owns the
    # process, so flush what is buffered and leave without running teardown.
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.flush()
        except (AttributeError, ValueError):
            pass
    os._exit(code)


def main() -> int:
    try:
        code = asyncio.run(_invoke())
    except (KeyboardInterrupt, asyncio.CancelledError):
        _restore_terminal()
        code = 130 if os.name != "nt" else 1
    except RuntimeError as error:
        print(f"mineru: {error}", file=sys.stderr)
        code = 1
    return _exit(code)
