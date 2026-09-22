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
