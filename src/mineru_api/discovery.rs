use super::{InputDocument, RemoteOptions, classifier::Classifier, ooxml, planning};
use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};

pub(super) fn discover(path: &Path, options: &RemoteOptions) -> Result<Vec<InputDocument>, String> {
    // Constructing this first deliberately keeps unsupported targets fail-before-IO.
    let mut classifier = Classifier::new()?;
    discover_with(
        path,
        options,
        ooxml::detect,
        |path| classifier.identify_path(path),
        probe_pdf,
    )
}

fn discover_with<Office, Classify, Probe>(
    path: &Path,
    options: &RemoteOptions,
    mut office: Office,
    mut classify: Classify,
    mut probe: Probe,
) -> Result<Vec<InputDocument>, String>
where
    Office: FnMut(&Path) -> Result<Option<&'static str>, String>,
    Classify: FnMut(&Path) -> Result<String, String>,
    Probe: FnMut(&Path) -> Result<usize, String>,
{
    let candidates = if path.is_dir() {
        let mut paths = fs::read_dir(path)
            .map_err(|_| "cannot enumerate input directory")?
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|_| "cannot enumerate input directory")
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();
        paths.into_iter().filter(|path| path.is_file()).collect()
    } else {
        vec![path.to_path_buf()]
    };

    let mut documents = Vec::new();
    for (order, path) in candidates.into_iter().enumerate() {
        let mut suffix = match office(&path)? {
            Some(suffix) => suffix.to_owned(),
            None => classify(&path)?,
        };
        if matches!(suffix.as_str(), "ai" | "html")
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
            && has_pdf_signature(&path)
        {
            suffix = "pdf".into();
        }
        if !matches!(
            suffix.as_str(),
            "pdf"
                | "png"
                | "jpeg"
                | "jp2"
                | "webp"
                | "gif"
                | "bmp"
                | "jpg"
                | "tiff"
                | "docx"
                | "pptx"
                | "xlsx"
        ) {
            continue;
        }
        let effective_pages = if suffix == "pdf" {
            planning::selected_pages(probe(&path)?, options.start, options.end)?
        } else {
            1
        };
        documents.push(InputDocument {
            stem: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path,
            suffix,
            effective_pages,
            order,
        });
    }
    if documents.is_empty() {
        return Err("no supported documents found".into());
    }
    let stems = planning::unique_stems(
        &documents
            .iter()
            .map(|document| document.stem.clone())
            .collect::<Vec<_>>(),
    );
    for (document, stem) in documents.iter_mut().zip(stems) {
        document.stem = stem;
    }
    Ok(documents)
}

fn has_pdf_signature(path: &Path) -> bool {
    let mut first = [0; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut first))
        .is_ok_and(|()| first == *b"%PDF")
}

fn probe_pdf(path: &Path) -> Result<usize, String> {
    let document = lopdf::Document::load(path).map_err(|_| "cannot read PDF")?;
    if document.is_encrypted() {
        return Err("PDF is encrypted".into());
    }
    let count =
        usize::try_from(document.get_pages().len()).map_err(|_| "PDF page count is too large")?;
    if count == 0 {
        return Err("PDF has no pages".into());
    }
    Ok(count)
}

fn pages_for_pdf(
    path: &Path,
    options: &RemoteOptions,
    probe: &mut impl FnMut(&Path) -> Result<usize, String>,
) -> Result<usize, String> {
    planning::selected_pages(probe(path)?, options.start, options.end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn raw_pdf(pages: usize, encrypted: bool) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize];
        let mut object = |body: String| {
            let number = offsets.len();
            offsets.push(out.len());
            out.extend(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
        };
        object("<< /Type /Catalog /Pages 2 0 R >>".into());
        let kids = (0..pages)
            .map(|n| format!("{} 0 R", n + 3))
            .collect::<Vec<_>>()
            .join(" ");
        object(format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>"));
        for _ in 0..pages {
            object("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 1 1] >>".into());
        }
        if encrypted {
            object("<< /Filter /Standard /V 1 /R 2 /O () /U () /P -4 >>".into());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
        for offset in offsets.iter().skip(1) {
            out.extend(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R{} >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len(),
                if encrypted {
                    format!(" /Encrypt {} 0 R", pages + 3)
                } else {
                    String::new()
                }
            )
            .as_bytes(),
        );
        out
    }

    fn nested_pages_pdf() -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize];
        let mut object = |body: &str| {
            let number = offsets.len();
            offsets.push(out.len());
            out.extend(format!("{number} 0 obj\n{body}\nendobj\n").as_bytes());
        };
        object("<< /Type /Catalog /Pages 2 0 R >>");
        object("<< /Type /Pages /Kids [3 0 R] /Count 2 /MediaBox [0 0 1 1] >>");
        object("<< /Type /Pages /Parent 2 0 R /Kids [4 0 R 5 0 R] /Count 2 >>");
        object("<< /Type /Page /Parent 3 0 R >>");
        object("<< /Type /Page /Parent 3 0 R >>");
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len()).as_bytes());
        for offset in offsets.iter().skip(1) {
            out.extend(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                offsets.len()
            )
            .as_bytes(),
        );
        out
    }

    fn file_with(name: &str, bytes: &[u8]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(name), bytes).unwrap();
        dir
    }

    fn run(
        path: &Path,
        labels: &[(&str, &str)],
        options: RemoteOptions,
    ) -> Result<Vec<InputDocument>, String> {
        let labels: HashMap<_, _> = labels.iter().map(|(name, label)| (*name, *label)).collect();
        discover_with(
            path,
            &options,
            |_| Ok(None),
            |path| {
                Ok(labels
                    .get(path.file_name().unwrap().to_str().unwrap())
                    .unwrap_or(&"txt")
                    .to_string())
            },
            |_| Ok(3),
        )
    }

    #[test]
    fn directory_is_one_level_sorted_with_orders_and_stems() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("z.png"), b"").unwrap();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.JPG"), b"").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/in.png"), b"").unwrap();
        let docs = run(
            dir.path(),
            &[("z.png", "png"), ("b.JPG", "jpg")],
            RemoteOptions::default(),
        )
        .unwrap();
        assert_eq!(
            docs.iter()
                .map(|d| (
                    d.path.file_name().unwrap().to_str().unwrap(),
                    d.order,
                    d.stem.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![("b.JPG", 1, "b"), ("z.png", 2, "z")]
        );
    }

    #[test]
    fn office_bypasses_classifier_and_pdf_ranges_are_inclusive() {
        let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        let options = RemoteOptions {
            start: 2,
            end: Some(u64::MAX),
            ..Default::default()
        };
        let docs = discover_with(
            file.path(),
            &options,
            |_| Ok(Some("docx")),
            |_| panic!("classifier called"),
            |_| panic!("probe called"),
        )
        .unwrap();
        assert_eq!(
            (docs[0].suffix.as_str(), docs[0].effective_pages),
            ("docx", 1)
        );
        let docs = discover_with(
            file.path(),
            &options,
            |_| Ok(None),
            |_| Ok("pdf".into()),
            |_| Ok(3),
        )
        .unwrap();
        assert_eq!(docs[0].effective_pages, 1);
        assert!(
            discover_with(
                file.path(),
                &RemoteOptions {
                    start: 3,
                    ..Default::default()
                },
                |_| Ok(None),
                |_| Ok("pdf".into()),
                |_| Ok(3)
            )
            .is_err()
        );
    }

    #[test]
    fn rescues_only_ai_or_html_pdf_signatures() {
        let pdf = tempfile::Builder::new().suffix(".PDF").tempfile().unwrap();
        fs::write(pdf.path(), b"%PDF-x").unwrap();
        let docs = discover_with(
            pdf.path(),
            &RemoteOptions::default(),
            |_| Ok(None),
            |_| Ok("html".into()),
            |_| Ok(1),
        )
        .unwrap();
        assert_eq!(docs[0].suffix, "pdf");
        let bad = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        fs::write(bad.path(), b"nope").unwrap();
        assert!(
            discover_with(
                bad.path(),
                &RemoteOptions::default(),
                |_| Ok(None),
                |_| Ok("ai".into()),
                |_| Ok(1)
            )
            .is_err()
        );
    }

    #[test]
    fn supported_labels_preserve_paths_and_stems_are_reserved() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.png", "A.JPG", "a_2.gif", "office.bin"] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        let docs = discover_with(
            dir.path(),
            &RemoteOptions::default(),
            |path| Ok((path.file_name().unwrap() == "office.bin").then_some("xlsx")),
            |path| {
                Ok(match path.file_name().unwrap().to_str().unwrap() {
                    "a.png" => "png",
                    "A.JPG" => "jpg",
                    "a_2.gif" => "gif",
                    _ => "txt",
                }
                .into())
            },
            |_| Ok(99),
        )
        .unwrap();
        assert_eq!(
            docs.iter()
                .map(|d| (d.suffix.as_str(), d.stem.as_str(), d.effective_pages))
                .collect::<Vec<_>>(),
            vec![
                ("jpg", "A", 1),
                ("png", "a_3", 1),
                ("gif", "a_2", 1),
                ("xlsx", "office", 1)
            ]
        );
        assert_eq!(docs[1].path.file_name().unwrap(), "a.png");
    }

    #[test]
    fn all_twelve_supported_labels_are_exact_and_non_pdf_ranges_are_ignored() {
        let labels = [
            "pdf", "png", "jpeg", "jp2", "webp", "gif", "bmp", "jpg", "tiff", "docx", "pptx",
            "xlsx",
        ];
        assert_eq!(labels.len(), 12);
        for label in labels {
            let dir = file_with(&format!("original.{label}"), b"x");
            let path = dir.path().join(format!("original.{label}"));
            let docs = discover_with(
                &path,
                &RemoteOptions {
                    start: 99,
                    end: Some(100),
                    ..Default::default()
                },
                |_| Ok(None),
                |_| Ok(label.into()),
                |_| Ok(101),
            );
            let doc = &docs.unwrap()[0];
            assert_eq!(
                (doc.suffix.as_str(), doc.path.as_path(), doc.effective_pages),
                (label, path.as_path(), if label == "pdf" { 2 } else { 1 })
            );
        }
    }

    #[test]
    fn direct_file_classifier_errors_and_order_gaps_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.skip", "b.keep"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let docs = run(dir.path(), &[("b.keep", "png")], RemoteOptions::default()).unwrap();
        assert_eq!(docs[0].order, 1);
        assert_eq!(
            run(
                &dir.path().join("b.keep"),
                &[("b.keep", "png")],
                RemoteOptions::default()
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            discover_with(
                &dir.path().join("missing"),
                &RemoteOptions::default(),
                |_| Ok(None),
                |_| Err("classify error".into()),
                |_| Ok(1)
            ),
            Err("classify error".into())
        );
    }

    #[test]
    fn discovery_stems_casefold_in_document_order_without_unsupported_gaps() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["STRASSE.png", "Straße.jpg", "ignored.txt"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let docs = run(
            dir.path(),
            &[("STRASSE.png", "png"), ("Straße.jpg", "jpg")],
            RemoteOptions::default(),
        )
        .unwrap();
        assert_eq!(
            docs.iter().map(|doc| doc.stem.as_str()).collect::<Vec<_>>(),
            vec!["STRASSE", "Straße_2"]
        );
    }

    #[test]
    fn ooxml_and_classifier_seams_short_circuit_exactly() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            discover_with(
                file.path(),
                &RemoteOptions::default(),
                |_| Ok(Some("pptx")),
                |_| panic!("classifier"),
                |_| panic!("pdf")
            )
            .is_ok()
        );
        assert_eq!(
            discover_with(
                file.path(),
                &RemoteOptions::default(),
                |_| Err("ooxml error".into()),
                |_| panic!("classifier"),
                |_| panic!("pdf")
            ),
            Err("ooxml error".into())
        );
        assert_eq!(
            discover_with(
                file.path(),
                &RemoteOptions::default(),
                |_| Ok(None),
                |_| Err("classifier error".into()),
                |_| Ok(1)
            ),
            Err("classifier error".into())
        );
    }

    #[test]
    fn rescue_matrix_is_exact() {
        for label in ["ai", "html"] {
            for extension in ["pdf", "PDF"] {
                let dir = file_with(&format!("x.{extension}"), b"%PDF");
                let docs = discover_with(
                    &dir.path().join(format!("x.{extension}")),
                    &RemoteOptions::default(),
                    |_| Ok(None),
                    |_| Ok(label.into()),
                    |_| Ok(1),
                )
                .unwrap();
                assert_eq!(docs[0].suffix, "pdf");
            }
        }
        for (name, label, bytes) in [
            ("x.pdf", "ai", b"bad".as_slice()),
            ("x.txt", "html", b"%PDF"),
            ("x.pdf", "txt", b"%PDF"),
            ("x.pdf", "ai", b"%P"),
        ] {
            let dir = file_with(name, bytes);
            assert_eq!(
                discover_with(
                    &dir.path().join(name),
                    &RemoteOptions::default(),
                    |_| Ok(None),
                    |_| Ok(label.into()),
                    |_| Ok(1)
                ),
                Err("no supported documents found".into())
            );
        }
    }

    #[test]
    fn detector_errors_stop_and_unsupported_inputs_fail() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            discover_with(
                file.path(),
                &RemoteOptions::default(),
                |_| Err("office error".into()),
                |_| Ok("png".into()),
                |_| Ok(1),
            ),
            Err("office error".into())
        );
        assert_eq!(
            discover_with(
                file.path(),
                &RemoteOptions::default(),
                |_| Ok(None),
                |_| Ok("txt".into()),
                |_| Ok(1),
            ),
            Err("no supported documents found".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_keeps_symlinks_to_files() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("target"), b"").unwrap();
        symlink(dir.path().join("target"), dir.path().join("linked")).unwrap();
        let docs = run(
            dir.path(),
            &[("target", "png"), ("linked", "jpg")],
            RemoteOptions::default(),
        )
        .unwrap();
        assert_eq!(docs.len(), 2);
    }

    #[test]
    fn pdf_probe_rejects_malformed_and_counts_fixture() {
        for (name, pages, encrypted, expected) in [
            ("one.pdf", 1, false, Ok(1)),
            ("three.pdf", 3, false, Ok(3)),
            ("encrypted.pdf", 1, true, Err("PDF is encrypted")),
            ("zero.pdf", 0, false, Err("PDF has no pages")),
        ] {
            let dir = file_with(name, &raw_pdf(pages, encrypted));
            assert_eq!(
                probe_pdf(&dir.path().join(name)),
                expected.map_err(str::to_owned)
            );
        }
        let bad = file_with("bad.pdf", b"%PDF-1.4\ntruncated");
        assert_eq!(
            probe_pdf(&bad.path().join("bad.pdf")),
            Err("cannot read PDF".into())
        );
    }

    #[test]
    fn pdf_range_table_is_inclusive_and_clamped() {
        for (start, end, expected) in [
            (0, None, Ok(3)),
            (0, Some(1), Ok(2)),
            (0, Some(u64::MAX), Ok(3)),
            (2, None, Ok(1)),
            (2, Some(1), Err("PDF page range is invalid")),
            (3, None, Err("PDF page range is invalid")),
        ] {
            let options = RemoteOptions {
                start,
                end,
                ..Default::default()
            };
            let result = pages_for_pdf(Path::new("x"), &options, &mut |_| Ok(3));
            assert_eq!(result, expected.map_err(str::to_owned));
        }
    }

    #[test]
    fn nested_page_tree_fixture_uses_production_probe_and_ranges() {
        let dir = file_with("nested.pdf", &nested_pages_pdf());
        let path = dir.path().join("nested.pdf");
        assert_eq!(probe_pdf(&path), Ok(2));
        for (start, end, expected) in [
            (0, None, Ok(2)),
            (1, None, Ok(1)),
            (0, Some(u64::MAX), Ok(2)),
            (2, None, Err("PDF page range is invalid")),
        ] {
            let options = RemoteOptions {
                start,
                end,
                ..Default::default()
            };
            assert_eq!(
                pages_for_pdf(&path, &options, &mut probe_pdf),
                expected.map_err(str::to_owned)
            );
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn real_classifier_discovers_minimal_pdf() {
        let fixture = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/pdf/minimal.pdf"
        ));
        let docs = discover(fixture, &RemoteOptions::default()).unwrap();
        assert_eq!(
            (docs[0].suffix.as_str(), docs[0].effective_pages),
            ("pdf", 1)
        );
    }
}
