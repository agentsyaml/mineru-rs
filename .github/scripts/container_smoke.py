#!/usr/bin/env python3
"""Hermetic Docker API smoke test using only the Python standard library."""

import argparse
import http.client
import io
import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import uuid
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def request(port, method, path, body=None, headers=None):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    connection.request(method, path, body=body, headers=headers or {})
    response = connection.getresponse()
    payload = response.read()
    connection.close()
    return response.status, payload


def json_request(port, method, path, body=None, headers=None):
    status, payload = request(port, method, path, body, headers)
    return status, json.loads(payload)


def multipart_pdf(pdf):
    boundary = "----mineru-smoke-" + uuid.uuid4().hex
    fields = [
        ("backend", "vlm-http-client"),
        ("formula_enable", "false"),
        ("table_enable", "false"),
        ("image_analysis", "false"),
        ("response_format_zip", "true"),
    ]
    chunks = []
    for name, value in fields:
        chunks.extend((
            f"--{boundary}\r\n".encode(),
            f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode(),
            value.encode(), b"\r\n",
        ))
    chunks.extend((
        f"--{boundary}\r\n".encode(),
        b'Content-Disposition: form-data; name="files"; filename="minimal.pdf"\r\n',
        b"Content-Type: application/pdf\r\n\r\n", pdf.read_bytes(), b"\r\n",
        f"--{boundary}--\r\n".encode(),
    ))
    return b"".join(chunks), {"Content-Type": f"multipart/form-data; boundary={boundary}"}


def validate_zip(payload):
    with zipfile.ZipFile(io.BytesIO(payload)) as archive:
        names = archive.namelist()
        check(names, "result ZIP is empty")
        for name in names:
            path = PurePosixPath(name)
            check(not path.is_absolute() and ".." not in path.parts, f"unsafe ZIP entry: {name!r}")


class Mock:
    def __init__(self):
        self.requests = []
        self.server = ThreadingHTTPServer(("0.0.0.0", 0), self.handler())
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def handler(self):
        mock = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                mock.record(self)
                if self.path == "/v1/models":
                    self.reply({"data": [{"id": "smoke-model"}]})
                else:
                    self.send_error(404)

            def do_POST(self):
                mock.record(self)
                if self.path == "/v1/chat/completions":
                    self.reply({"choices": [{"finish_reason": "stop", "message": {"content": "<|box_start|>0 0 1000 1000<|box_end|><|ref_start|>text<|ref_end|><|rotate_up|>"}}]})
                else:
                    self.send_error(404)

            def log_message(self, format, *args):
                pass

            def reply(self, value):
                body = json.dumps(value).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        return Handler

    def record(self, handler):
        length = int(handler.headers.get("Content-Length", "0"))
        handler.rfile.read(length)
        self.requests.append((handler.path, handler.headers.get("Host"), handler.headers.get("Authorization")))

    @property
    def port(self):
        return self.server.server_address[1]

    def start(self):
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def container_logs(container):
    result = subprocess.run(["docker", "logs", container], text=True, capture_output=True)
    return (result.stdout + result.stderr).strip()


def docker_runtime():
    try:
        result = subprocess.run(["docker", "info"], text=True, capture_output=True, timeout=10)
    except (FileNotFoundError, OSError) as error:
        raise AssertionError(f"Docker runtime is unavailable: {error}") from error
    except subprocess.TimeoutExpired as error:
        raise AssertionError("Docker runtime did not respond to `docker info` within 10 seconds") from error
    detail = (result.stderr or result.stdout).strip()
    check(result.returncode == 0, f"Docker runtime is unavailable: {detail or 'docker info failed'}")


def image_config(image):
    try:
        result = subprocess.run(
            ["docker", "image", "inspect", image], check=True, text=True, capture_output=True,
        )
    except (FileNotFoundError, OSError) as error:
        raise AssertionError(f"Docker image inspection is unavailable: {error}") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout).strip()
        raise AssertionError(f"Docker image {image!r} cannot be inspected: {detail}") from error
    return json.loads(result.stdout)[0]["Config"]


def assert_non_root_configured_user(config):
    configured_user = str(config.get("User", "")).strip()
    check(configured_user, "image has no configured non-root USER")
    user = configured_user.split(":", 1)[0].strip()
    check(user and user.lower() != "root", f"image USER is root: {configured_user!r}")
    if user.isdigit():
        check(int(user) != 0, f"image USER is root: {configured_user!r}")


def runtime_uid(container):
    try:
        result = subprocess.run(
            ["docker", "exec", container, "id", "-u"], check=True, text=True, capture_output=True,
        )
    except (FileNotFoundError, OSError) as error:
        raise AssertionError(f"cannot inspect the container runtime UID: {error}") from error
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout).strip()
        raise AssertionError(f"cannot inspect the container runtime UID: {detail}") from error
    value = result.stdout.strip()
    check(value.isdigit() and int(value) != 0, f"container runtime UID is root or invalid: {value!r}")


def run(image, pdf):
    container = "mineru-container-smoke-" + uuid.uuid4().hex[:12]
    output = Path(tempfile.mkdtemp(prefix="mineru-container-output-"))
    mock = Mock()
    port = free_port()
    mock_started = False
    try:
        docker_runtime()
        assert_non_root_configured_user(image_config(image))
        os.chmod(output, 0o777)
        mock.start()
        mock_started = True
        try:
            subprocess.run([
                "docker", "run", "--detach", "--name", container,
                "--network", "bridge",
                "--add-host", "host.docker.internal:host-gateway",
                "--publish", f"127.0.0.1:{port}:8000",
                "--volume", f"{output}:/app/output",
                "--env", f"MINERU_VL_SERVER=http://host.docker.internal:{mock.port}",
                "--env", "MINERU_VL_MODEL_NAME=smoke-model",
                "--env", "MINERU_VL_API_KEY=smoke-key",
                "--env", "MINERU_API_PUBLIC_BIND_EXPOSED=true",
                "--env", "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT=true", image,
            ], check=True, text=True, capture_output=True)
        except (FileNotFoundError, OSError) as error:
            raise AssertionError(f"Docker could not start the bridge-network smoke container: {error}") from error
        except subprocess.CalledProcessError as error:
            detail = (error.stderr or error.stdout).strip()
            raise AssertionError(
                f"Docker could not start the bridge-network smoke container (host-gateway may be unavailable): {detail}"
            ) from error
        deadline = time.monotonic() + 30
        while True:
            try:
                status, health = json_request(port, "GET", "/health")
                if status == 200 and health.get("status") == "healthy":
                    break
            except (ConnectionError, OSError, http.client.HTTPException, json.JSONDecodeError):
                pass
            if time.monotonic() >= deadline:
                raise AssertionError(f"API health did not become ready: {container_logs(container)}")
            time.sleep(0.25)
        runtime_uid(container)
        running = subprocess.run(
            ["docker", "inspect", "--format", "{{.State.Running}}", container],
            check=True, text=True, capture_output=True,
        ).stdout.strip()
        check(running == "true", "container exited after reporting healthy")
        mounts = json.loads(subprocess.run(
            ["docker", "inspect", container], check=True, text=True, capture_output=True,
        ).stdout)[0]["Mounts"]
        check(any(m["Destination"] == "/app/output" and os.path.realpath(m["Source"]) == os.path.realpath(output) for m in mounts), "output is not mounted at exact /app/output")
        body, headers = multipart_pdf(pdf)
        status, created = json_request(port, "POST", "/tasks", body, headers)
        check(status == 202, f"task creation returned {status}: {created}")
        task_id = created["task_id"]
        deadline = time.monotonic() + 60
        while True:
            status, task = json_request(port, "GET", f"/tasks/{task_id}")
            check(status == 200, f"task poll returned {status}: {task}")
            if task["status"] == "completed":
                break
            check(task["status"] not in {"failed", "cancelled"}, f"task failed: {task}")
            check(time.monotonic() < deadline, f"task timed out: {task}")
            time.sleep(0.25)
        status, result = request(port, "GET", f"/tasks/{task_id}/result")
        check(status == 200, f"result download returned {status}")
        validate_zip(result)
        check(any(output.iterdir()), "no output persisted through /app/output mount")
        expected_host = f"host.docker.internal:{mock.port}"
        check(mock.requests, "mock provider received no requests")
        check(any(path == "/v1/models" for path, _, _ in mock.requests), "model discovery was not exercised")
        check(any(path == "/v1/chat/completions" for path, _, _ in mock.requests), "completion was not exercised")
        for path, host, authorization in mock.requests:
            check(path in {"/v1/models", "/v1/chat/completions"} and host == expected_host, f"provider request escaped mock origin: {path!r}, {host!r}")
            check(authorization == "Bearer smoke-key", f"provider Authorization missing for {path}")
    finally:
        try:
            subprocess.run(["docker", "rm", "--force", "--volumes", container], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError:
            pass
        if mock_started:
            mock.close()
        shutil.rmtree(output, ignore_errors=True)


def self_check():
    assert_non_root_configured_user({"User": "mineru"})
    for config in ({}, {"User": "root"}, {"User": "0:0"}):
        try:
            assert_non_root_configured_user(config)
        except AssertionError:
            continue
        raise AssertionError(f"non-root image user self-check accepted {config!r}")
    valid = io.BytesIO()
    with zipfile.ZipFile(valid, "w") as archive:
        archive.writestr("result/document.md", "ok")
    validate_zip(valid.getvalue())
    invalid = io.BytesIO()
    with zipfile.ZipFile(invalid, "w") as archive:
        archive.writestr("../escape", "no")
    try:
        validate_zip(invalid.getvalue())
    except AssertionError:
        return
    raise AssertionError("ZIP traversal self-check did not fail")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--image")
    parser.add_argument("--pdf", type=Path)
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args()
    if args.self_check:
        self_check()
    else:
        check(args.image and args.pdf and args.pdf.is_file(), "--image and an existing --pdf are required")
        run(args.image, args.pdf)
