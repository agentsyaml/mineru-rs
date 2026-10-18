import json
import subprocess
import sys
import unittest
from pathlib import Path


WORKER_DIR = str(Path(__file__).parent)


class WorkerEntryImportTests(unittest.TestCase):
    def run_isolated(self, body: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-S", "-c", body],
            env={"PYTHONPATH": WORKER_DIR},
            capture_output=True,
            text=True,
            check=False,
        )

    def test_entry_and_bundle_import_without_docvortex(self) -> None:
        result = self.run_isolated("import mineru_official_worker; import mineru_official_worker_bundle")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_wrong_package_pin_fails_before_heavy_import(self) -> None:
        result = self.run_isolated(r'''
import importlib.metadata, io, json, sys
from types import SimpleNamespace
from unittest.mock import patch
import mineru_official_worker as worker
request = {"protocol": worker.PROTOCOL, "request_id": "pin-test", "max_bundle_bytes": 1,
           "bundle_name": worker.BUNDLE_NAME, "input_path": "/input", "bundle_path": "/bundle",
           "tier": "standard", "ocr_mode": "auto", "image_analysis": False}
out = io.BytesIO()
worker.sys.stdin = SimpleNamespace(buffer=io.BytesIO(json.dumps(request).encode()))
worker.sys.__stdout__ = SimpleNamespace(buffer=out, flush=lambda: None)
with patch.object(importlib.metadata, "version", return_value="3.4.5"), \
     patch.object(worker.importlib, "import_module", side_effect=AssertionError("heavy import before pin")):
    worker.main()
response = json.loads(out.getvalue())
assert response["status"] == "error"
assert response["package_version"] == "3.4.5"
assert "package version is not 4.0.12" in response["error"]
''')
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
