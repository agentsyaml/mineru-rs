import asyncio
import io
import sys
import tempfile
import types
import unittest
import zipfile
from pathlib import Path

try:
    import mineru_rs  # noqa: F401  (installed wheel / built native module)
except ImportError:
    # Plain source checkout: mineru_rs is importable but the compiled
    # `_native` module may be absent (maturin build not run). Resolve the
    # source package and stub `_native` so the facade tests can still run.
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
    try:
        import mineru_rs  # noqa: F401
    except ImportError:
        stub = types.ModuleType("mineru_rs._native")
        stub.canonical_stem = lambda value: Path(value).stem  # mirror stem semantics
        stub.validate_pdf_options = lambda *args: True
        stub._run = None
        sys.modules["mineru_rs._native"] = stub
        sys.modules.pop("mineru_rs", None)
        import mineru_rs  # noqa: F401


class FakeRun:
    """Patches `_native._run`; writes `{file_stem}.md` (real CLI naming) into the output dir."""

    def __init__(self, *, subdir=False, write_markdown=True):
        self.subdir = subdir
        self.write_markdown = write_markdown
        self.output_paths = []

    async def __call__(self, path, output, *_args):
        output = Path(output)
        self.output_paths.append(output)
        if not self.write_markdown:
            return ["warn"]
        stem = Path(path).stem
        if self.subdir:
            target = output / stem / "vlm"
        else:
            target = output
        target.mkdir(parents=True, exist_ok=True)
        (target / f"{stem}.md").write_text("# parsed", encoding="utf-8")
        return ["warn"]


class ParseTests(unittest.TestCase):
    def setUp(self):
        self.original_run = mineru_rs._native._run

    def tearDown(self):
        mineru_rs._native._run = self.original_run

    def patch_run(self, fake):
        mineru_rs._native._run = fake

    def test_returns_markdown_and_warnings(self):
        fake = FakeRun()
        self.patch_run(fake)
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "input.pdf"
            source.write_text("fake", encoding="utf-8")
            result = asyncio.run(mineru_rs.parse(source))
        self.assertEqual(result.markdown, "# parsed")
        self.assertEqual(result.warnings, ["warn"])
        self.assertTrue(fake.output_paths)
        for output in fake.output_paths:
            self.assertFalse(output.exists(), "temp output dir was not cleaned up")

    def test_reads_markdown_from_subdirectory(self):
        fake = FakeRun(subdir=True)
        self.patch_run(fake)
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "input.pdf"
            source.write_text("fake", encoding="utf-8")
            result = asyncio.run(mineru_rs.parse(source))
        self.assertEqual(result.markdown, "# parsed")
        self.assertFalse(fake.output_paths[0].exists())

    def test_cleanup_when_markdown_is_missing(self):
        fake = FakeRun(write_markdown=False)
        self.patch_run(fake)
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "input.pdf"
            source.write_text("fake", encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "no markdown output"):
                asyncio.run(mineru_rs.parse(source))
        self.assertFalse(fake.output_paths[0].exists())

    @unittest.skipIf(
        getattr(mineru_rs._native, "_run", None) is None,
        "compiled native module unavailable; cannot exercise the office helper contract",
    )
    def test_office_conversion_unavailable_is_reported(self):
        # A minimal but valid OOXML package passes the core's OOXML detection and reaches the
        # office helper spawn. The helper is not bundled with the Python package, so the spawn
        # fails and the unavailable contract is reported. A bogus (non-OOXML) file would instead
        # fail earlier with "invalid OOXML" and never reach this branch.
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        import threading

        class Models(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path == "/v1/models":
                    body = b'{"data":[{"id":"mock"}]}'
                    self.send_response(200)
                    self.send_header("content-type", "application/json")
                    self.send_header("content-length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                else:
                    self.send_response(404)
                    self.end_headers()

            def log_message(self, format, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Models)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                buf = io.BytesIO()
                with zipfile.ZipFile(buf, "w") as z:
                    z.writestr(
                        "_rels/.rels",
                        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
                    )
                    z.writestr(
                        "[Content_Types].xml",
                        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
                    )
                source = Path(tmp) / "office.docx"
                output = Path(tmp) / "output"
                source.write_bytes(buf.getvalue())
                output.mkdir()
                with self.assertRaisesRegex(RuntimeError, "office conversion is unavailable"):
                    asyncio.run(
                        mineru_rs.run(
                            source,
                            output,
                            method="ocr",
                            url=f"http://127.0.0.1:{server.server_address[1]}",
                        )
                    )
        finally:
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    unittest.main()
