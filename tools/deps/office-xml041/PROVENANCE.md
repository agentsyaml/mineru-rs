# Pinned Office XML dependency sources

Use these instructions when changing the local Office dependency inputs. The
bundle preserves upstream package identities; it does not publish or claim
ownership of a fork. `prepare_office.py` verifies pinned metadata, archive
digests, byte-edit recipes, and the complete generated-tree digest before
materializing sources under the ignored `.deps/office-xml041/` directory.

From the repository root, Python 3.9+ is required. Preparation may download the
two immutable HTTPS archives; verification is offline:

```sh
python3 tools/deps/prepare_office.py prepare
python3 tools/deps/prepare_office.py verify
python3 -m unittest discover -s tools/deps -p 'test_*.py'
```

After changing a source pin or edit recipe, update its pinned digest in
`prepare_office.py`, the expected generated-tree digest in `sources.json`, and
the corresponding source description below. Regenerate from a clean ignored
`.deps/office-xml041/` tree, run the commands above, then review the exact
upstream and adapted bytes. Never commit generated sources.

| Package | Original identity/license | Immutable source | SHA-256 |
| --- | --- | --- | --- |
| citationberg 0.7.0 | MIT OR Apache-2.0 | https://static.crates.io/crates/citationberg/citationberg-0.7.0.crate | `756ff1e3d43a9ecc8183932fb4d9fd3971236f3ce4acb62fe51d1cd43297547d` |
| anydoc 0.2.4 | MIT | https://static.crates.io/crates/anydoc/anydoc-0.2.4.crate | `cf0d78e4cfe3654eb3ea04422ec5a94352000ee56e97902125e68e51f7527062` |

`sources.json` and `edit_recipes.json` are integrity-pinned by
`prepare_office.py`; the metadata pins are constants in that script. The
citationberg archive preserves both license texts; the anydoc archive preserves
its MIT `LICENSE`. The original manifests are checked for package name, version,
and license before edits; required license files are checked as well.

The recipes in `edit_recipes.json` require exact input hashes and replacement
counts, then verify exact output hashes. They preserve package versions:
citationberg raises its quick-xml requirement from 0.38.1 to 0.41.0 while
retaining `serialize` and `overlapped-lists`; anydoc raises its zip requirement
from 8.6.0 to 9.7.0 while retaining the `deflate` feature. Cargo.lock registry
checksums for the two crate archives match the archive hashes above.

Only these two forks remain. office2pdf 0.9.1+ publishes its own
`office2pdf-docx-rs` and `office2pdf-umya-spreadsheet` forks, so `docx-rs` and
`umya-spreadsheet` no longer need local patches; office2pdf 0.11.0 already
requires quick-xml 0.41 and zip 9.0.5, so it needs no patch either. anydoc 0.2.4
dropped `calamine`, so the `calamine` patch is gone as well. The two remaining
edits exist because a published crate still declares an older dependency than
the workspace pins: citationberg 0.7.0 requires quick-xml 0.38.1, below the
0.41.0 that clears RUSTSEC-2026-0194/0195, and anydoc 0.2.4 requires zip 8.6.0,
below the 9.7.0 that clears RUSTSEC-2026-0367/0368/0369.

The root workspace patches point to these local generated sources. A Cargo
consumer does not inherit the root workspace's `[patch.crates-io]`; this local
repair is therefore not evidence that a published crate would use the same
dependency chain. See [release instructions](../../../docs/releasing.md) for
the publication boundary.