#[cfg(feature = "legacy-office")]
use anydoc::Format as AnyDocFormat;
use mineru::command::service::OfficeLimits;
#[cfg(feature = "legacy-office")]
use mineru::legacy_office;
#[cfg(feature = "office")]
use office2pdf::config::{ConvertOptions, Format as PdfFormat};
use std::io::Read;
#[cfg(any(feature = "office", feature = "legacy-office"))]
use std::io::Write;

fn main() {
    // The parent CLI resolves the office policy at startup and writes it into this child's
    // explicit environment; the helper reads it exactly once here and never re-reads it.
    let limits = OfficeLimits::from_child_env();
    let _containment = containment(&limits).unwrap_or_else(|_| fail("containment setup failed"));
    let mut args = std::env::args_os();
    let _ = args.next();
    let first = args.next();
    let mode = std::env::var("MINERU_OFFICE_CONVERT_MODE").unwrap_or_default();
    let native_request = if mode == "native-pdf" {
        let max_pages = native_request_arg(args.next());
        let max_output_bytes = native_request_arg(args.next());
        if args.next().is_some() {
            fail("invalid native PDF request");
        }
        Some((max_pages, max_output_bytes))
    } else {
        if args.next().is_some() {
            fail(
                "usage: mineru-office-convert <docx|pptx|xlsx|doc|ppt|xls|odt|rtf|epub|ods|odp|csv>",
            );
        }
        None
    };
    let value = first.unwrap_or_else(|| {
        fail("usage: mineru-office-convert <docx|pptx|xlsx|doc|ppt|xls|odt|rtf|epub|ods|odp|csv>")
    });
    let requested = match value.to_str() {
        Some(value) => value,
        None => fail("invalid format"),
    };
    let mut input = Vec::new();
    if std::io::stdin()
        .take((limits.input_bytes as u64) + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() > limits.input_bytes
    {
        fail("input too large");
    }
    if let Some((max_pages, max_output_bytes)) = native_request {
        if requested != "pdf"
            || max_pages.is_none()
            || max_output_bytes.is_none()
            || max_output_bytes.is_some_and(|cap| cap > limits.output_bytes)
        {
            fail("invalid native PDF request");
        }
        convert_native_pdf(
            limits,
            &input,
            max_pages.unwrap_or_default(),
            max_output_bytes.unwrap_or_default(),
        );
    } else {
        convert(requested, &input, limits, mode == "legacy-pdf");
    }
    #[cfg(windows)]
    _containment.finish();
}

fn native_request_arg(value: Option<std::ffi::OsString>) -> Option<usize> {
    value
        .as_deref()
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
}

/// Dispatch on the requested format name. OOXML always converts to PDF; legacy formats use
/// Markdown for the compatibility lane or a bounded text PDF when the parent selects the
/// non-local VLM lane.
#[cfg_attr(
    not(any(feature = "office", feature = "legacy-office")),
    allow(unused_variables)
)]
fn convert(requested: &str, input: &[u8], limits: OfficeLimits, legacy_pdf: bool) {
    if legacy_pdf {
        #[cfg(feature = "legacy-office")]
        if let Some((kind, format)) = text_format(requested) {
            return convert_legacy_pdf(limits, kind, format, input);
        }
        fail("legacy PDF conversion is unavailable");
    }
    #[cfg(feature = "office")]
    if let Some((kind, format)) = pdf_format(requested) {
        return convert_pdf(limits, kind, format, input);
    }
    #[cfg(feature = "legacy-office")]
    if let Some((kind, format)) = text_format(requested) {
        return convert_text(limits, kind, format, input);
    }
    fail("invalid format");
}

/// The requested OOXML format is resolved against the OOXML preflight before conversion.
#[cfg(feature = "office")]
fn pdf_format(requested: &str) -> Option<(&'static str, PdfFormat)> {
    Some(match requested {
        "docx" => ("docx", PdfFormat::Docx),
        "pptx" => ("pptx", PdfFormat::Pptx),
        "xlsx" => ("xlsx", PdfFormat::Xlsx),
        _ => return None,
    })
}

/// The requested legacy format; `xls` shares the Excel parser with the OOXML family.
#[cfg(feature = "legacy-office")]
fn text_format(requested: &str) -> Option<(&'static str, AnyDocFormat)> {
    let kind = legacy_office::kind_from_name(requested)?;
    Some((kind.suffix(), legacy_office::format_for_kind(kind)?))
}

/// Content cross-validation for the legacy family. CSV carries no file signature, so it is
/// accepted as declared; every other legacy format must match its container signature. A UTF-8
/// BOM (written by LibreOffice/Word for non-ASCII text) is not part of any signature, so it is
/// skipped for detection.
#[cfg(feature = "legacy-office")]
#[cfg(test)]
fn format_matches(requested_kind: &str, _format: AnyDocFormat, input: &[u8]) -> bool {
    legacy_office::kind_from_name(requested_kind)
        .is_some_and(|kind| legacy_office::format_matches(kind, input))
}

#[cfg(feature = "office")]
fn convert_pdf(limits: OfficeLimits, requested_kind: &str, format: PdfFormat, input: &[u8]) {
    if !requested_kind_matches(
        mineru::preflight_ooxml_bytes_with(
            input,
            mineru::command::service::OoxmlLimits::from_child_env(),
        ),
        requested_kind,
    ) {
        fail("input format does not match requested format");
    }
    let result = office2pdf::convert_bytes(input, format, &ConvertOptions::default())
        .unwrap_or_else(|_| fail("conversion failed"));
    if !result.pdf.starts_with(b"%PDF-") {
        fail("conversion produced invalid PDF");
    }
    if result.pdf.len() > limits.output_bytes {
        fail("conversion produced oversized PDF");
    }
    if std::io::stdout().write_all(&result.pdf).is_err() {
        fail("output failed");
    }
    if !result.warnings.is_empty() {
        eprintln!("conversion warnings: {}", result.warnings.len());
    }
}

#[cfg(feature = "legacy-office")]
fn convert_text(limits: OfficeLimits, requested_kind: &str, _format: AnyDocFormat, input: &[u8]) {
    let kind =
        legacy_office::kind_from_name(requested_kind).unwrap_or_else(|| fail("invalid format"));
    // No image assets are written: embedded images are referenced but not extracted.
    let markdown = legacy_office::to_markdown_bytes(kind, input, limits).unwrap_or_else(|error| {
        if matches!(
            error.as_str(),
            "input format does not match requested format" | "conversion produced oversized output"
        ) {
            fail(&error);
        }
        fail("conversion failed");
    });
    if std::io::stdout().write_all(&markdown).is_err() {
        fail("output failed");
    }
}

#[cfg_attr(not(feature = "legacy-office"), allow(unused_variables))]
fn convert_native_pdf(
    limits: OfficeLimits,
    input: &[u8],
    max_pages: usize,
    max_output_bytes: usize,
) {
    #[cfg(feature = "legacy-office")]
    {
        let markdown = legacy_office::native_pdf_to_markdown(input, max_pages, max_output_bytes)
            .unwrap_or_else(|error| fail(&error));
        if markdown.len() > max_output_bytes {
            fail("native PDF assessment produced oversized output");
        }
        if std::io::stdout().write_all(&markdown).is_err() {
            fail("output failed");
        }
    }
    #[cfg(not(feature = "legacy-office"))]
    fail("native PDF conversion is unavailable");
}

#[cfg(feature = "legacy-office")]
fn convert_legacy_pdf(
    limits: OfficeLimits,
    requested_kind: &str,
    _format: AnyDocFormat,
    input: &[u8],
) {
    let kind =
        legacy_office::kind_from_name(requested_kind).unwrap_or_else(|| fail("invalid format"));
    let pdf = legacy_office::to_pdf_bytes(kind, input, limits).unwrap_or_else(|error| {
        fail(&format!(
            "{error}; {}; {}",
            legacy_office::LEGACY_PDF_WARNING,
            legacy_office::LEGACY_PDF_RECOMMENDATION
        ))
    });
    if !pdf.starts_with(b"%PDF-") {
        fail("conversion produced invalid PDF");
    }
    if pdf.len() > limits.output_bytes {
        fail("conversion produced oversized PDF");
    }
    if std::io::stdout().write_all(&pdf).is_err() {
        fail("output failed");
    }
}

#[cfg(feature = "office")]
fn requested_kind_matches(detected: Result<Option<&'static str>, String>, requested: &str) -> bool {
    matches!(detected, Ok(Some(kind)) if kind == requested)
}

#[cfg(feature = "office")]
#[cfg(test)]
fn valid_pdf_output(pdf: &[u8], output_cap: usize) -> bool {
    pdf.len() <= output_cap && pdf.starts_with(b"%PDF-")
}

#[cfg(unix)]
fn containment(limits: &OfficeLimits) -> Result<(), ()> {
    macro_rules! limit {
        ($resource:expr, $value:expr) => {{
            let mut existing = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            // SAFETY: `existing` points to writable storage for the libc-provided resource.
            if unsafe { libc::getrlimit($resource, existing.as_mut_ptr()) } != 0 {
                Err(())
            } else {
                // SAFETY: getrlimit initialized `existing` on its successful return.
                let limit = clamp_limit(unsafe { existing.assume_init() }, $value);
                // SAFETY: `limit` is initialized and only tightens the existing resource limits.
                if unsafe { libc::setrlimit($resource, &limit) } == 0 {
                    Ok(())
                } else {
                    Err(())
                }
            }
        }};
    }
    limit!(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    limit!(libc::RLIMIT_NOFILE, limits.nofile)?;
    #[cfg(target_os = "linux")]
    limit!(libc::RLIMIT_AS, limits.address_space_bytes)?;
    // macOS has no reliable no-entitlement process memory rlimit; external isolation supplies hard memory containment.
    Ok(())
}

#[cfg(unix)]
fn clamp_limit(existing: libc::rlimit, requested: libc::rlim_t) -> libc::rlimit {
    let rlim_max = existing.rlim_max.min(requested);
    libc::rlimit {
        rlim_cur: existing.rlim_cur.min(rlim_max),
        rlim_max,
    }
}

#[cfg(windows)]
struct Job(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: this is the handle successfully returned by CreateJobObjectW.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
impl Job {
    fn finish(self) {
        // Keep the sole KILL_ON_JOB_CLOSE handle open until process teardown. Closing it after a
        // successful conversion would terminate this helper before it can return to its parent.
        std::mem::forget(self);
    }
}

#[cfg(windows)]
fn containment(limits: &OfficeLimits) -> Result<Job, ()> {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject,
            },
            Threading::GetCurrentProcess,
        },
    };
    // SAFETY: null attributes/name request an unnamed job. The returned handle is retained by Job.
    let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(());
    }
    let job = Job(handle);
    let limits = job_limits(limits);
    // SAFETY: `limits` is the exact structure selected by JobObjectExtendedLimitInformation.
    if unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    } == 0
    {
        return Err(());
    }
    // SAFETY: both handles are valid for this process; assignment is required before parsing input.
    if unsafe { AssignProcessToJobObject(job.0, GetCurrentProcess()) } == 0 {
        return Err(());
    }
    Ok(job)
}

#[cfg(windows)]
fn job_limits(
    limits: &OfficeLimits,
) -> windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    let mut limits_config = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits_config.BasicLimitInformation.PerProcessUserTimeLimit =
        i64::try_from(limits.process_time_seconds.saturating_mul(10_000_000)).unwrap_or(i64::MAX);
    limits_config.BasicLimitInformation.PerJobUserTimeLimit =
        i64::try_from(limits.job_time_seconds.saturating_mul(10_000_000)).unwrap_or(i64::MAX);
    limits_config.BasicLimitInformation.ActiveProcessLimit = limits.active_process_limit;
    limits_config.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_JOB_TIME
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits_config.ProcessMemoryLimit =
        usize::try_from(limits.process_memory_bytes).unwrap_or(usize::MAX);
    limits_config.JobMemoryLimit = usize::try_from(limits.job_memory_bytes).unwrap_or(usize::MAX);
    limits_config
}

#[cfg(not(any(unix, windows)))]
fn containment(_limits: &OfficeLimits) -> Result<(), ()> {
    Err(())
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "office")]
    #[test]
    fn rejects_exact_kind_mismatch() {
        assert!(!requested_kind_matches(Ok(Some("pptx")), "docx"));
        assert!(!requested_kind_matches(Ok(None), "docx"));
        assert!(!requested_kind_matches(Err("bad archive".into()), "docx"));
    }

    #[cfg(feature = "office")]
    #[test]
    fn output_cap_is_inclusive() {
        assert!(!valid_pdf_output(b"not a PDF", 64));
        let mut at_cap = vec![b'x'; 64];
        at_cap[..5].copy_from_slice(b"%PDF-");
        assert!(valid_pdf_output(&at_cap, 64));
        at_cap.push(b'x');
        assert!(!valid_pdf_output(&at_cap, 64));
    }

    #[cfg(feature = "office")]
    #[test]
    fn pdf_format_mapping_is_closed() {
        assert_eq!(pdf_format("docx"), Some(("docx", PdfFormat::Docx)));
        assert_eq!(pdf_format("pptx"), Some(("pptx", PdfFormat::Pptx)));
        assert_eq!(pdf_format("xlsx"), Some(("xlsx", PdfFormat::Xlsx)));
        assert_eq!(pdf_format("doc"), None);
        assert_eq!(pdf_format("DOCX"), None); // helper receives lowercase names only
    }

    #[cfg(feature = "legacy-office")]
    #[test]
    fn text_format_mapping_is_closed() {
        for (name, format) in [
            ("doc", AnyDocFormat::Doc),
            ("ppt", AnyDocFormat::Ppt),
            ("xls", AnyDocFormat::Excel),
            ("odt", AnyDocFormat::Odt),
            ("rtf", AnyDocFormat::Rtf),
            ("epub", AnyDocFormat::Epub),
            ("ods", AnyDocFormat::Ods),
            ("odp", AnyDocFormat::Odp),
            ("csv", AnyDocFormat::Csv),
        ] {
            assert_eq!(text_format(name), Some((name, format)), "{name}");
        }
        assert_eq!(text_format("docx"), None);
        assert_eq!(text_format("bad"), None);
    }

    #[cfg(feature = "legacy-office")]
    #[test]
    fn legacy_content_cross_validation_matches_or_rejects() {
        assert!(format_matches(
            "rtf",
            AnyDocFormat::Rtf,
            b"{\\rtf1\\ansi hello}"
        ));
        // A UTF-8 BOM is skipped for detection, so a BOM-prefixed RTF still matches.
        assert!(format_matches(
            "rtf",
            AnyDocFormat::Rtf,
            b"\xef\xbb\xbf{\\rtf1\\ansi hello}"
        ));
        // A signature-less container cannot be detected: CSV is accepted as declared.
        assert!(format_matches("csv", AnyDocFormat::Csv, b"a,b\n1,2\n"));
        // Mismatched or undetectable content is rejected with the shared mismatch error.
        assert!(!format_matches(
            "doc",
            AnyDocFormat::Doc,
            b"{\\rtf1\\ansi hello}"
        ));
        assert!(!format_matches("doc", AnyDocFormat::Doc, b"garbage"));
    }

    #[test]
    fn office_limits_round_trip_through_child_env() {
        use std::sync::{LazyLock, Mutex};
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        let _guard = LOCK.lock().unwrap();
        let mut saved = Vec::new();
        for name in mineru::command::service::OFFICE_ENV_NAMES {
            saved.push((name, std::env::var_os(name)));
        }
        let limits = OfficeLimits {
            input_bytes: 3,
            output_bytes: 5,
            stderr_bytes: 7,
            wall: std::time::Duration::from_secs(9),
            cpu_seconds: 11,
            nofile: 13,
            address_space_bytes: 17,
            active_process_limit: 19,
            process_memory_bytes: 23,
            job_memory_bytes: 29,
            process_time_seconds: 31,
            job_time_seconds: 37,
        };
        for (name, value) in limits.child_env() {
            // SAFETY: serialized by the mutex in this single-threaded test process.
            unsafe { std::env::set_var(name, value) };
        }
        let read = OfficeLimits::from_child_env();
        assert_eq!(read, limits);
        for (name, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn limit_clamping_only_tightens_existing_limits() {
        let tightened = clamp_limit(
            libc::rlimit {
                rlim_cur: 60,
                rlim_max: 90,
            },
            120,
        );
        assert_eq!((tightened.rlim_cur, tightened.rlim_max), (60, 90));
        let requested = clamp_limit(
            libc::rlimit {
                rlim_cur: libc::RLIM_INFINITY,
                rlim_max: libc::RLIM_INFINITY,
            },
            120,
        );
        assert_eq!((requested.rlim_cur, requested.rlim_max), (120, 120));
        let inconsistent = clamp_limit(
            libc::rlimit {
                rlim_cur: 500,
                rlim_max: 100,
            },
            120,
        );
        assert_eq!((inconsistent.rlim_cur, inconsistent.rlim_max), (100, 100));
    }

    #[cfg(windows)]
    #[test]
    fn job_configuration_requires_a_retained_kill_handle() {
        use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let limits = OfficeLimits::default();
        assert_ne!(
            job_limits(&limits).BasicLimitInformation.LimitFlags
                & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0
        );
        assert_eq!(
            job_limits(&limits).BasicLimitInformation.ActiveProcessLimit,
            limits.active_process_limit
        );
    }
}
