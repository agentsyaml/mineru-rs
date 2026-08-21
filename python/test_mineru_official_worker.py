import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import mineru_official_worker as worker  # noqa: E402


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
            writer._rewrite_text_bytes("content_list.json", b"{}")

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
        class Result:
            def save(self, target: object) -> None:
                sys.stdout.write("save stdout\n" + "s" * 20_000)
                sys.stderr.write("save stderr\n" + "t" * 20_000)
                getattr(target, "write_string")("markdown.md", "ok")

        class FakeParser:
            async def parse_async(self, _input: str, **_kwargs: object) -> Result:
                sys.stdout.write("parse stdout\n" + "p" * 20_000)
                sys.stderr.write("parse stderr\n" + "q" * 20_000)
                return Result()

        fake_parser = FakeParser()
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        request = {
            "protocol": worker.PROTOCOL,
            "request_id": "diagnostic-test",
            "bundle_name": worker.BUNDLE_NAME,
            "backend": "hybrid-http-client",
            "effort": "medium",
            "server_url": None,
            "method": "auto",
            "lang": "en",
            "image_analysis": False,
            "input_path": "input.pdf",
            "bundle_path": str(Path(directory.name) / "bundle"),
            "max_bundle_bytes": 1024,
        }
        output = _BinaryOutput()
        stderr = io.StringIO()

        def fake_import(name: str) -> object:
            return fake_parser if name == "mineru.parser" else object()

        with (
            mock.patch.object(sys, "stdin", _BinaryInput(json.dumps(request).encode())),
            mock.patch.object(sys, "__stdout__", output),
            mock.patch.object(sys, "stderr", stderr),
            mock.patch.object(worker.importlib.metadata, "version", return_value=worker.PACKAGE_VERSION),
            mock.patch.object(worker.importlib, "import_module", side_effect=fake_import),
        ):
            worker.main()

        response = json.loads(output.buffer.getvalue().decode().strip())
        diagnostic = response["diagnostic"]
        self.assertEqual(response["status"], "ok")
        self.assertIn("parse stdout", diagnostic)
        self.assertIn("parse stderr", diagnostic)
        self.assertIn("save stdout", diagnostic)
        self.assertIn("save stderr", diagnostic)
        self.assertLessEqual(len(diagnostic.encode()), worker.DIAGNOSTIC_CAP)
        self.assertLessEqual(len(stderr.getvalue().encode()), worker.DIAGNOSTIC_CAP)
        self.assertLessEqual(len(output.buffer.getvalue()), worker.PROTOCOL_CAP)


if __name__ == "__main__":
    unittest.main()
