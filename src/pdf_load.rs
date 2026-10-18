use lopdf::{Document, LoadOptions, Object, ObjectStream, Reader, xref::XrefEntry};
use std::collections::{BTreeMap, HashMap, HashSet};

pub(crate) fn load_bounded(bytes: &[u8], limit: usize) -> lopdf::Result<Document> {
    let mut options = LoadOptions::with_max_decompressed_size(limit);
    options.strict = false;
    let document = Document::load_mem_with_options(bytes, options)?;
    if document.is_encrypted() || document.was_encrypted() {
        return Err(lopdf::Error::InvalidPassword);
    }

    let offset = bytes
        .windows(5)
        .position(|window| window == b"%PDF-")
        .unwrap_or(0);
    let reader = Reader {
        buffer: &bytes[offset..],
        document,
        encryption_state: None,
        raw_objects: BTreeMap::new(),
        password: None,
        strict: false,
        max_decompressed_size: Some(limit),
    };
    let entries: Vec<_> = reader
        .document
        .reference_table
        .entries
        .iter()
        .map(|(&number, entry)| (number, entry.clone()))
        .collect();
    // Reader::get_object reparses an object stream for each compressed member.
    let mut members = HashMap::<u32, Vec<u32>>::new();
    for (number, entry) in &entries {
        if let XrefEntry::Compressed { container, .. } = entry {
            members.entry(*container).or_default().push(*number);
        }
    }
    let mut validated = HashSet::new();
    for (number, entry) in entries.iter() {
        if let XrefEntry::Normal { generation, .. } = entry {
            let object = reader.get_object((*number, *generation), &mut HashSet::new())?;
            let Object::Stream(mut stream) = object else {
                continue;
            };
            if (stream.dict.has_type(b"ObjStm") || stream.dict.has_type(b"XRef"))
                && stream.start_position.is_some()
            {
                return Err(lopdf::Error::InvalidStream(
                    "structural stream data is unresolved".into(),
                ));
            }
            if stream.dict.has_type(b"ObjStm") {
                let decoded = ObjectStream::new_with_limit(&mut stream, Some(limit))?;
                if let Some(ids) = members.get(number) {
                    for id in ids {
                        let member = (*id, 0);
                        if !decoded.objects.contains_key(&member)
                            || !reader.document.objects.contains_key(&member)
                        {
                            return Err(lopdf::Error::MissingXrefEntry);
                        }
                    }
                    validated.insert(*number);
                }
            } else if stream.dict.has_type(b"XRef") {
                stream.decompressed_content_with_limit(limit)?;
            }
        }
    }
    if members
        .keys()
        .any(|container| !validated.contains(container))
    {
        return Err(lopdf::Error::MissingXrefEntry);
    }
    Ok(reader.document)
}

#[cfg(test)]
mod tests {
    use super::load_bounded;
    use lopdf::{Document, Object, StringFormat, dictionary};

    fn fixture(stream_type: &[u8]) -> Vec<u8> {
        let mut document = Document::with_version("1.5");
        let pages = document.add_object(
            dictionary! { "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0 },
        );
        let extra = if stream_type == b"ObjStm" {
            document.add_object(Object::String(vec![b' '; 64 * 1024], StringFormat::Literal))
        } else {
            for index in 0..1_500 {
                document.add_object(Object::Integer(index));
            }
            document.add_object(Object::Integer(0))
        };
        let catalog = document
            .add_object(dictionary! { "Type" => "Catalog", "Pages" => pages, "Extra" => extra });
        document.trailer.set("Root", catalog);
        let mut bytes = Vec::new();
        document.save_modern(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn rejects_oversized_object_and_xref_streams_without_partial_documents() {
        for stream_type in [b"ObjStm".as_slice(), b"XRef"] {
            let bytes = fixture(stream_type);
            let document = load_bounded(&bytes, 128 * 1024).unwrap();
            if stream_type == b"ObjStm" {
                let root = document
                    .trailer
                    .get(b"Root")
                    .unwrap()
                    .as_reference()
                    .unwrap();
                let member = document
                    .get_object(root)
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get(b"Extra")
                    .unwrap()
                    .as_reference()
                    .unwrap();
                assert_eq!(
                    document.get_object(member).unwrap().as_str().unwrap().len(),
                    64 * 1024
                );
            }
            assert!(
                load_bounded(&bytes, 8 * 1024).is_err(),
                "stream type: {stream_type:?}"
            );
        }
    }

    #[test]
    fn preserves_normal_pdf_and_header_offset_loading() {
        let bytes = include_bytes!("../tests/fixtures/pdf/minimal.pdf");
        let document = load_bounded(bytes, 128 * 1024).unwrap();
        assert_eq!(document.get_pages().len(), 1);
        let mut prefixed = b"transport prefix\n".to_vec();
        prefixed.extend_from_slice(bytes);
        let document = load_bounded(&prefixed, 128 * 1024).unwrap();
        assert_eq!(document.get_pages().len(), 1);
        let mut tolerant = bytes.to_vec();
        let newline = tolerant.iter().position(|byte| *byte == b'\n').unwrap();
        tolerant[newline - 1] = b'x';
        assert_eq!(
            load_bounded(&tolerant, 128 * 1024)
                .unwrap()
                .get_pages()
                .len(),
            1
        );
    }

    #[test]
    fn rejects_empty_password_encryption_before_raw_object_reread() {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions, StringFormat};
        let mut document =
            Document::load_mem(include_bytes!("../tests/fixtures/pdf/minimal.pdf")).unwrap();
        document.trailer.set(
            "ID",
            vec![
                Object::String(vec![b'1'; 16], StringFormat::Hexadecimal),
                Object::String(vec![b'1'; 16], StringFormat::Hexadecimal),
            ],
        );
        let state = EncryptionState::try_from(EncryptionVersion::V1 {
            document: &document,
            owner_password: "owner",
            user_password: "",
            permissions: Permissions::all(),
        })
        .unwrap();
        document.encrypt(&state).unwrap();
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        assert!(matches!(
            load_bounded(&bytes, 128 * 1024),
            Err(lopdf::Error::InvalidPassword)
        ));
    }
}
