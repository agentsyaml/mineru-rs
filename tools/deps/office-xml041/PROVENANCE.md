# Pinned Office XML sources

This bundle keeps the published crate identities; it does not publish or claim
ownership of any fork. The three source archives and every byte adaptation are
checked by `sources.json` and `edit_recipes.json` before materialization.

| Package | Original identity/license | Immutable source | SHA-256 |
| --- | --- | --- | --- |
| office2pdf 0.6.5 | Apache-2.0 | https://static.crates.io/crates/office2pdf/office2pdf-0.6.5.crate | `103138077535e6e1db8c5f73ca2d2a75b9dc027288d8a94abf034a72f8dfd2a6` |
| citationberg 0.6.1 | MIT OR Apache-2.0 | https://static.crates.io/crates/citationberg/citationberg-0.6.1.crate | `1f6597e8bdbca37f1f56e5a80d15857b0932aead21a78d20de49e99e74933046` |
| umya-spreadsheet 2.3.3 | MIT | https://codeload.github.com/developer0hye/umya-spreadsheet/tar.gz/3e001dcbdbf2c490e93724af6f502560a3d7d766 | `a9fc92e72c55d8245ab90a2efb55b7d1e677a909dce55be21500acaa21946006` |

The umya archive is the complete repository snapshot at commit
`3e001dcbdbf2c490e93724af6f502560a3d7d766`, not a hand-written manifest or a
moving branch. Its Cargo package remains 2.3.3 and its source already requests
genuine quick-xml 0.41 with `serialize`. The repository snapshot preserves its
MIT `LICENSE`. The citationberg archive preserves both license texts. The
office2pdf crate archive declares Apache-2.0 in its manifest and contains no
separate license text file.

The exact byte edits preserve the verified POC adaptations: office2pdf moves
to quick-xml 0.41 attribute/text APIs and its manifest constraint; citationberg
changes only its quick-xml requirement, retaining `serialize` and
`overlapped-lists`. No package version is changed. Cargo.lock crate checksums
for the two registry archives match their archive hashes above.

The quick-xml 0.41 attribute migration removes all 50 deprecated attribute
calls across 16 office2pdf parser files. UTF-8 calls decode as UTF-8 and unescape
without XML attribute whitespace normalization; decoder-aware calls use the
actual reader decoder and the same unescape behavior. Borrowed values and
decoding/entity errors are preserved. XML 1.0 text-content end-of-line handling
remains explicit; unrelated GeneralRef text-event loops are unchanged.

Generated sources live only in ignored `.deps/office-xml041/`. They are local
workspace patch inputs, not inherited by a consumer and not publication proof.
