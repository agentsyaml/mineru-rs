import asyncio
import base64
import os
import inspect
import json
import tempfile
import unittest
import sys
import types
from email.message import EmailMessage
from pathlib import Path
from unittest.mock import patch

try:
    import mineru_rs
except ImportError:
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python"))
    try:
        import mineru_rs
    except ImportError:
        stub = types.ModuleType("mineru_rs._native")
        sys.modules["mineru_rs._native"] = stub
        sys.modules.pop("mineru_rs", None)
        import mineru_rs
from mineru_rs import _native

_NEW_NATIVE = all(hasattr(_native, name) for name in ("_OfficialRuntime", "_OfficialParser", "_restore"))
if os.environ.get("MINERU_TEST_REQUIRE_NATIVE") == "1" and not _NEW_NATIVE:
    raise ImportError("native official Python SDK module is required")


class NativeResult:
    middle_json = '{"pages":[{"n":1}]}'
    model_json = None
    markdown = "# owned"
    structured_content = '{"text":"owned"}'
    pages = '[{"n":1}]'
    assets = [("images/a.png", b"asset")]
    warnings = ["warning"]
    version_metadata = ("4.0.12", "2.0", "hybrid-v4")
    def export_pages(self): return self.pages
    async def to_dict(self, skip_defaults=False): return '{"pages":[{"n":1}]}'
    async def to_json(self, skip_defaults=False): return '{"pages":[{"n":1}]}'
    async def render(self, target, options=None):
        return ("binary", None, b"doc", [("images/a.png", b"asset")])
    def save(self, root, stem): self.saved = (root, stem)
    def save_with_writer(self, writer): writer.write_string("markdown.md", "# owned")


class NativeParser:
    def __init__(self, runtime): self.runtime, self.calls = runtime, []
    async def parse_rich(self, **kwargs): self.calls.append(kwargs); return NativeResult()
    async def parse_batch_rich(self, items):
        return [(0, "bad.pdf", None, "failed"), (1, "ok.pdf", NativeResult(), None)]
    async def shutdown(self): self.stopped = True


class OfficialFacadeTests(unittest.TestCase):
    def test_legacy_parse_signature_is_unchanged(self):
        self.assertEqual(list(inspect.signature(mineru_rs.parse).parameters), [
            "path", "api_url", "method", "backend", "effort", "tier", "lang", "url",
            "start", "end", "formula", "table", "image_analysis", "client_side_output_generation",
        ])
        self.assertIs(mineru_rs.ParseResult, type(mineru_rs.ParseResult("text", [])))

    def test_options_are_typed_and_credentials_hidden_from_repr(self):
        cfg = mineru_rs.OfficialVlmConfig(api_key="secret", server_url="https://u:p@host")
        opts = mineru_rs.OfficialParseOptions(vlm_config=cfg, vlm_api_key="alias")
        self.assertNotIn("secret", repr(cfg) + repr(opts))
        self.assertNotIn("alias", repr(opts))
        self.assertNotIn("/private/python", repr(mineru_rs.OfficialRuntime(python="/private/python")))
        with self.assertRaises(ValueError):
            mineru_rs.OfficialParseOptions(tier="invalid")
        with self.assertRaises(TypeError):
            mineru_rs.OfficialParseOptions(image_analysis=1)

    def test_all_parse_controls_preserve_full_values(self):
        options = mineru_rs.OfficialParseOptions(
            tier=mineru_rs.OfficialTier.FLASH,
            ocr_mode=mineru_rs.OfficialOcrMode.OCR,
            image_analysis=False,
            page_range="1,3-5,9",
            source_context=mineru_rs.HtmlSourceContext(source_uri="file:///source", local_resource_root="/trusted"),
            vlm_config=mineru_rs.OfficialVlmConfig(engine="vllm", api_key="secret"),
        )._dict()
        self.assertEqual(options["page_range"], "1,3-5,9")
        self.assertEqual(options["source_context"]["local_resource_root"], "/trusted")
        self.assertEqual(options["vlm_config"]["api_key"], "secret")
        self.assertFalse(options["image_analysis"])

    def test_bytes_parse_maps_options_and_result_owns_data(self):
        with patch.object(_native, "_OfficialRuntime", side_effect=lambda *a: a, create=True), \
             patch.object(_native, "_OfficialParser", NativeParser, create=True):
            parser = mineru_rs.OfficialParser(mineru_rs.OfficialRuntime())
            options = mineru_rs.OfficialParseOptions(
                tier=mineru_rs.OfficialTier.FLASH, ocr_mode=mineru_rs.OfficialOcrMode.OCR,
                image_analysis=False, page_range="1,3-5,9",
                source_context=mineru_rs.HtmlSourceContext(local_resource_root="/trusted"),
                vlm_config=mineru_rs.OfficialVlmConfig(engine="vllm", api_key="secret"))
            result = asyncio.run(parser.parse_rich(b"input", filename="a.pdf", options=options))
            self.assertEqual(parser._native.calls[0]["data"], b"input")
            forwarded = parser._native.calls[0]["options"]
            self.assertEqual(forwarded["tier"], "flash")
            self.assertEqual(forwarded["ocr_mode"], "ocr")
            self.assertIs(forwarded["image_analysis"], False)
            self.assertEqual(forwarded["page_range"], "1,3-5,9")
            self.assertEqual(forwarded["source_context"]["local_resource_root"], "/trusted")
            self.assertEqual(forwarded["vlm_config"]["api_key"], "secret")
            middle = result.middle_json; middle["pages"].clear()
            self.assertEqual(result.pages, [{"n": 1}])
            self.assertEqual(result.assets["images/a.png"], b"asset")
            self.assertEqual(result.version_metadata.schema_version, "2.0")
            self.assertEqual(asyncio.run(result.to_dict())["pages"][0]["n"], 1)

    def test_batch_errors_keep_order_and_render_formats_are_tagged(self):
        with patch.object(_native, "_OfficialRuntime", side_effect=lambda *a: a, create=True), \
             patch.object(_native, "_OfficialParser", NativeParser, create=True):
            parser = mineru_rs.OfficialParser()
            rows = asyncio.run(parser.parse_batch_rich([
                mineru_rs.OfficialBatchItem("bad.pdf"), mineru_rs.OfficialBatchItem("ok.pdf")]))
            self.assertEqual([r.index for r in rows], [0, 1])
            self.assertEqual(rows[0].error, "failed")
            result = rows[1].result
            for target in mineru_rs.OfficialRenderFormat:
                rendered = asyncio.run(result.render(target))
                self.assertIsInstance(rendered, mineru_rs.OfficialBinary)

    def test_custom_writer_exception_propagates_and_old_result_is_unaffected(self):
        result = mineru_rs.OfficialResult(NativeResult(), mineru_rs.OfficialRuntime())
        class Writer:
            def write_string(self, path, text): raise LookupError("writer failed")
        with self.assertRaisesRegex(LookupError, "writer failed"):
            result.save_with_writer(Writer())


@unittest.skipUnless(os.environ.get("MINERU_OFFICIAL_PYTHON"), "set MINERU_OFFICIAL_PYTHON for native worker integration")
class OfficialNativeIntegrationTests(unittest.IsolatedAsyncioTestCase):
    async def test_worker_parse_batch_restore_render_save_and_shutdown(self):
        worker_python = os.environ["MINERU_OFFICIAL_PYTHON"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            csv = root / "sample.csv"
            csv.write_text("name,value\nalpha,1\n", encoding="utf-8")
            for mode in (mineru_rs.OfficialWorkerMode.PER_DOCUMENT,
                         mineru_rs.OfficialWorkerMode.PERSISTENT):
                runtime = mineru_rs.OfficialRuntime(python=worker_python, worker_mode=mode)
                parser = mineru_rs.OfficialParser(runtime)
                self.addAsyncCleanup(parser.shutdown)
                parsed = await parser.parse_rich(csv, options=mineru_rs.OfficialParseOptions(
                    tier=mineru_rs.OfficialTier.FLASH, ocr_mode=mineru_rs.OfficialOcrMode.AUTO,
                    image_analysis=True))
                self.assertIn("alpha", parsed.markdown)
                self.assertEqual(parsed.version_metadata.schema_version, "2.0")
                self.assertEqual(parsed.pages, parsed.export_pages())
                batch = await parser.parse_batch_rich([
                    mineru_rs.OfficialBatchItem(b"bad", filename="bad.unknown"),
                    mineru_rs.OfficialBatchItem(csv)])
                self.assertEqual([row.index for row in batch], [0, 1])
                self.assertIsNotNone(batch[0].error)
                self.assertIsNotNone(batch[1].result)
                await parser.shutdown()

            png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=")
            message = EmailMessage()
            message.set_type("multipart/related")
            message.set_param("type", "text/html")
            message.set_boundary("python-sdk-smoke")
            html = EmailMessage()
            html.set_content('<p>owned MHTML asset</p><img src="cid:asset">', subtype="html")
            html["Content-Location"] = "file:///mhtml/index.html"
            message.attach(html)
            image = EmailMessage()
            image.set_content(png, maintype="image", subtype="png", cte="base64")
            image["Content-ID"] = "<asset>"
            message.attach(image)
            runtime = mineru_rs.OfficialRuntime(python=worker_python,
                                                worker_mode=mineru_rs.OfficialWorkerMode.PERSISTENT)
            parser = mineru_rs.OfficialParser(runtime)
            self.addAsyncCleanup(parser.shutdown)
            parsed = await parser.parse_rich(message.as_bytes(), filename="asset.mhtml",
                                             options=mineru_rs.OfficialParseOptions(
                                                 tier=mineru_rs.OfficialTier.FLASH))
            self.assertTrue(parsed.assets)
            encoded = await parsed.to_json()
            restored = await mineru_rs.OfficialResult.from_json(runtime, encoded, parsed.assets)
            rebuilt = await mineru_rs.OfficialResult.from_dict(runtime, await restored.to_dict(), restored.assets)
            self.assertEqual(rebuilt.markdown, parsed.markdown)
            self.assertEqual(rebuilt.assets, parsed.assets)
            for target in mineru_rs.OfficialRenderFormat:
                output = await rebuilt.render(target)
                if target in (mineru_rs.OfficialRenderFormat.MARKDOWN,
                              mineru_rs.OfficialRenderFormat.HTML,
                              mineru_rs.OfficialRenderFormat.LATEX):
                    self.assertIsInstance(output, mineru_rs.OfficialText)
                elif target in (mineru_rs.OfficialRenderFormat.STRUCTURED_CONTENT,
                                mineru_rs.OfficialRenderFormat.CONTENT_LIST,
                                mineru_rs.OfficialRenderFormat.CONTENT_LIST_V2):
                    self.assertIsInstance(output, mineru_rs.OfficialJson)
                else:
                    self.assertIsInstance(output, mineru_rs.OfficialBinary)
            save_root = root / "saved"
            rebuilt.save(save_root, "mhtml")
            self.assertTrue(list((save_root / "mhtml" / "hybrid-v4" / "images").glob("*")))
            class Writer:
                def __init__(self): self.files = {}
                def write(self, name, data): self.files[name] = bytes(data)
                def write_string(self, name, text): self.files[name] = text
            writer = Writer()
            rebuilt.save_with_writer(writer)
            self.assertIn("markdown.md", writer.files)
            class FailingWriter:
                def write_string(self, name, text): raise LookupError("writer failure")
                def write(self, name, data): raise LookupError("writer failure")
            with self.assertRaisesRegex(LookupError, "writer failure"):
                rebuilt.save_with_writer(FailingWriter())
            await parser.shutdown()


if __name__ == "__main__":
    unittest.main()
