use std::ffi::OsString;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
pub const DEFAULT_MAX_INPUT_BYTES: u64 = 4_293_918_719;
pub const DEFAULT_MAX_ENCODED_DOCUMENT_BYTES: u64 = 8 * GIB;
pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 8 * GIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentLimitPolicy {
    pub max_input_bytes: u64,
    pub max_encoded_document_bytes: u64,
    pub max_output_bytes: u64,
    pub(crate) multipart_body_bytes: u64,
    pub(crate) asset_total_bytes: u64,
    pub(crate) staged_text_bytes: u64,
    pub(crate) raw_output_bytes: u64,
    pub(crate) server_zip_bytes: u64,
    pub(crate) download_compressed_bytes: u64,
    pub(crate) expanded_archive_bytes: u64,
    pub(crate) archive_entry_bytes: u64,
}

/// Crate-private aggregate allowances for one official document. Resident limits remain on
/// `OfficialPdfOptions`; these totals may exceed `usize` on 32-bit targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OfficialDocumentTotals {
    pub(crate) encoded: u64,
    pub(crate) raw: u64,
    pub(crate) assets: u64,
    pub(crate) staged_text: u64,
}

impl OfficialDocumentTotals {
    pub(crate) fn from_options(options: &crate::OfficialPdfOptions) -> Self {
        Self {
            encoded: options.max_encoded_document_bytes as u64,
            raw: options.max_raw_output_bytes as u64,
            assets: options.max_total_asset_bytes as u64,
            staged_text: options.max_staged_text_bytes as u64,
        }
    }

    pub(crate) fn from_policy(policy: DocumentLimitPolicy) -> Self {
        Self {
            encoded: policy.max_encoded_document_bytes,
            raw: policy.raw_output_bytes,
            assets: policy.asset_total_bytes,
            staged_text: policy.staged_text_bytes,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DocumentLimitOverrides {
    pub max_input_bytes: Option<String>,
    pub max_encoded_document_bytes: Option<String>,
    pub max_output_bytes: Option<String>,
}

impl DocumentLimitPolicy {
    pub fn defaults() -> Self {
        Self::new(
            DEFAULT_MAX_INPUT_BYTES,
            DEFAULT_MAX_ENCODED_DOCUMENT_BYTES,
            DEFAULT_MAX_OUTPUT_BYTES,
        )
        .expect("compiled document limit defaults are valid")
    }

    pub fn resolve(
        cli: &DocumentLimitOverrides,
        mut environment: impl FnMut(&'static str) -> Option<OsString>,
    ) -> Result<Self, String> {
        Self::new(
            resolve_one(
                cli.max_input_bytes.as_deref(),
                environment("MINERU_MAX_INPUT_BYTES"),
                DEFAULT_MAX_INPUT_BYTES,
                "max input bytes",
            )?,
            resolve_one(
                cli.max_encoded_document_bytes.as_deref(),
                environment("MINERU_MAX_ENCODED_DOCUMENT_BYTES"),
                DEFAULT_MAX_ENCODED_DOCUMENT_BYTES,
                "max encoded document bytes",
            )?,
            resolve_one(
                cli.max_output_bytes.as_deref(),
                environment("MINERU_MAX_OUTPUT_BYTES"),
                DEFAULT_MAX_OUTPUT_BYTES,
                "max output bytes",
            )?,
        )
    }

    pub fn with_cli_overrides(self, cli: &DocumentLimitOverrides) -> Result<Self, String> {
        Self::new(
            resolve_one(
                cli.max_input_bytes.as_deref(),
                None,
                self.max_input_bytes,
                "max input bytes",
            )?,
            resolve_one(
                cli.max_encoded_document_bytes.as_deref(),
                None,
                self.max_encoded_document_bytes,
                "max encoded document bytes",
            )?,
            resolve_one(
                cli.max_output_bytes.as_deref(),
                None,
                self.max_output_bytes,
                "max output bytes",
            )?,
        )
    }

    pub fn new(
        max_input_bytes: u64,
        max_encoded_document_bytes: u64,
        max_output_bytes: u64,
    ) -> Result<Self, String> {
        for (value, minimum, name) in [
            (max_input_bytes, 1, "max input bytes"),
            (max_encoded_document_bytes, 1, "max encoded document bytes"),
            (max_output_bytes, 4, "max output bytes"),
        ] {
            if value < minimum {
                return Err(format!("{name} must be at least {minimum}"));
            }
        }
        let multipart_body_bytes = max_input_bytes
            .checked_add(MIB)
            .ok_or("multipart body limit overflow")?;
        let asset_total_bytes = max_output_bytes;
        let staged_text_bytes = max_output_bytes / 4;
        let raw_output_bytes = max_output_bytes / 4;
        let server_zip_bytes = max_input_bytes
            .checked_add(asset_total_bytes)
            .and_then(|v| v.checked_add(staged_text_bytes))
            .and_then(|v| v.checked_add(MIB))
            .ok_or("server ZIP limit overflow")?;
        let rounded_server_zip = server_zip_bytes
            .checked_add(GIB - 1)
            .ok_or("server ZIP rounding overflow")?
            / GIB
            * GIB;
        let download_compressed_bytes = rounded_server_zip;
        let expanded_archive_bytes = download_compressed_bytes
            .checked_mul(2)
            .ok_or("expanded archive limit overflow")?;
        Ok(Self {
            max_input_bytes,
            max_encoded_document_bytes,
            max_output_bytes,
            multipart_body_bytes,
            asset_total_bytes,
            staged_text_bytes,
            raw_output_bytes,
            server_zip_bytes,
            download_compressed_bytes,
            expanded_archive_bytes,
            archive_entry_bytes: expanded_archive_bytes,
        })
    }
}

fn resolve_one(
    cli: Option<&str>,
    environment: Option<OsString>,
    default: u64,
    name: &str,
) -> Result<u64, String> {
    match cli {
        Some(value) => parse_bytes(value, name),
        None => match environment {
            Some(value) => parse_bytes(
                value
                    .to_str()
                    .ok_or_else(|| format!("{name} must be unsigned decimal bytes"))?,
                name,
            ),
            None => Ok(default),
        },
    }
}

fn parse_bytes(value: &str, name: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('+') || value.starts_with('-') {
        return Err(format!("{name} must be unsigned decimal bytes"));
    }
    let mut number = 0u64;
    let mut previous_digit = false;
    for byte in value.bytes() {
        match byte {
            b'0'..=b'9' => {
                number = number
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((byte - b'0') as u64))
                    .ok_or_else(|| format!("{name} overflows u64"))?;
                previous_digit = true;
            }
            b'_' if previous_digit => previous_digit = false,
            _ => return Err(format!("{name} must be unsigned decimal bytes")),
        }
    }
    if !previous_digit || number == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    #[test]
    fn defaults_and_derivations_are_exact() {
        let p = DocumentLimitPolicy::defaults();
        assert_eq!(
            (
                p.max_input_bytes,
                p.max_encoded_document_bytes,
                p.max_output_bytes
            ),
            (4_293_918_719, 8 * GIB, 8 * GIB)
        );
        assert_eq!(p.multipart_body_bytes, u32::MAX as u64);
        assert_eq!(
            (p.asset_total_bytes, p.staged_text_bytes, p.raw_output_bytes),
            (8 * GIB, 2 * GIB, 2 * GIB)
        );
        assert_eq!(
            (
                p.server_zip_bytes,
                p.download_compressed_bytes,
                p.expanded_archive_bytes,
                p.archive_entry_bytes
            ),
            (14 * GIB - 1, 14 * GIB, 28 * GIB, 28 * GIB)
        );
    }
    #[test]
    fn parser_boundaries_and_precedence() {
        let env = HashMap::from([
            ("MINERU_MAX_INPUT_BYTES", " 4_294_967_296 "),
            ("MINERU_MAX_ENCODED_DOCUMENT_BYTES", "9"),
            ("MINERU_MAX_OUTPUT_BYTES", "10"),
        ]);
        let p = DocumentLimitPolicy::resolve(
            &DocumentLimitOverrides {
                max_input_bytes: Some(" 4_293_918_720 ".into()),
                ..Default::default()
            },
            |n| env.get(n).map(|v| OsString::from(*v)),
        )
        .unwrap();
        assert_eq!(
            (
                p.max_input_bytes,
                p.max_encoded_document_bytes,
                p.max_output_bytes
            ),
            (4_293_918_720, 9, 10)
        );
        for value in ["4293918719", "4294967296"] {
            assert_eq!(
                DocumentLimitPolicy::resolve(
                    &DocumentLimitOverrides {
                        max_input_bytes: Some(value.into()),
                        ..Default::default()
                    },
                    |_| None
                )
                .unwrap()
                .max_input_bytes,
                value.parse::<u64>().unwrap()
            );
        }
        for value in ["0", "-1", "1MiB", "", "18446744073709551616"] {
            assert!(
                DocumentLimitPolicy::resolve(
                    &DocumentLimitOverrides {
                        max_input_bytes: Some(value.into()),
                        ..Default::default()
                    },
                    |_| None
                )
                .is_err(),
                "{value}"
            );
        }
        // Values above the removed 16/64/16 GiB ceilings are accepted without allocation;
        // only u64 overflow, non-positive values, and derived-value overflow are rejected.
        for (name, value) in [
            ("MINERU_MAX_INPUT_BYTES", "17179869185"),
            ("MINERU_MAX_ENCODED_DOCUMENT_BYTES", "68719476736"),
            ("MINERU_MAX_OUTPUT_BYTES", "17179869185"),
            ("MINERU_MAX_INPUT_BYTES", "1099511627776"),
        ] {
            let resolved = DocumentLimitPolicy::resolve(&DocumentLimitOverrides::default(), |n| {
                (n == name).then(|| OsString::from(value))
            })
            .unwrap_or_else(|error| panic!("{name}={value}: {error}"));
            let received = if name == "MINERU_MAX_OUTPUT_BYTES" {
                resolved.max_output_bytes
            } else if name == "MINERU_MAX_ENCODED_DOCUMENT_BYTES" {
                resolved.max_encoded_document_bytes
            } else {
                resolved.max_input_bytes
            };
            assert_eq!(received, value.parse::<u64>().unwrap(), "{name}={value}");
        }
        // The multipart body derivation is checked: an input at u64::MAX overflows it.
        assert!(DocumentLimitPolicy::new(u64::MAX, 4, 4).is_err());
        assert!(
            DocumentLimitPolicy::resolve(&DocumentLimitOverrides::default(), |n| (n
                == "MINERU_MAX_OUTPUT_BYTES")
                .then(|| OsString::from("bad")))
            .is_err()
        );
    }
    #[test]
    fn resolved_multipart_body_bytes_fit_platform_usize() {
        // The multipart body derivation stays within the u32-sized domain every supported
        // platform's usize can represent; larger derived caps are checked by `usize::try_from`
        // at their consumption sites.
        let p = DocumentLimitPolicy::defaults();
        assert_eq!(p.multipart_body_bytes, u32::MAX as u64);
        assert!(usize::try_from(p.multipart_body_bytes).is_ok());
    }

    #[test]
    fn output_below_four_is_rejected() {
        for value in 1..4 {
            assert!(DocumentLimitPolicy::new(DEFAULT_MAX_INPUT_BYTES, 4, value).is_err());
        }
        let policy = DocumentLimitPolicy::new(DEFAULT_MAX_INPUT_BYTES, 4, 4).unwrap();
        assert_eq!((policy.staged_text_bytes, policy.raw_output_bytes), (1, 1));
    }
}
