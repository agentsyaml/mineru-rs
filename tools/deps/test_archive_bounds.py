import io
import tarfile
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

import archive_safe


class ReadSpy:
    def __init__(self, payload=b""):
        self.payload = payload
        self.read_sizes = []

    def read(self, size=-1):
        self.read_sizes.append(size)
        if size < 0:
            size = len(self.payload)
        return self.payload[:size].ljust(size, b"\0")


def pax_record(key, value):
    body = key + b"=" + value + b"\n"
    length = len(body) + 2
    while True:
        record = str(length).encode("ascii") + b" " + body
        if len(record) == length:
            return record
        length = len(record)


def fake_archive(stream, *, members=(), extra_members=0, extra_bytes=0, expanded=0):
    return SimpleNamespace(
        fileobj=stream,
        members=list(members),
        extra_members=extra_members,
        extra_bytes=extra_bytes,
        expanded_bytes=expanded,
    )


class ArchiveBoundTests(unittest.TestCase):
    def test_gnu_longname_longlink_and_sparse_reject_before_payload_reads(self):
        for kind in (tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK, tarfile.GNUTYPE_SPARSE):
            stream = ReadSpy(b"untrusted payload")
            member = archive_safe._BoundedTarInfo()
            member.type = kind
            member.size = 1_000_000
            with self.assertRaises(archive_safe.ArchiveError):
                member._proc_member(fake_archive(stream))
            self.assertEqual(stream.read_sizes, [])

    def test_pax_member_budget_rejects_before_read(self):
        stream = ReadSpy(b"5 x=\n")
        member = archive_safe._BoundedTarInfo()
        member.type = tarfile.XHDTYPE
        member.size = 5
        archive = fake_archive(stream, extra_members=archive_safe.MAX_MEMBERS - 1)
        with self.assertRaises(archive_safe.ArchiveError):
            member._proc_pax(archive)
        self.assertEqual(stream.read_sizes, [])

    def test_pax_metadata_and_payload_share_one_expansion_budget(self):
        stream = ReadSpy(b"5 x=\n")
        member = archive_safe._BoundedTarInfo()
        member.size = 5
        archive = fake_archive(stream, expanded=archive_safe.MAX_EXPANDED - 4)
        with self.assertRaises(archive_safe.ArchiveError):
            member._proc_pax(archive)
        self.assertEqual(stream.read_sizes, [])

    def test_pax_sparse_maps_and_excess_records_are_rejected_bounded(self):
        sparse = pax_record(b"GNU.sparse.map", b"0,1")
        stream = ReadSpy(sparse)
        member = archive_safe._BoundedTarInfo()
        member.size = len(sparse)
        with self.assertRaises(archive_safe.ArchiveError):
            member._proc_pax(fake_archive(stream))
        self.assertLessEqual(max(stream.read_sizes), tarfile.BLOCKSIZE)

        with self.assertRaises(archive_safe.ArchiveError):
            archive_safe._validate_pax_payload(
                b"5 x=\n" * (archive_safe.MAX_MEMBERS + 1),
                5 * (archive_safe.MAX_MEMBERS + 1),
            )

    def test_pax_long_path_archive_remains_supported(self):
        with tempfile.TemporaryDirectory() as directory:
            archive_path = Path(directory) / "pax.tar.gz"
            output = Path(directory) / "out"
            with tarfile.open(archive_path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
                root = tarfile.TarInfo("root/")
                root.type = tarfile.DIRTYPE
                archive.addfile(root)
                member = tarfile.TarInfo("root/" + "a" * 120)
                member.size = 1
                archive.addfile(member, io.BytesIO(b"x"))
            archive_safe.extract_tar_gz(archive_path, output, "root")
            self.assertEqual((output / ("a" * 120)).read_bytes(), b"x")


if __name__ == "__main__":
    unittest.main()
