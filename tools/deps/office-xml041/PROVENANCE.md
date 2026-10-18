# Pinned Office XML dependency sources

Use these instructions when changing the local Office dependency inputs. The
bundle preserves upstream package identities; it does not publish or claim
ownership of a fork. `prepare_office.py` verifies pinned metadata, archive
digests, byte-edit recipes, and the complete generated-tree digest before
materializing sources under the ignored `.deps/office-xml041/` directory.

From the repository root, Python 3.9+ is required. Preparation may download the
six immutable HTTPS archives; verification is offline:

```sh
python3 tools/deps/prepare_office.py prepare
python3 tools/deps/prepare_office.py verify
python3 -m unittest discover -s tools/deps -p 'test_*.py'
```

After changing a source pin or edit recipe, update its pinned digest in
`prepare_office.py`, the expected generated-tree digest in `sources.json`, and
the corresponding source description below. Regenerate from a clean ignored
`.deps/office-xml041/` tree, run the commands above and
`python3 tools/deps/office_xml_parity.py`, then review the exact upstream and
adapted bytes. Never commit generated sources.

| Package | Original identity/license | Immutable source | SHA-256 |
| --- | --- | --- | --- |
| office2pdf 0.6.5 | Apache-2.0 | https://static.crates.io/crates/office2pdf/office2pdf-0.6.5.crate | `103138077535e6e1db8c5f73ca2d2a75b9dc027288d8a94abf034a72f8dfd2a6` |
| citationberg 0.6.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/citationberg/citationberg-0.6.1.crate | `1f6597e8bdbca37f1f56e5a80d15857b0932aead21a78d20de49e99e74933046` |
| umya-spreadsheet 2.3.3 | MIT | https://codeload.github.com/developer0hye/umya-spreadsheet/tar.gz/3e001dcbdbf2c490e93724af6f502560a3d7d766 | `a9fc92e72c55d8245ab90a2efb55b7d1e677a909dce55be21500acaa21946006` |
| anydoc 0.1.9 | MIT | https://static.crates.io/crates/anydoc/anydoc-0.1.9.crate | `93a8b0dbecf79be9329111093b493d256db2a573ed3f1cc827c96bdbbe32a936` |
| calamine 0.36.1 | MIT | https://static.crates.io/crates/calamine/calamine-0.36.1.crate | `5fa68281b1a76b54a62156474adb06bb380a67e07dd60656e3217152b42183f3` |
| docx-rs 0.4.22 | MIT | https://static.crates.io/crates/docx-rs/docx-rs-0.4.22.crate | `7fdf00e8af6d0b3e92d4bbf9b76f773d8b84ea80f310324ad16cbdc2e653e02c` |

`sources.json` and `edit_recipes.json` are integrity-pinned by
`prepare_office.py`; the metadata pins are constants in that script. The umya
archive is the complete repository snapshot at commit
`3e001dcbdbf2c490e93724af6f502560a3d7d766`, not a hand-written manifest or a
moving branch. The original manifests are checked for package name, version,
and license before edits; required license files are checked as well. It keeps
its Cargo package at 2.3.3, requests registry quick-xml 0.41 with `serialize`,
and preserves its MIT `LICENSE`. The citationberg archive preserves both
license texts. The office2pdf archive declares Apache-2.0 in its manifest and
contains no separate license text file.

The recipes in `edit_recipes.json` require exact input hashes and replacement
counts, then verify exact output hashes. They preserve package versions:
office2pdf updates its quick-xml 0.41 manifest constraint and parser API calls;
citationberg changes only its quick-xml requirement, retaining `serialize` and
`overlapped-lists`. Cargo.lock registry checksums for the two crate archives
match the archive hashes above.

The quick-xml 0.41 migration updates 50 deprecated attribute calls across 16
office2pdf parser files. UTF-8 attributes decode as UTF-8 and unescape without
XML attribute whitespace normalization; decoder-aware calls use the reader's
actual decoder. Borrowed values and decoding/entity errors are preserved. XML
1.0 text-content end-of-line handling remains explicit; unrelated GeneralRef
text-event loops are unchanged.

The zip 9 migration updates anydoc, calamine, docx-rs, office2pdf, and umya
manifests to zip 9.0.5. Its recipes update office2pdf's FileOptions uses in
source, tests, and benchmarks, and give umya's reader driver the new ZipFile
reader type parameter and skip entries with invalid UTF-8 names. Calamine
skips invalid UTF-8 names while building its ZIP-path cache. The bounded
extractor accepts GNU longname headers with the same path, member-count, and
expanded-size validation as ordinary headers.

The root workspace patches point to these local generated sources. A Cargo
consumer does not inherit the root workspace's `[patch.crates-io]`; this local
repair is therefore not evidence that a published crate would use the same
dependency chain. See [release instructions](../../../docs/releasing.md) for
the publication boundary.
