use office2pdf::config::{ConvertOptions, Format};
use std::io::{Read, Write};

const INPUT_CAP: usize = 32 * 1024 * 1024;
const OUTPUT_CAP: usize = 64 * 1024 * 1024;

fn main() {
    let _containment = containment().unwrap_or_else(|_| fail("containment setup failed"));
    let mut args = std::env::args_os();
    let _ = args.next();
    let (requested_kind, format) = match (args.next().as_deref(), args.next()) {
        (Some(value), None) => match value.to_str() {
            Some("docx") => ("docx", Format::Docx),
            Some("pptx") => ("pptx", Format::Pptx),
            Some("xlsx") => ("xlsx", Format::Xlsx),
            _ => fail("invalid format"),
        },
        _ => fail("usage: mineru-office-convert <docx|pptx|xlsx>"),
    };
    let mut input = Vec::new();
    if std::io::stdin()
        .take((INPUT_CAP as u64) + 1)
        .read_to_end(&mut input)
        .is_err()
        || input.len() > INPUT_CAP
    {
        fail("input too large");
    }
    if !requested_kind_matches(mineru::preflight_ooxml_bytes(&input), requested_kind) {
        fail("input format does not match requested format");
    }
    let result = office2pdf::convert_bytes(&input, format, &ConvertOptions::default())
        .unwrap_or_else(|_| fail("conversion failed"));
    if !result.pdf.starts_with(b"%PDF-") {
        fail("conversion produced invalid PDF");
    }
    if result.pdf.len() > OUTPUT_CAP {
        fail("conversion produced oversized PDF");
    }
    if std::io::stdout().write_all(&result.pdf).is_err() {
        fail("output failed");
    }
    if !result.warnings.is_empty() {
        eprintln!("conversion warnings: {}", result.warnings.len());
    }
    #[cfg(windows)]
    _containment.finish();
}

fn requested_kind_matches(detected: Result<Option<&'static str>, String>, requested: &str) -> bool {
    matches!(detected, Ok(Some(kind)) if kind == requested)
}

#[cfg(test)]
fn valid_pdf_output(pdf: &[u8]) -> bool {
    pdf.len() <= OUTPUT_CAP && pdf.starts_with(b"%PDF-")
}

#[cfg(unix)]
fn containment() -> Result<(), ()> {
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
    limit!(libc::RLIMIT_CPU, 120)?;
    limit!(libc::RLIMIT_NOFILE, 256)?;
    #[cfg(target_os = "linux")]
    limit!(libc::RLIMIT_AS, 1024 * 1024 * 1024)?;
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
fn containment() -> Result<Job, ()> {
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
    let limits = job_limits();
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
fn job_limits() -> windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.PerProcessUserTimeLimit = 120 * 10_000_000;
    limits.BasicLimitInformation.PerJobUserTimeLimit = 120 * 10_000_000;
    limits.BasicLimitInformation.ActiveProcessLimit = 8;
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_JOB_TIME
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits.ProcessMemoryLimit = 1024 * 1024 * 1024;
    limits.JobMemoryLimit = 1024 * 1024 * 1024;
    limits
}

#[cfg(not(any(unix, windows)))]
fn containment() -> Result<(), ()> {
    Err(())
}

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_exact_kind_mismatch() {
        assert!(!requested_kind_matches(Ok(Some("pptx")), "docx"));
        assert!(!requested_kind_matches(Ok(None), "docx"));
        assert!(!requested_kind_matches(Err("bad archive".into()), "docx"));
    }

    #[test]
    fn output_cap_is_inclusive() {
        assert!(!valid_pdf_output(b"not a PDF"));
        let mut at_cap = vec![b'x'; OUTPUT_CAP];
        at_cap[..5].copy_from_slice(b"%PDF-");
        assert!(valid_pdf_output(&at_cap));
        at_cap.push(b'x');
        assert!(!valid_pdf_output(&at_cap));
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
        assert_ne!(
            job_limits().BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            0
        );
    }
}
