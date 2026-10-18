"""Bounded, link-free extraction for pinned dependency archives."""

import http.client
import io
import os
import re
import shutil
import tarfile
import urllib.request
from contextlib import contextmanager
from pathlib import Path


MAX_DOWNLOAD = 64 * 1024 * 1024
MAX_EXPANDED = 512 * 1024 * 1024
MAX_MEMBERS = 20_000
MAX_FILE = 32 * 1024 * 1024
MAX_PATH = 1024
MAX_COMPONENT = 255
MAX_METADATA_DEPTH = 32
_DEVICE = re.compile(r"^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?$", re.I)


class ArchiveError(ValueError):
    """The archive violates the fixed extraction limits or path policy."""


class _DeadlineSocketReader(io.RawIOBase):
    def __init__(self, source, sock, deadline, clock):
        self._source = source
        self._sock = sock
        self._deadline = deadline
        self._clock = clock

    def readable(self):
        return True

    def readinto(self, buffer):
        remaining = self._deadline - self._clock()
        if remaining <= 0:
            raise TimeoutError("archive download timed out")
        self._sock.settimeout(remaining)
        return self._source.readinto(buffer)

    def close(self):
        try:
            self._source.close()
        finally:
            super().close()


class _DeadlineHTTPResponse(http.client.HTTPResponse):
    def __init__(self, sock, method, deadline, clock):
        super().__init__(sock, method=method)
        self.fp.close()
        raw = sock.makefile("rb", buffering=0)
        self.fp = io.BufferedReader(_DeadlineSocketReader(raw, sock, deadline, clock))


class _DeadlineHTTPConnection(http.client.HTTPConnection):
    def __init__(self, host, deadline, clock, **kwargs):
        super().__init__(host, **kwargs)
        setattr(
            self,
            "response_class",
            lambda sock, method=None: _DeadlineHTTPResponse(sock, method, deadline, clock),
        )


class _DeadlineHTTPSConnection(http.client.HTTPSConnection):
    def __init__(self, host, deadline, clock, **kwargs):
        super().__init__(host, **kwargs)
        setattr(
            self,
            "response_class",
            lambda sock, method=None: _DeadlineHTTPResponse(sock, method, deadline, clock),
        )


class _DeadlineHTTPHandler(urllib.request.HTTPHandler):
    def __init__(self, deadline, clock):
        super().__init__()
        self.deadline = deadline
        self.clock = clock

    def http_open(self, req):
        connection = lambda host, **kwargs: _DeadlineHTTPConnection(
            host, self.deadline, self.clock, **kwargs
        )
        return self.do_open(connection, req)


class _DeadlineHTTPSHandler(urllib.request.HTTPSHandler):
    def __init__(self, deadline, clock):
        super().__init__()
        self.deadline = deadline
        self.clock = clock

    def https_open(self, req):
        connection = lambda host, **kwargs: _DeadlineHTTPSConnection(
            host, self.deadline, self.clock, **kwargs
        )
        # check_hostname was removed from HTTPSConnection in Python 3.12; the
        # validating context still verifies hostnames.
        return self.do_open(
            connection,
            req,
            context=getattr(self, "_context", None),
        )


class _BoundedTarFile(tarfile.TarFile):
    def __init__(self, *args, **kwargs):
        self.extra_members = 0
        self.extra_bytes = 0
        self.expanded_bytes = 0
        self.metadata_depth = 0
        super().__init__(*args, **kwargs)


@contextmanager
def _metadata_header(archive):
    archive.metadata_depth += 1
    try:
        if archive.metadata_depth > MAX_METADATA_DEPTH:
            raise ArchiveError("archive metadata nesting limit exceeded")
        yield
    finally:
        archive.metadata_depth -= 1


class _ReplayReader:
    def __init__(self, source, prefix):
        self._source = source
        self._prefix = prefix
        self._position = 0

    def read(self, size=-1):
        remaining = len(self._prefix) - self._position
        if size is None or size < 0:
            first = self._prefix[self._position:]
            self._position = len(self._prefix)
            return first + self._source.read()
        if not remaining or size == 0:
            return self._source.read(size)
        amount = min(size, remaining)
        start = self._position
        self._position += amount
        first = self._prefix[start:self._position]
        if amount == size:
            return self._prefix if start == 0 and amount == len(self._prefix) else first
        return first + self._source.read(size - amount)

    def tell(self):
        return self._source.tell() - (len(self._prefix) - self._position)

    def __getattr__(self, name):
        return getattr(self._source, name)


def _validate_pax_payload(payload, size):
    position = 0
    records = 0
    while position < size and payload[position] != 0:
        separator = payload.find(b" ", position, size)
        prefix = payload[position:separator] if separator >= 0 else b""
        if (not prefix or len(prefix) > 10 or not prefix.isdigit()):
            raise ArchiveError("invalid PAX record length")
        length = int(prefix)
        end = position + length
        if length < 5 or end > size or payload[end - 1] != 0x0A:
            raise ArchiveError("invalid PAX record")
        key_start = separator + 1
        equals = payload.find(b"=", key_start, end - 1)
        if equals <= key_start:
            raise ArchiveError("invalid PAX record")
        if payload.startswith(b"GNU.sparse.", key_start, equals):
            raise ArchiveError("GNU sparse PAX extensions are forbidden")
        records += 1
        if records > MAX_MEMBERS:
            raise ArchiveError("PAX record count limit exceeded")
        position = end


class _BoundedTarInfo(tarfile.TarInfo):
    def _proc_member(self, archive):
        if self.type == tarfile.GNUTYPE_LONGNAME:
            with _metadata_header(archive):
                if self.size < 1 or self.size > MAX_PATH + 1:
                    raise ArchiveError("GNU longname exceeds path limit")
                archive.extra_members += 1
                archive.extra_bytes += self.size
                if (archive.extra_members > MAX_MEMBERS or archive.extra_bytes > MAX_EXPANDED
                        or len(archive.members) + archive.extra_members + 1 > MAX_MEMBERS
                        or archive.expanded_bytes + self.size > MAX_EXPANDED):
                    raise ArchiveError("archive metadata limit exceeded")
                archive.expanded_bytes += self.size
                return self._proc_gnulong(archive)
        if self.type in (tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE):
            raise ArchiveError("GNU longname and sparse extensions are forbidden")
        if self.type in (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.SOLARIS_XHDTYPE):
            return self._proc_pax(archive)
        processor = getattr(tarfile.TarInfo, "_proc_builtin", None)
        if processor is None:
            raise ArchiveError("unsupported tar member processor")
        return processor(self, archive)

    def _proc_pax(self, archive):
        with _metadata_header(archive):
            return self._proc_pax_bounded(archive)

    def _proc_pax_bounded(self, archive):
        archive.extra_members += 1
        archive.extra_bytes += self.size
        if (self.size < 0 or self.size > MAX_FILE
                or archive.extra_members > MAX_MEMBERS
                or archive.extra_bytes > MAX_EXPANDED):
            raise ArchiveError("archive metadata limit exceeded")
        if (len(archive.members) + archive.extra_members + 1 > MAX_MEMBERS
                or archive.expanded_bytes + self.size > MAX_EXPANDED):
            raise ArchiveError("archive combined member or size limit exceeded")
        block_size = (self.size + tarfile.BLOCKSIZE - 1) // tarfile.BLOCKSIZE
        block_size *= tarfile.BLOCKSIZE
        payload = archive.fileobj.read(block_size)
        if len(payload) != block_size:
            raise ArchiveError("truncated PAX header")
        _validate_pax_payload(payload, self.size)
        archive.expanded_bytes += self.size
        original = archive.fileobj
        archive.fileobj = _ReplayReader(original, payload)
        processor = getattr(tarfile.TarInfo, "_proc_pax", None)
        if processor is None:
            raise ArchiveError("unsupported tar metadata format")
        try:
            return processor(self, archive)
        finally:
            archive.fileobj = original


def _parts(name):
    if not name or "\\" in name or name.startswith("/") or "\x00" in name:
        raise ArchiveError("unsafe archive path")
    if len(name.encode("utf-8", "strict")) > MAX_PATH:
        raise ArchiveError("archive path exceeds limit")
    trimmed = name[:-1] if name.endswith("/") else name
    parts = trimmed.split("/")
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise ArchiveError("unsafe archive path")
    for part in parts:
        if (len(part.encode("utf-8", "strict")) > MAX_COMPONENT
                or part.endswith((".", " "))
                or _DEVICE.fullmatch(part)
                or any(ord(ch) < 32 or ch in '<>:"|?*' for ch in part)):
            raise ArchiveError("Windows-unsafe archive path")
    if re.match(r"^[A-Za-z]:", parts[0]):
        raise ArchiveError("drive-qualified archive path")
    return parts


def validate_relative_path(name):
    """Validate one unrooted portable path using extraction path rules."""
    return _parts(name)


def extract_tar_gz(archive_path, destination, expected_root):
    """Extract one pinned top-level tree without following archive links."""
    archive_path = Path(archive_path)
    destination = Path(destination)
    if destination.exists():
        raise ArchiveError("extraction destination already exists")
    destination.mkdir(parents=True)
    nodes = {"": "dir"}
    spellings = {"": ""}
    explicit = set()
    executable = set()
    member_count = 0
    root_seen = False
    try:
        with _BoundedTarFile.open(
            archive_path, mode="r:gz", tarinfo=_BoundedTarInfo
        ) as archive:
            for member in archive:
                member_count += 1
                if member_count + archive.extra_members > MAX_MEMBERS:
                    raise ArchiveError("archive member limit exceeded")
                parts = _parts(member.name)
                if parts[0] != expected_root:
                    raise ArchiveError("unexpected archive root")
                if len(parts) == 1:
                    if root_seen or not member.isdir():
                        raise ArchiveError("archive root is not a directory")
                    root_seen = True
                    continue
                relative = parts[1:]
                path = "/".join(relative)
                if path in explicit:
                    raise ArchiveError("duplicate archive member")
                explicit.add(path)
                is_dir = member.isdir()
                if not is_dir and not member.isfile():
                    raise ArchiveError("links and special archive members are forbidden")
                if member.isfile():
                    if member.size < 0 or member.size > MAX_FILE:
                        raise ArchiveError("archive file size limit exceeded")
                    archive.expanded_bytes += member.size
                    if archive.expanded_bytes > MAX_EXPANDED:
                        raise ArchiveError("expanded archive limit exceeded")
                for index in range(1, len(parts)):
                    subpath = "/".join(parts[1:index + 1])
                    canonical = subpath.casefold()
                    prior = spellings.get(canonical)
                    if prior is not None and prior != subpath:
                        raise ArchiveError("case-colliding archive paths")
                    spellings[canonical] = subpath
                    want = ("dir" if index < len(parts) - 1 or is_dir
                            else "file")
                    have = nodes.get(subpath)
                    if have is not None and have != want:
                        raise ArchiveError("archive file/directory conflict")
                    nodes[subpath] = want
                target = destination.joinpath(*relative)
                if is_dir:
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
                if hasattr(os, "O_NOFOLLOW"):
                    flags |= os.O_NOFOLLOW
                fd = os.open(str(target), flags, 0o600)
                source = archive.extractfile(member)
                if source is None:
                    os.close(fd)
                    raise ArchiveError("archive file payload is missing")
                copied = 0
                with source, os.fdopen(fd, "wb") as output:
                    while copied < member.size:
                        block = source.read(min(64 * 1024, member.size - copied))
                        if not block:
                            raise ArchiveError("truncated archive payload")
                        output.write(block)
                        copied += len(block)
                os.chmod(str(target), 0o644 | (0o111 if member.mode & 0o111 else 0))
                if member.mode & 0o111:
                    executable.add(path)
        if not any(member.is_file() for member in destination.rglob("*")):
            raise ArchiveError("archive contains no files")
    except Exception:
        shutil.rmtree(destination, ignore_errors=True)
        raise
    return executable
