import asyncio
import sys
import tempfile
import types
import unittest
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
        self.original_helper = mineru_rs._helper_path
        mineru_rs._helper_path = lambda: "/nonexistent/mineru-office-convert"

    def tearDown(self):
        mineru_rs._native._run = self.original_run
        mineru_rs._helper_path = self.original_helper

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


if __name__ == "__main__":
    unittest.main()
