import asyncio
import io
import json
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import mineru_rs


PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010802000000907753de"
    "0000000c4944415408d763f8ffff3f0005fe02fe0def46b80000000049454e44ae426082"
)


def result_zip(files=None):
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", zipfile.ZIP_STORED) as archive:
        for name, data in (files or {"result.txt": b"ok"}).items():
            archive.writestr(name, data)
    return output.getvalue()


class MockApi:
    def __init__(self, *, blocked=False, failed=False, files=None):
        self.failed = failed
        self.body = b""
        self.result = result_zip(files)
        self.result_started = threading.Event()
        self.gate = threading.Event()
        if not blocked:
            self.gate.set()
        state = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def json(self, status, value):
                payload = json.dumps(value, separators=(",", ":")).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def do_GET(self):
                if self.path == "/health":
                    self.json(
                        200,
                        {
                            "status": "healthy",
                            "protocol_version": 2,
                            "max_concurrent_requests": 1,
                            "processing_window_size": 1,
                        },
                    )
                elif self.path == "/status/1":
                    if state.failed:
                        self.json(200, {"status": "failed", "message": "mock detail"})
                    else:
                        self.json(200, {"status": "completed"})
                elif self.path == "/result/1":
                    state.result_started.set()
                    if not state.gate.wait(5):
                        self.send_error(500)
                        return
                    self.send_response(200)
                    self.send_header("content-type", "application/zip")
                    self.send_header("content-length", str(len(state.result)))
                    self.end_headers()
                    self.wfile.write(state.result)
                else:
                    self.send_error(404)

            def do_POST(self):
                if self.path != "/tasks":
                    self.send_error(404)
                    return
                state.body = self.rfile.read(int(self.headers["content-length"]))
                host, port = self.server.server_address
                base = f"http://{host}:{port}"
                self.json(
                    202,
                    {
                        "task_id": "1",
                        "status_url": f"{base}/status/1",
                        "result_url": f"{base}/result/1",
                    },
                )

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(
            target=lambda: self.server.serve_forever(poll_interval=0.01), daemon=True
        )
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}"

    def close(self):
        self.gate.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


async def wait_thread_event(event, timeout=2):
    if not await asyncio.to_thread(event.wait, timeout):
        raise AssertionError("mock event timed out")


class BindingTests(unittest.TestCase):
    def test_legacy_helpers_and_public_surface(self):
        self.assertEqual(
            mineru_rs.__all__,
            [
                "ParseResult",
                "RunReport",
                "canonical_stem",
                "parse",
                "run",
                "validate_pdf_options",
            ],
        )
        self.assertEqual(mineru_rs.canonical_stem("a bad/pdf"), "a bad_pdf")
        self.assertEqual(mineru_rs.canonical_stem("文档《报告》"), "文档《报告》")
        self.assertEqual(mineru_rs.canonical_stem(""), "document")
        with self.assertRaises(ValueError):
            mineru_rs.canonical_stem("con")
        self.assertTrue(mineru_rs.validate_pdf_options(0, None, True, True, True))
        with self.assertRaises(ValueError):
            mineru_rs.validate_pdf_options(5, 2, True, True, True)

    def test_private_cli_rejects_invalid_byte_protocol(self):
        code = (
            "import asyncio\n"
            "from mineru_rs import _helper_path, _native\n"
            "async def invoke(): return await _native._run_cli([b'\\x00'], _helper_path())\n"
            "value = asyncio.run(invoke())\n"
            "raise SystemExit(0 if value == 2 else 3)\n"
        )
        result = subprocess.run(
            [sys.executable, "-c", code], stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout, b"")
        self.assertEqual(
            result.stderr, b"error: invalid Python CLI argument encoding\n"
        )

    def test_options_report_and_warnings(self):
        async def scenario(root, api):
            inputs = root / "inputs"
            inputs.mkdir()
            (inputs / "input.png").write_bytes(PNG)
            (inputs / "ignored.txt").write_text("ignored")
            output = root / "output"
            output.mkdir()
            report = await mineru_rs.run(
                inputs,
                output,
                api_url=api.url,
                method="ocr",
                effort="high",
                lang="korean",
                url="http://model.invalid",
                start=3,
                end=4,
                formula=False,
                table=False,
                image_analysis=False,
            )
            self.assertEqual(vars(report), {"warnings": report.warnings})
            self.assertTrue(any("unsupported input" in warning for warning in report.warnings))
            self.assertEqual((output / "result.txt").read_bytes(), b"ok")

        with tempfile.TemporaryDirectory() as tmp, MockApi() as api:
            asyncio.run(scenario(Path(tmp), api))
            for expected in [
                b'form-data; name="lang_list"\r\n\r\nkorean',
                b'form-data; name="effort"\r\n\r\nhigh',
                b'form-data; name="parse_method"\r\n\r\nocr',
                b'form-data; name="formula_enable"\r\n\r\nfalse',
                b'form-data; name="start_page_id"\r\n\r\n3',
                b'form-data; name="end_page_id"\r\n\r\n4',
                b'form-data; name="server_url"\r\n\r\nhttp://model.invalid',
            ]:
                self.assertIn(expected, api.body)

    def test_runtime_error_detail_and_static_rejection(self):
        async def scenario(root, api):
            source = root / "input.png"
            source.write_bytes(PNG)
            failed_output = root / "failed"
            failed_output.mkdir()
            with self.assertRaisesRegex(
                RuntimeError, r"1 API task\(s\) failed: task#1 \[input\].*mock detail"
            ):
                await mineru_rs.run(source, failed_output, api_url=api.url)

            missing = root / "missing.png"
            untouched = root / "untouched"
            with self.assertRaisesRegex(RuntimeError, "unsupported method: invalid"):
                await mineru_rs.run(missing, untouched, method="invalid")
            self.assertFalse(untouched.exists())

        with tempfile.TemporaryDirectory() as tmp, MockApi(failed=True) as api:
            asyncio.run(scenario(Path(tmp), api))

    def test_event_loop_timer_is_not_blocked(self):
        async def scenario(root, api):
            source = root / "input.png"
            source.write_bytes(PNG)
            output = root / "output"
            output.mkdir()
            running = asyncio.create_task(mineru_rs.run(source, output, api_url=api.url))
            await wait_thread_event(api.result_started)
            started = time.monotonic()
            await asyncio.sleep(0.01)
            self.assertLess(time.monotonic() - started, 0.2)
            self.assertFalse(running.done())
            api.gate.set()
            await running

        with tempfile.TemporaryDirectory() as tmp, MockApi(blocked=True) as api:
            asyncio.run(scenario(Path(tmp), api))

    def test_cancelled_observer_detaches_owned_native_task(self):
        async def scenario(root, api):
            source = root / "input.png"
            source.write_bytes(PNG)
            output = root / "output"
            output.mkdir()
            observer = asyncio.create_task(mineru_rs.run(source, output, api_url=api.url))
            await wait_thread_event(api.result_started)
            started = time.monotonic()
            observer.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await observer
            self.assertLess(time.monotonic() - started, 0.2)

            api.gate.set()
            deadline = time.monotonic() + 2
            while not (output / "result.txt").exists() and time.monotonic() < deadline:
                await asyncio.sleep(0.005)
            self.assertEqual((output / "result.txt").read_bytes(), b"ok")
            leftovers = [path for path in output.rglob("*") if ".mineru-" in path.name]
            self.assertEqual(leftovers, [])

        with tempfile.TemporaryDirectory() as tmp, MockApi(blocked=True) as api:
            asyncio.run(scenario(Path(tmp), api))


if __name__ == "__main__":
    unittest.main()
