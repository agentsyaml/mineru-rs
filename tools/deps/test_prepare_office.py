import email.message
import hashlib
import io
import json
import os
import re
import socket
import runpy
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import unittest
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import archive_safe

DEPS = Path(__file__).resolve().parent
REPO = DEPS.parents[1]

def digest(data):
    return hashlib.sha256(data).hexdigest()


class Clock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def advance(self, amount):
        self.now += amount


def tar_bytes(root, members):
    target = io.BytesIO()
    with tarfile.open(fileobj=target, mode="w:gz") as archive:
        directory = tarfile.TarInfo(root + "/")
        directory.type = tarfile.DIRTYPE
        archive.addfile(directory)
        for name, payload, kind in members:
            info = tarfile.TarInfo(root + "/" + name)
            if kind == "dir":
                info.type = tarfile.DIRTYPE
                archive.addfile(info)
            elif kind == "symlink":
                info.type = tarfile.SYMTYPE
                info.linkname = payload.decode()
                archive.addfile(info)
            else:
                info.size = len(payload)
                archive.addfile(info, io.BytesIO(payload))
    return target.getvalue()


class Response(io.BytesIO):
    def __init__(self, payload, url, headers=None):
        super().__init__(payload)
        self.url = url
        self.headers = headers or {}
        self.socket = SimpleNamespace(timeouts=[])
        self.socket.settimeout = self.socket.timeouts.append
        self.fp = SimpleNamespace(fp=SimpleNamespace(raw=SimpleNamespace(_sock=self.socket)))

    def geturl(self):
        return self.url

    def read1(self, size=-1):
        return self.read(size)


class SlowDripResponse(Response):
    def __init__(self, payload, url, clock, delay):
        super().__init__(payload, url)
        self.clock = clock
        self.delay = delay

    def read1(self, size=-1):
        timeout = self.socket.timeouts[-1]
        if self.delay >= timeout:
            self.clock.advance(timeout)
            raise TimeoutError("simulated socket timeout")
        self.clock.advance(self.delay)
        return super().read1(min(size, 1))


class Fixture:
    def __init__(self, parent):
        self.root = Path(parent) / "repo"
        self.deps = self.root / "tools" / "deps"
        self.bundle = self.deps / "office-xml041"
        self.bundle.mkdir(parents=True)
        shutil.copy2(DEPS / "archive_safe.py", self.deps / "archive_safe.py")
        shutil.copy2(DEPS / "prepare_office.py", self.deps / "prepare_office.py")
        self.packages = {
            "office2pdf": ('name = "office2pdf"\nversion = "0.6.5"\nlicense = "Apache-2.0"\nversion = "0.38"\n', "0.6.5", "Apache-2.0"),
            "citationberg": ('name = "citationberg"\nversion = "0.6.1"\nlicense = "MIT OR Apache-2.0"\nversion = "0.38.1"\nfeatures = ["serialize", "overlapped-lists"]\n', "0.6.1", "MIT OR Apache-2.0"),
            "umya-spreadsheet": ('name = "umya-spreadsheet"\nversion = "2.3.3"\nlicense = "MIT"\nquick-xml = { version = "0.41", features = ["serialize"] }\n', "2.3.3", "MIT"),
        }
        self.archives = {}
        self.sources = []
        for name, (manifest, version, license) in self.packages.items():
            root_name = name + "-fixture"
            source = tar_bytes(root_name, [
                ("Cargo.toml", manifest.encode(), "file"),
                ("src/lib.rs", b"pub const VALUE: &str = \"before\";\n", "file"),
            ])
            url = "https://static.crates.io/crates/{}/fixture.crate".format(name)
            self.archives[url] = source
            self.sources.append({
                "package": name, "version": version, "license": license,
                "archive_name": name + ".crate", "archive_root": root_name, "url": url,
                "sha256": digest(source),
                "identity_fragments": ["name = \"{}\"".format(name), 'version = "{}"'.format(version), 'license = "{}"'.format(license)],
                "patched_fragments": ["name = \"{}\"".format(name), 'version = "{}"'.format(version), 'license = "{}"'.format(license)],
                "license_files": [],
            })
        self.sources[0]["patched_fragments"].append('version = "0.41"')
        self.sources[1]["patched_fragments"].extend(['version = "0.41.0"', '"serialize"', '"overlapped-lists"'])
        self._write_metadata()

    def _write_metadata(self):
        original = b'pub const VALUE: &str = "before";\n'
        updated = b'pub const VALUE: &str = "after";\n'
        replacements = [["VALUE: &str = \"before\"", "VALUE: &str = \"after\"", 1]]
        files = {
            "office2pdf/Cargo.toml": [digest(self.packages["office2pdf"][0].encode()), digest(self.packages["office2pdf"][0].replace('version = "0.38"', 'version = "0.41"').encode()), [["version = \"0.38\"", "version = \"0.41\"", 1]]],
            "citationberg/Cargo.toml": [digest(self.packages["citationberg"][0].encode()), digest(self.packages["citationberg"][0].replace('version = "0.38.1"', 'version = "0.41.0"').encode()), [["version = \"0.38.1\"", "version = \"0.41.0\"", 1]]],
            "office2pdf/src/lib.rs": [digest(original), digest(updated), replacements],
        }
        recipe = {"schema_version": 1, "files": files}
        recipe_bytes = (json.dumps(recipe, sort_keys=True, indent=2) + "\n").encode()
        (self.bundle / "edit_recipes.json").write_bytes(recipe_bytes)
        expected = self.root / "expected"
        for package, (manifest, version, license) in self.packages.items():
            path = expected / package
            (path / "src").mkdir(parents=True)
            if package == "office2pdf":
                manifest = manifest.replace('version = "0.38"', 'version = "0.41"')
            elif package == "citationberg":
                manifest = manifest.replace('version = "0.38.1"', 'version = "0.41.0"')
            # Byte-exact: text-mode writes would translate \n to \r\n on Windows
            # and diverge from the LF archives and recipe hashes.
            assert "\r" not in manifest
            (path / "Cargo.toml").write_bytes(manifest.encode("utf-8"))
            (path / "src/lib.rs").write_bytes(updated if package == "office2pdf" else original)
        (expected / ".source-executable-files.json").write_bytes(b"[]\n")
        sys.path.insert(0, str(self.deps))
        try:
            namespace = runpy.run_path(str(self.deps / "prepare_office.py"), run_name="fixture_digest")
            tree_hash = namespace["_tree_digest"](expected)
        finally:
            sys.path.remove(str(self.deps))
        sources = {"schema_version": 1, "tree_sha256": tree_hash, "sources": self.sources}
        source_bytes = (json.dumps(sources, sort_keys=True, indent=2) + "\n").encode()
        (self.bundle / "sources.json").write_bytes(source_bytes)
        script = self.deps / "prepare_office.py"
        text = script.read_text(encoding="utf-8")
        for pin, value in (("SOURCE_PIN", digest(source_bytes)), ("RECIPE_PIN", digest(recipe_bytes))):
            text = re.sub(
                r'^{} = .*$'.format(pin),
                '{} = "{}"'.format(pin, value),
                text, count=1, flags=re.MULTILINE,
            )
        script.write_text(text, encoding="utf-8")

    def module(self):
        sys.path.insert(0, str(self.deps))
        try:
            return runpy.run_path(str(self.deps / "prepare_office.py"), run_name="fixture_prepare")
        finally:
            sys.path.remove(str(self.deps))


class PrepareOfficeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.fixture = Fixture(self.temp.name)
        self.module = self.fixture.module()

    def tearDown(self):
        self.temp.cleanup()

    def opener(self, before_open=None):
        owner = self

        class FakeOpener:
            def open(self, request, timeout):
                if before_open:
                    before_open(request.full_url)
                return Response(owner.fixture.archives[request.full_url], request.full_url)

        return FakeOpener()

    def patch_network(self, before_open=None):
        return mock.patch("urllib.request.build_opener", return_value=self.opener(before_open))

    def test_fresh_prepare_is_noop_and_verify_is_offline(self):
        calls = []
        with self.patch_network(calls.append):
            self.module["prepare"]()
            self.module["prepare"]()
            self.module["verify"]()
        self.assertEqual(len(calls), 3)
        self.assertTrue(self.module["OUTPUT"].is_dir())
        self.assertFalse(self.module["LOCK"].exists())
        self.assertFalse(any(self.module["OUTPUT"].parent.glob(".office-xml041-stage-*")))

    def test_archive_and_recipe_tampering_fail_before_publish(self):
        url = self.fixture.sources[0]["url"]
        original = self.fixture.archives[url]
        self.fixture.archives[url] += b"tampered"
        with self.patch_network(), self.assertRaises(self.module["PreparationError"]):
            self.module["prepare"]()
        self.assertFalse(self.module["OUTPUT"].exists())
        self.assertFalse(self.module["LOCK"].exists())

        self.fixture.archives[url] = original
        recipes_path = self.fixture.bundle / "edit_recipes.json"
        recipes = json.loads(recipes_path.read_text(encoding="utf-8"))
        recipes["files"]["office2pdf/src/lib.rs"][2][0][2] = 9
        recipes_path.write_text(json.dumps(recipes), encoding="utf-8")
        with self.patch_network(), self.assertRaises(self.module["PreparationError"]):
            self.module["prepare"]()
        self.assertFalse(self.module["OUTPUT"].exists())
        self.assertFalse(self.module["LOCK"].exists())

    def test_tree_tampering_fails_without_replacing_it(self):
        with self.patch_network():
            self.module["prepare"]()
        victim = self.module["OUTPUT"] / "office2pdf" / "src" / "lib.rs"
        victim.write_bytes(b"tampered\n")
        with self.assertRaises(self.module["PreparationError"]):
            self.module["verify"]()
        with self.patch_network(), self.assertRaises(self.module["PreparationError"]):
            self.module["prepare"]()
        self.assertEqual(victim.read_bytes(), b"tampered\n")

    def test_concurrent_prepare_is_rejected(self):
        entered = threading.Event()
        release = threading.Event()
        failure = []

        def block(url):
            entered.set()
            if not release.wait(5):
                raise RuntimeError("test wait expired")

        def run_first():
            try:
                self.module["prepare"]()
            except BaseException as error:
                failure.append(error)

        with self.patch_network(block):
            worker = threading.Thread(target=run_first)
            worker.start()
            self.assertTrue(entered.wait(5))
            with self.assertRaises(self.module["PreparationError"]):
                self.module["prepare"]()
            release.set()
            worker.join(10)
        self.assertFalse(worker.is_alive())
        self.assertEqual(failure, [])
        self.assertTrue(self.module["OUTPUT"].is_dir())

    def test_interruption_cleans_only_owned_staging_and_lock(self):
        class InterruptingOpener:
            def open(self, request, timeout):
                raise KeyboardInterrupt()

        with mock.patch("urllib.request.build_opener", return_value=InterruptingOpener()):
            with self.assertRaises(KeyboardInterrupt):
                self.module["prepare"]()
        self.assertFalse(self.module["LOCK"].exists())
        self.assertFalse(self.module["OUTPUT"].exists())
        self.assertFalse(any(self.module["OUTPUT"].parent.glob(".office-xml041-stage-*")))

    def test_malicious_tar_paths_links_collisions_and_conflicts_are_rejected(self):
        hostile = [
            [("../escape", b"x", "file")],
            [("CON.txt", b"x", "file")],
            [("link", b"../../outside", "symlink")],
            [("Folder/a", b"x", "file"), ("folder/b", b"y", "file")],
            [("same", b"x", "file"), ("same", b"y", "file")],
            [("parent", b"x", "file"), ("parent/child", b"y", "file")],
        ]
        with tempfile.TemporaryDirectory() as directory:
            for index, members in enumerate(hostile):
                archive_path = Path(directory) / "bad-{}.tar.gz".format(index)
                destination = Path(directory) / "out-{}".format(index)
                archive_path.write_bytes(tar_bytes("root", members))
                with self.assertRaises(archive_safe.ArchiveError):
                    archive_safe.extract_tar_gz(archive_path, destination, "root")
                self.assertFalse(destination.exists())

    def test_archive_file_size_limit_is_enforced(self):
        with tempfile.TemporaryDirectory() as directory:
            archive_path = Path(directory) / "large.tar.gz"
            destination = Path(directory) / "out"
            archive_path.write_bytes(tar_bytes("root", [("large", b"xx", "file")]))
            with mock.patch.object(archive_safe, "MAX_FILE", 1):
                with self.assertRaises(archive_safe.ArchiveError):
                    archive_safe.extract_tar_gz(archive_path, destination, "root")
            self.assertFalse(destination.exists())

    def test_https_proxy_authentication_is_never_forwarded(self):
        with mock.patch.dict(os.environ, {"https_proxy": "http://user:secret@proxy.test:8080"}):
            self.assertEqual(self.module["_proxy_without_credentials"](), {"https": "http://proxy.test:8080"})

    def test_pinned_json_and_exec_manifest_keep_lf_with_autocrlf(self):
        repo = self.fixture.root
        shutil.copy2(REPO / ".gitattributes", repo / ".gitattributes")

        def git(*args):
            return subprocess.run(
                ["git", "-C", str(repo), "-c", "core.autocrlf=true", *args],
                check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            ).stdout

        git("-c", "init.defaultBranch=main", "init", "--quiet")
        relative = [
            ".gitattributes",
            "tools/deps/office-xml041/sources.json",
            "tools/deps/office-xml041/edit_recipes.json",
        ]
        git("add", *relative)
        git("-c", "user.name=Bootstrap Test", "-c",
            "user.email=bootstrap@example.invalid", "commit", "-m", "fixture", "--quiet")
        for name in relative[1:]:
            committed = git("show", "HEAD:" + name)
            self.assertNotIn(b"\r", committed)
            path = repo / name
            path.unlink()
            git("checkout", "--", name)
            self.assertEqual(path.read_bytes(), committed)

        with tempfile.TemporaryDirectory() as directory:
            stage = Path(directory) / "stage"
            stage.mkdir()
            self.module["_write_exec_manifest"](stage, set())
            self.assertEqual((stage / ".source-executable-files.json").read_bytes(), b"[]\n")

    def test_deadline_is_shared_across_approved_redirects(self):
        # runpy returns a dict COPY; functions still bind to their original globals.
        module = self.module
        source = self.fixture.sources[0]
        clock = Clock()
        redirected = "https://codeload.github.com/example/source.tar.gz"
        payload = self.fixture.archives[source["url"]]
        calls = []

        class Opener:
            def open(self, request, timeout):
                calls.append((request.full_url, timeout))
                if len(calls) == 1:
                    clock.advance(10)
                    raise module["_ApprovedRedirect"](redirected)
                return Response(payload, redirected)

        destination = Path(self.temp.name) / "download"
        with mock.patch.dict(module["_download"].__globals__, {"MAX_DOWNLOAD_SECONDS": 15}):
            with mock.patch("urllib.request.build_opener", return_value=Opener()):
                with mock.patch.object(module["time"], "monotonic", clock):
                    module["_download"](source, destination, clock)
        self.assertEqual([call[1] for call in calls], [15, 5])
        self.assertEqual(destination.read_bytes(), payload)

        response = Response(b"", source["url"])
        request = module["urllib"].request.Request(source["url"])
        with self.assertRaises(module["_ApprovedRedirect"]):
            module["_PinnedRedirect"]().redirect_request(
                request, response, 302, "Found", {}, redirected
            )
        self.assertTrue(response.closed)

    def test_slow_drip_timeout_cleans_only_owned_prepare_state(self):
        source = self.fixture.sources[0]
        clock = Clock()
        payload = self.fixture.archives[source["url"]]

        class Opener:
            def open(self, request, timeout):
                return SlowDripResponse(payload, request.full_url, clock, 10)

        with mock.patch.dict(self.module["_download"].__globals__, {"MAX_DOWNLOAD_SECONDS": 15}):
            with mock.patch("urllib.request.build_opener", return_value=Opener()):
                with mock.patch.object(self.module["time"], "monotonic", clock):
                    with self.assertRaises(self.module["PreparationError"]):
                        self.module["prepare"]()
        self.assertFalse(self.module["OUTPUT"].exists())
        self.assertFalse(self.module["LOCK"].exists())
        self.assertFalse(any(self.module["OUTPUT"].parent.glob(".office-xml041-stage-*")))

    def test_response_deadline_adjusts_a_real_socket(self):
        client, server = socket.socketpair()
        clock = Clock()
        server.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx")
        response = archive_safe._DeadlineHTTPResponse(client, "GET", 30, clock)
        try:
            response.begin()
            stream = getattr(response, "fp", response)
            raw = getattr(stream, "raw", None) or getattr(getattr(stream, "fp", None), "raw", None)
            sock = getattr(raw, "_sock")
            sock.settimeout(3)
            self.assertEqual(sock.gettimeout(), 3)
            self.assertEqual(response.read1(1), b"x")
        finally:
            response.close()
            client.close()
            server.close()

    def test_download_stops_reading_socket_after_content_length(self):
        # Newer Python closes the response socket once Content-Length is exactly
        # consumed; the loop must not call settimeout on the dead descriptor
        # (EBADF) and must exit through the length-based break instead.
        payload = b"x" * 5
        response = Response(payload, "https://static.crates.io/x", {"Content-Length": str(len(payload))})
        state = {"eof_closing": False}
        original_read = response.read1

        def fake_read1(size=-1):
            block = original_read(size)
            state["eof_closing"] = bool(block) and response.tell() >= len(payload)
            return block

        response.read1 = fake_read1  # type: ignore[method-assign]
        source = {"url": response.url, "sha256": digest(payload)}
        destination = Path(self.temp.name) / "capped-download"
        module_ns = self.module["_download"].__globals__
        live_original = module_ns["_response_socket"]
        module_ns["_response_socket"] = lambda resp: resp.socket

        class Opener:
            def open(self, request, timeout):
                return response

        try:
            with mock.patch("urllib.request.build_opener", return_value=Opener()):
                self.module["_download"](source, destination, Clock())
        finally:
            module_ns["_response_socket"] = live_original
        self.assertEqual(destination.read_bytes(), payload)
        self.assertTrue(state["eof_closing"])

    def _https_open_captures_connection_kwargs(self, handler_class):
        # do_open consumes one response; the stand-in below satisfies its reads.
        captured = {}
        ok = SimpleNamespace(reason="OK", version=11, status=200, code=200, msg="OK",
                             will_close=True, isclosed=lambda: False,
                             read=lambda *a: b"", readline=lambda *a: b"",
                             readinto=lambda *a: 0, info=email.message.Message(),
                             close=lambda: None)

        class Recorder:
            def __init__(self, *args, **kwargs):
                captured["args"] = args
                captured["kwargs"] = kwargs
                self.sock = None

            def request(self, *args, **kwargs):
                pass

            def set_debuglevel(self, level):
                pass

            def close(self):
                pass

            def getresponse(self):
                return ok

        request = urllib.request.Request("https://static.crates.io/x")
        request.timeout = 30
        handler = handler_class(60, Clock())
        # runpy namespaces are dict copies; patch the live globals functions bind to.
        live = handler_class.https_open.__globals__
        with mock.patch.dict(live, {"_DeadlineHTTPSConnection": Recorder}):
            handler.https_open(request)
        self.assertEqual(captured["args"][0], "static.crates.io")
        self.assertIn("context", captured["kwargs"])
        self.assertNotIn(
            "check_hostname",
            captured["kwargs"],
            "stale do_open kwarg rejected by Python >= 3.12",
        )

    def test_https_open_forwards_only_context_to_the_connection(self):
        # http.client.HTTPSConnection lost the check_hostname kwarg in
        # Python 3.12; do_open must not forward it anymore.
        self._https_open_captures_connection_kwargs(self.module["_DeadlineHTTPSHandler"])

    def test_https_open_forwards_only_context_in_archive_safe(self):
        self._https_open_captures_connection_kwargs(archive_safe._DeadlineHTTPSHandler)
