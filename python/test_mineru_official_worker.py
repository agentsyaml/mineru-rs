import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from typing import cast
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import mineru_official_worker as worker  # noqa: E402
import mineru_official_worker_protocol as protocol  # noqa: E402
import mineru_official_worker_operations as worker_operations  # noqa: E402
import mineru_official_worker_render as worker_render  # noqa: E402


class _BinaryInput:
    def __init__(self, value: bytes) -> None:
        self.buffer = io.BytesIO(value)


class _BinaryOutput:
    def __init__(self) -> None:
        self.buffer = io.BytesIO()

    def flush(self) -> None:
        pass


class BundleWriterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)

    def tearDown(self) -> None:
        self.directory.cleanup()

    def test_normalizes_and_limits_paths(self) -> None:
        writer = worker.BundleWriter(self.root, 1024)
        self.assertEqual(writer._name("image.jpg"), "images/image.jpg")
        self.assertEqual(writer._name("images/image.jpg"), "images/image.jpg")
        self.assertEqual(writer._name("markdown.md"), "markdown.md")
        self.assertEqual(writer._name("images/" + "é" * 127 + "x"), "images/" + "é" * 127 + "x")
        with self.assertRaises(worker.LimitError):
            writer._name("images/" + "/".join(["x"] * 32))
        with self.assertRaises(worker.LimitError):
            writer._name("images/" + "é" * 128)
        with self.assertRaises(worker.LimitError):
            writer._name("images/" + "/".join(["x" * 255] * 16))

    def test_rejects_windows_nonportable_names(self) -> None:
        writer = worker.BundleWriter(self.root, 1024)
        for character in '<>:\"|?*':
            with self.assertRaises(ValueError):
                writer._name(f"images/name{character}.png")
        for character in ("\x00", "\x1f", "\x7f", "\x9f"):
            with self.assertRaises(ValueError):
                writer._name(f"images/name{character}.png")
        for name in ("name.", "name ", ".", "..", "CON", "con.txt", "PrN.data", "AUX", "NUL", "COM1", "LPT9"):
            with self.assertRaises(ValueError):
                writer._name(f"images/{name}")
        self.assertEqual(writer._name("images/COM0"), "images/COM0")
        self.assertEqual(writer._name("images/COM10"), "images/COM10")

    def test_case_insensitive_file_directory_and_alias_collisions(self) -> None:
        writer = worker.BundleWriter(self.root, 1024)
        writer.write("Photo.PNG", b"one")
        with self.assertRaises(ValueError):
            writer.write("photo.png", b"two")
        self.assertEqual((self.root / "images/Photo.PNG").read_bytes(), b"one")
        self.assertEqual(writer._image_aliases, {"Photo.PNG": "images/Photo.PNG"})
        self.assertEqual(writer._image_alias_keys, {"photo.png": "Photo.PNG"})

        writer = worker.BundleWriter(self.root / "nested", 1024)
        writer.write("images/Photo/meta.bin", b"one")
        with self.assertRaises(ValueError):
            writer.write("PHOTO", b"two")
        with self.assertRaises(ValueError):
            writer.write("images/photo/other.bin", b"two")
        self.assertEqual((self.root / "nested/images/Photo/meta.bin").read_bytes(), b"one")

        writer = worker.BundleWriter(self.root / "reverse", 1024)
        writer.write("PHOTO", b"one")
        with self.assertRaises(ValueError):
            writer.write("images/photo/meta.bin", b"two")
        self.assertEqual((self.root / "reverse/images/PHOTO").read_bytes(), b"one")

    def test_entry_and_name_budgets_are_bounded(self) -> None:
        writer = worker.BundleWriter(self.root, 1024)
        writer._files.update(f"existing-{index}" for index in range(worker.MAX_BUNDLE_ENTRIES))
        with self.assertRaises(worker.LimitError):
            writer.write("new.jpg", b"x")

        writer = worker.BundleWriter(self.root, 1024)
        writer._name_bytes = worker.MAX_BUNDLE_NAME_BUDGET
        with self.assertRaises(worker.LimitError):
            writer.write("new.jpg", b"x")

    def test_bundle_and_text_caps(self) -> None:
        writer = worker.BundleWriter(self.root, 4)
        with self.assertRaisesRegex(worker.LimitError, "official bundle exceeds configured byte limit"):
            writer.write_string("markdown.md", "12345")
        self.assertFalse((self.root / "markdown.md").exists())

        writer = worker.BundleWriter(self.root, 1)
        with self.assertRaises(worker.LimitError):
            writer._rewrite_text_bytes("structured_content.json", b"{}")

    def test_temp_file_is_exclusive_and_does_not_collide(self) -> None:
        image_dir = self.root / "images"
        image_dir.mkdir()
        collision = image_dir / "image.jpg.tmp"
        collision.write_bytes(b"sentinel")
        writer = worker.BundleWriter(self.root, 1024)
        writer.write("image.jpg", b"payload")
        self.assertEqual((image_dir / "image.jpg").read_bytes(), b"payload")
        self.assertEqual(collision.read_bytes(), b"sentinel")
        self.assertEqual(list(image_dir.glob(".image.jpg.*.tmp")), [])

    def test_typed_rebasing_preserves_metadata_extensions_and_code(self) -> None:
        writer = worker.BundleWriter(self.root, 4096, typed_references=True)
        writer.write("sample.png", b"image")
        middle = {
            "metadata": {"document": {"title": "sample.png"}},
            "extensions": {"image_path": "ordinary-value", "sample.png": "ordinary-key"},
            "pages": [{"blocks": [
                {"type": "image", "index": 0, "content": [
                    {"type": "image_body", "index": 0, "image_path": "sample.png", "content": '<img src="sample.png"> ![a](sample.png) ![b](sample.png) <img src="sample.png"> ![c](sample.png)'}
                ]},
                {"type": "code", "index": 1, "sub_type": "code", "guess_lang": "text", "content": [
                    {"type": "code_body", "index": 1, "content": "![literal](sample.png)"}
                ]},
            ]}],
        }
        value = json.dumps(middle)
        actual = writer._rewrite_text_bytes("middle_json.json", value.encode()).decode()
        restored = json.loads(actual)
        self.assertEqual(restored["metadata"]["document"]["title"], "sample.png")
        self.assertEqual(restored["extensions"]["image_path"], "ordinary-value")
        self.assertEqual(restored["extensions"]["sample.png"], "ordinary-key")
        image, code = restored["pages"][0]["blocks"]
        self.assertEqual(image["content"][0]["image_path"], "images/sample.png")
        self.assertEqual(image["content"][0]["content"], '<img src="images/sample.png"> ![a](images/sample.png) ![b](images/sample.png) <img src="images/sample.png"> ![c](images/sample.png)')
        self.assertEqual(code["content"][0]["content"], "![literal](sample.png)")

class DiagnosticTests(unittest.TestCase):
    def test_both_streams_are_captured_and_large_diagnostics_stay_bounded(self) -> None:
        captured: dict[str, object] = {}

        class Result:
            def save(self, target: object) -> None:
                sys.stdout.write("save stdout\n" + "s" * 20_000)
                sys.stderr.write("save stderr\n" + "t" * 20_000)
                getattr(target, "write_string")("markdown.md", "ok")

        class FakeParser:
            async def parse_async(self, _input: str, **_kwargs: object) -> Result:
                captured["kwargs"] = _kwargs
                sys.stdout.write("parse stdout\n" + "p" * 20_000)
                sys.stderr.write("parse stderr\n" + "q" * 20_000)
                return Result()

        fake_parser = FakeParser()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        request = {
            "protocol": worker.PROTOCOL,
            "request_id": "diagnostic-test",
            "tier": "standard",
            "ocr_mode": "auto",
            "image_analysis": False,
            "input_path": "input.pdf",
            "bundle_path": str(Path(directory.name) / "bundle"),
            "max_bundle_bytes": 1024,
            "bundle_name": worker.BUNDLE_NAME,
        }
        output = _BinaryOutput()
        stderr = io.StringIO()

        def fake_import(name: str) -> object:
            return fake_parser if name == "mineru.parser" else object()

        with (
            mock.patch.object(sys, "stdin", _BinaryInput(json.dumps(request).encode())),
            mock.patch.object(sys, "__stdout__", output),
            mock.patch.object(sys, "stderr", stderr),
            mock.patch.object(
                worker.importlib.metadata,
                "version",
                return_value=worker.PACKAGE_VERSION,
            ),
            mock.patch.object(worker.importlib, "import_module", side_effect=fake_import),
        ):
            worker.main()

        response = json.loads(output.buffer.getvalue().decode().strip())
        diagnostic = response["diagnostic"]
        self.assertEqual(response["status"], "ok")
        self.assertEqual(response["package_version"], worker.PACKAGE_VERSION)
        self.assertEqual(response["schema_version"], protocol.SCHEMA_VERSION)

        self.assertEqual(
            captured["kwargs"],
            {"tier": "standard", "ocr_mode": "auto", "image_analysis": False},
        )
        self.assertIn("parse stdout", diagnostic)
        self.assertIn("parse stderr", diagnostic)
        self.assertIn("save stdout", diagnostic)
        self.assertIn("save stderr", diagnostic)
        self.assertLessEqual(len(diagnostic.encode()), worker.DIAGNOSTIC_CAP)
        self.assertLessEqual(len(stderr.getvalue().encode()), worker.DIAGNOSTIC_CAP)
        self.assertLessEqual(len(output.buffer.getvalue()), worker.PROTOCOL_CAP)


class OfficialOperationSerializationTests(unittest.TestCase):
    def test_json_serialization_honors_skip_defaults_for_empty_pages(self) -> None:
        class EmptyPages:
            def to_dict(self, *, skip_defaults: bool) -> dict[str, object]:
                page: dict[str, object] = {"page_idx": 1}
                value: dict[str, object] = {"pages": [page]}
                if not skip_defaults:
                    page["blocks"] = []
                    value.update({"schema_version": "2.0", "producer": "MinerU"})
                return value

            def to_json(self) -> str:
                return json.dumps(self.to_dict(skip_defaults=True), ensure_ascii=False)

        result = EmptyPages()
        self.assertEqual(worker_operations.serialize_result(result, True, False), {"pages": [{"page_idx": 1}]})
        self.assertEqual(
            worker_operations.serialize_result(result, False, False),
            {"pages": [{"page_idx": 1, "blocks": []}], "schema_version": "2.0", "producer": "MinerU"},
        )
        encoded = worker_operations.serialize_result(result, False, True)
        self.assertIsInstance(encoded, str)
        self.assertIn('"schema_version": "2.0"', cast(str, encoded))
        self.assertIn('"producer": "MinerU"', cast(str, encoded))

    def test_empty_render_options_are_allowed_for_all_frozen_formats(self) -> None:
        for output_format in (
            "markdown", "html", "latex", "docx", "epub", "structured_content",
            "content_list", "content_list_v2", "pdf",
        ):
            self.assertEqual(worker_render.validate_render_options(output_format, {}), {})
        with self.assertRaises(ValueError):
            worker_render.validate_render_options("pdf", {"standalone": False})

    def test_version_pin_in_both_worker_modes(self) -> None:
        self.assertEqual(worker.PACKAGE_VERSION, "4.0.12")
        request = {
            "protocol": worker.PROTOCOL, "request_id": "version-test",
            "tier": "standard", "ocr_mode": "auto", "image_analysis": False,
            "input_path": "input.pdf", "bundle_path": "bundle",
            "max_bundle_bytes": 1024, "bundle_name": worker.BUNDLE_NAME,
        }
        startup = {
            "type": "start", "protocol": protocol.PERSISTENT_PROTOCOL,
            "package_version": protocol.PACKAGE_VERSION,
            "schema_version": protocol.SCHEMA_VERSION, "model_base_dir": None,
            "config": None, "vlm_api_key": None, "vlm_model": None,
            "capabilities": protocol.PERSISTENT_CAPABILITIES,
        }
        for version, accepted in (("4.0.12", True), ("4.0.4", False)):
            for persistent in (False, True):
                output = _BinaryOutput()
                stdin = _BinaryInput(json.dumps(request).encode())
                with (
                    mock.patch.object(sys, "stdin", stdin),
                    mock.patch.object(sys, "__stdout__", output),
                    mock.patch.object(sys, "stderr", io.StringIO()),
                    mock.patch.dict(os.environ),
                    mock.patch.object(worker, "BundleWriter", return_value=object()),
                    mock.patch.object(worker.importlib.metadata, "version", return_value=version),
                    mock.patch.object(worker.importlib, "import_module", return_value=mock.Mock()),
                    mock.patch.object(worker.asyncio, "run", return_value=mock.Mock()),
                    mock.patch.object(protocol.importlib.metadata, "version", return_value=version),
                    mock.patch.object(protocol, "_persistent_read_frame", side_effect=[startup, None]),
                ):
                    if persistent:
                        protocol._persistent_main(lambda _path, _limit, **_kwargs: object())
                    else:
                        worker.main()
                response = json.loads(output.buffer.getvalue().decode().splitlines()[0])
                failed = response["type"] == "error" if persistent else response["status"] == "error"
                self.assertEqual(failed, not accepted)
                if not accepted:
                    self.assertIn("4.0.12", response["error"])


class RequestValidationTests(unittest.TestCase):
    def _request(self, **overrides: object) -> dict[str, object]:
        request: dict[str, object] = {
            "protocol": worker.PROTOCOL,
            "request_id": "validate-test",
            "tier": "standard",
            "ocr_mode": "auto",
            "image_analysis": False,
            "input_path": "input.pdf",
            "bundle_path": "bundle",
            "max_bundle_bytes": 1024,
            "bundle_name": worker.BUNDLE_NAME,
        }
        request.update(overrides)
        return request

    def test_tier_and_ocr_mode_domains_match_upstream(self) -> None:
        self.assertEqual(worker.PROTOCOL, "mineru-rs-official-worker/1")
        for tier in ("flash", "basic", "standard", "advanced"):
            protocol._validate_document_request(self._request(tier=tier))
        with self.assertRaises(ValueError):
            protocol._validate_document_request(self._request(tier="premium"))
        for ocr_mode in ("auto", "txt", "ocr"):
            protocol._validate_document_request(self._request(ocr_mode=ocr_mode))
        with self.assertRaises(ValueError):
            protocol._validate_document_request(self._request(ocr_mode="fast"))

    def test_model_base_dir_field_replaces_model_home(self) -> None:
        protocol._validate_document_request(self._request(model_base_dir="/models"))
        with self.assertRaises(ValueError):
            protocol._validate_document_request(self._request(model_home="/models"))

    def test_env_injection_uses_mineru_model_base_dir(self) -> None:
        # The model root is Config.model.base_dir, mapped by upstream
        # MINERU_MODEL_BASE_DIR; MINERU_HOME is a home directory, not this path.
        with mock.patch.dict(os.environ, {"MINERU_HOME": "/stale/home"}, clear=False):
            protocol._document_env(
                self._request(model_base_dir="/models/root", config="/cfg.yaml")
            )
            self.assertEqual(os.environ.get("MINERU_MODEL_BASE_DIR"), "/models/root")
            self.assertEqual(os.environ.get("MINERU_CONFIG"), "/cfg.yaml")
            self.assertEqual(os.environ.get("MINERU_HOME"), "/stale/home")  # untouched
            protocol._document_env(self._request())
            self.assertNotIn("MINERU_MODEL_BASE_DIR", os.environ)
            self.assertNotIn("MINERU_CONFIG", os.environ)

    def test_parse_kwargs_preserve_loaded_vlm_config(self) -> None:
        loaded = {
            "server_url": "",
            "api_key": "file-key",
            "model": "file-model",
            "engine": "vllm",
            "http_timeout": 333,
            "max_concurrency": 7,
        }

        class FakeVlm:
            def model_dump(self) -> dict[str, object]:
                return dict(loaded)

        class FakeVlmConfig:
            server_url: str
            api_key: str
            model: str
            engine: str
            http_timeout: int
            max_concurrency: int

            def __init__(self, **kwargs: object) -> None:
                merged = {**loaded, **kwargs}
                # Upstream normalizes a trailing /v1 off server_url.
                if str(merged["server_url"]).endswith("/v1"):
                    merged["server_url"] = str(merged["server_url"])[:-3] + "/"
                self.__dict__.update(merged)

        fake_config = mock.Mock()
        fake_config.model.vlm = FakeVlm()
        fake_module = mock.Mock(VlmConfig=FakeVlmConfig, config=fake_config)
        request = self._request(vlm_server_url="https://vlm.example.com/v1")
        with mock.patch.dict(sys.modules, {"mineru.config": fake_module}):
            kwargs: dict[str, object] = protocol._document_parse_kwargs(request)
        vlm = cast(FakeVlmConfig, kwargs["vlm_config"])
        # Upstream validation runs on the merged values.
        self.assertEqual(vlm.server_url, "https://vlm.example.com/")
        self.assertEqual(vlm.api_key, "file-key")
        self.assertEqual(vlm.model, "file-model")
        self.assertEqual(vlm.engine, "vllm")
        self.assertEqual(vlm.http_timeout, 333)
        self.assertEqual(vlm.max_concurrency, 7)


class PersistentFrameTests(unittest.TestCase):
    def test_persistent_capability_orders_preserve_sdk_and_legacy_contracts(self) -> None:
        self.assertEqual(
            protocol.PERSISTENT_CAPABILITIES["input_formats"],
            ["pdf", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tiff", "jp2", "doc", "docx", "ppt", "pptx", "xls", "xlsx", "rtf", "odt", "ods", "odp", "html", "htm", "shtml", "mhtml", "mht", "csv", "tsv", "epub", "ofd"],
        )
        self.assertEqual(
            protocol.PERSISTENT_LEGACY_CAPABILITIES["input_formats"],
            ["pdf", "png", "jpeg", "jpg", "jp2", "webp", "gif", "bmp", "tiff"],
        )

    def _startup(self, **overrides: object) -> dict[str, object]:
        startup: dict[str, object] = {
            "type": "start",
            "protocol": protocol.PERSISTENT_PROTOCOL,
            "package_version": protocol.PACKAGE_VERSION,
            "schema_version": protocol.SCHEMA_VERSION,
            "model_base_dir": "/models",
            "config": None,
            "vlm_api_key": None,
            "vlm_model": None,
            "capabilities": protocol.PERSISTENT_CAPABILITIES,
        }
        startup.update(overrides)
        return startup

    def _request(self, **overrides: object) -> dict[str, object]:
        request: dict[str, object] = {
            "type": "request",
            "protocol": protocol.PERSISTENT_PROTOCOL,
            "request_id": "persistent-1",
            "sequence": 1,
            "package_version": protocol.PACKAGE_VERSION,
            "schema_version": protocol.SCHEMA_VERSION,
            "bundle_name": protocol.BUNDLE_NAME,
            "input_path": "input.pdf",
            "bundle_path": "bundle",
            "max_bundle_bytes": 1024,
            "tier": "flash",
            "ocr_mode": "txt",
            "image_analysis": False,
        }
        request.update(overrides)
        return request

    def test_startup_frame_uses_model_base_dir(self) -> None:
        protocol._persistent_start(self._startup())
        legacy = self._startup(capabilities=protocol.PERSISTENT_LEGACY_CAPABILITIES)
        self.assertEqual(protocol._persistent_handshake(legacy)["capabilities"], protocol.PERSISTENT_LEGACY_CAPABILITIES)
        legacy = self._startup()
        del legacy["model_base_dir"]
        legacy["model_home"] = None
        with self.assertRaises(ValueError):
            protocol._persistent_start(legacy)

    def test_per_request_model_base_dir_and_config_must_match_startup(self) -> None:
        startup = self._startup()
        recent = protocol._PersistentRecentRequests()
        protocol._persistent_request(
            self._request(model_base_dir="/models"), startup, 1, recent
        )
        recent = protocol._PersistentRecentRequests()
        with self.assertRaises(ValueError):
            protocol._persistent_request(
                self._request(model_base_dir="/other"), startup, 1, recent
            )
        recent = protocol._PersistentRecentRequests()
        with self.assertRaises(ValueError):
            protocol._persistent_request(
                self._request(config="/other.yaml"), startup, 1, recent
            )
        recent = protocol._PersistentRecentRequests()
        # Omitting a field the startup frame set counts as a difference.
        with self.assertRaises(ValueError):
            protocol._persistent_request(self._request(), startup, 1, recent)
        recent = protocol._PersistentRecentRequests()
        protocol._persistent_request(
            self._request(model_base_dir="/models", config=None), startup, 1, recent
        )


if __name__ == "__main__":
    unittest.main()
