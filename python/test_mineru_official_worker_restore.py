import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import mineru_official_worker as worker
import mineru_official_worker_operations as operations


class RichRestoreRebasingTests(unittest.TestCase):
    def test_dispatch_rebases_actual_middle_and_structured_content(self) -> None:
        try:
            from docvortex.schema import MiddleJson
            import mineru.parser
        except ModuleNotFoundError:
            self.skipTest("real MinerU/DocVortex restore requires the prepared worker venv")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            assets = root / "assets" / "images"
            assets.mkdir(parents=True)
            (assets / "sample.png").write_bytes(b"image")
            middle = MiddleJson.from_dict({
                "schema": "docvortex.middle", "schema_version": "2.0", "is_full_document": True,
                "metadata": {"file_suffix": "csv", "producer": {"name": "MinerU", "version": "4.0.12"},
                             "document": {"title": "sample.png"}},
                "extensions": {"audit": {"image_path": "ordinary-value", "label": "sample.png"},
                               "sample.png": "ordinary-key"},
                "pages": [{"page_idx": 0, "blocks": [
                    {"type": "image", "index": 0, "content": [
                        {"type": "image_body", "index": 0, "image_path": "sample.png", "content": '<img src="sample.png"> ![a](sample.png) ![b](sample.png) <img src="sample.png"> ![c](sample.png)'}
                    ]},
                    {"type": "code", "index": 1, "sub_type": "code", "guess_lang": "text", "content": [
                        {"type": "code_body", "index": 1, "content": "![literal](sample.png)"}
                    ]},
                ]}],
            })
            source, bundle = root / "operation.json", root / "bundle"
            source.write_text(json.dumps({"middle_json": middle.to_dict(skip_defaults=False), "assets": ["images/sample.png"]}))
            request: dict[str, object] = {
                "input_path": str(source), "operation_assets": str(root / "assets"),
                "bundle_path": str(bundle), "max_bundle_bytes": 1 << 20, "operation": "restore",
            }
            operations.dispatch(request, worker.BundleWriter)
            restored = json.loads((bundle / "middle_json.json").read_text())
            structured = json.loads((bundle / "structured_content.json").read_text())
            markdown = (bundle / "markdown.md").read_text()
            self.assertEqual(restored["metadata"]["document"]["title"], "sample.png")
            self.assertEqual(restored["extensions"]["audit"], {"image_path": "ordinary-value", "label": "sample.png"})
            self.assertEqual(restored["extensions"]["sample.png"], "ordinary-key")
            self.assertEqual(restored["pages"][0]["blocks"][0]["content"][0]["image_path"], "images/sample.png")
            self.assertEqual(restored["pages"][0]["blocks"][0]["content"][0]["content"], '<img src="images/sample.png"> ![a](images/sample.png) ![b](images/sample.png) <img src="images/sample.png"> ![c](images/sample.png)')
            self.assertEqual(restored["pages"][0]["blocks"][1]["content"][0]["content"], "![literal](sample.png)")
            self.assertEqual(structured["pages"][0]["blocks"][0]["image_source"], "images/sample.png")
            self.assertIn("images/sample.png", markdown)
            self.assertEqual((bundle / "images/sample.png").read_bytes(), b"image")


if __name__ == "__main__":
    unittest.main()
