import gzip
import hashlib
import io
import json
import os
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import source_snapshot as snapshot


class SourceSnapshotTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.git("init", "-q")
        (self.root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.2.3"\n')
        (self.root / "source.txt").write_bytes(b"committed\n")
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
        self.commit = self.git("rev-parse", "HEAD").decode().strip()
        self.output = Path(self.temp.name) / "evidence"
        snapshot.snapshot_create(self.root, "commit", self.commit, "1.2.3", self.output)
        snapshot.record_evidence(self.output, self.commit, "1.2.3")

    def tearDown(self):
        self.temp.cleanup()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, check=True, stdout=subprocess.PIPE).stdout

    def test_commit_evidence_uses_git_blobs_not_worktree(self):
        (self.root / "source.txt").write_bytes(b"modified worktree\n")
        snapshot.verify_evidence(self.output, self.commit, "1.2.3", self.root)

    def test_resealed_changed_archive_does_not_authenticate_itself(self):
        archive_path = self.output / "source-snapshot.tar.gz"
        with tarfile.open(archive_path, "r:gz") as source:
            members = []
            for member in source:
                if member.isfile():
                    payload = source.extractfile(member)
                    if payload is None:
                        self.fail("archive file payload missing")
                    members.append((member.name, payload.read()))
        changed = [(name, b"altered\n" if name == "source/source.txt" else data) for name, data in members]
        with archive_path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as zipped:
            with tarfile.open(fileobj=zipped, mode="w|", format=tarfile.PAX_FORMAT) as target:
                for name, content in changed:
                    member = tarfile.TarInfo(name)
                    member.mode, member.size = 0o644, len(content)
                    target.addfile(member, io.BytesIO(content))
        manifest_path = self.output / "source-snapshot.json"
        manifest = json.loads(manifest_path.read_text())
        item = next(item for item in manifest["files"] if item["path"] == "source.txt")
        item["size"], item["sha256"] = len(b"altered\n"), hashlib.sha256(b"altered\n").hexdigest()
        manifest["archive_sha256"] = snapshot.hash_file(archive_path)
        manifest_path.write_text(json.dumps(manifest))
        evidence_path = self.output / "source-build-evidence.json"
        evidence = json.loads(evidence_path.read_text())
        evidence["archive_sha256"] = manifest["archive_sha256"]
        evidence_path.write_text(json.dumps(evidence))
        with self.assertRaises(ValueError):
            snapshot.verify_evidence(self.output, self.commit, "1.2.3", self.root)

    def test_relabelled_working_tree_snapshot_is_rejected(self):
        # A clean working tree is byte-identical to its commit, so relabelling it carries
        # no information a verifier can distinguish. The meaningful case is a dirty tree:
        # the relabelled snapshot must fail the Git-blob comparison.
        (self.root / "source.txt").write_bytes(b"dirty worktree\n")
        local = Path(self.temp.name) / "local"
        snapshot.snapshot_create(self.root, "working-tree", self.commit, "1.2.3", local)
        (local / "source-snapshot.json").write_text(
            (local / "source-snapshot.json").read_text().replace('"working-tree"', '"commit"')
        )
        with self.assertRaises(ValueError):
            snapshot.snapshot_verify(local, self.commit, "1.2.3", source_root=self.root)

    def test_existing_extract_destination_survives_failure(self):
        dest = Path(self.temp.name) / "existing"
        dest.mkdir()
        sentinel = dest / "sentinel"
        sentinel.write_bytes(b"keep")
        with self.assertRaises((OSError, ValueError)):
            snapshot.snapshot_verify(self.output, self.commit, "1.2.3", dest)
        self.assertEqual(sentinel.read_bytes(), b"keep")

    def test_occupied_publication_destination_is_never_removed(self):
        stage = Path(self.temp.name) / "stage"
        stage.mkdir()
        dest = Path(self.temp.name) / "raced"
        dest.mkdir()
        (dest / "sentinel").write_bytes(b"keep")
        with self.assertRaises(OSError):
            snapshot.publish_directory(stage, dest)
        self.assertEqual((dest / "sentinel").read_bytes(), b"keep")
        self.assertTrue(stage.is_dir())

    def test_extract_staging_shares_the_destination_filesystem(self):
        # The exclusive atomic rename cannot cross filesystems (EXDEV), so the
        # private staging directory must live beside the destination.
        observed = []
        real_publish = snapshot.publish_directory

        def watch(stage, destination):
            observed.append((Path(stage), Path(destination)))
            real_publish(stage, destination)

        destination = Path(self.temp.name) / "extracted-staging"
        with mock.patch.object(snapshot, "publish_directory", watch):
            manifest = snapshot.snapshot_verify(self.output, self.commit, "1.2.3", destination)
        self.assertEqual(manifest["source_mode"], "commit")
        self.assertEqual(len(observed), 1)
        stage, published = observed[0]
        self.assertEqual(published, destination)
        # Staging must be a sibling on the destination's own filesystem; the
        # owned staging directory is cleaned up after the publish rename, so
        # observe st_dev through the capture itself.
        self.assertEqual(stage.parent.parent, destination.parent)
        self.assertIn("source-snapshot-stage-", stage.parent.name)
        self.assertTrue((destination / "source.txt").is_file())
        self.assertEqual((destination / "source.txt").read_bytes(), b"committed\n")

    def test_omitted_added_or_mode_changed_manifest_entries_are_rejected(self):
        path = self.output / "source-snapshot.json"
        original = json.loads(path.read_text())
        for mutate in (
            lambda files: files.pop(),
            lambda files: files.append(dict(files[0], path="added.txt")),
            lambda files: files[0].update(mode="100755" if files[0]["mode"] == "100644" else "100644"),
        ):
            altered = json.loads(json.dumps(original))
            mutate(altered["files"])
            path.write_text(json.dumps(altered))
            with self.assertRaises(ValueError):
                snapshot.verify_evidence(self.output, self.commit, "1.2.3", self.root)
        path.write_text(json.dumps(original))


if __name__ == "__main__":
    unittest.main()
