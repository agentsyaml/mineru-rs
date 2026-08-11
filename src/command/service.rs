//! Phase-1B service/ingestion/archive/Office policy seam.
//!
//! Every numeric operational policy that bounds remote-API ingestion, result archives, OOXML
//! preflight, or Office helper resources has exactly one environment spelling plus a flag on the
//! CLI that owns the operation, with the same strict precedence as the Phase-1A core seam:
//! compiled default -> frozen environment -> explicit CLI. Malformed, non-finite, zero-where-
//! invalid, overflow, or platform-unrepresentable values fail before any network or output work.
//!
//! Security invariants (path traversal, symlink/special-file rejection, CRC/ZIP structural
//! correctness, portable filenames, same-origin rules, fixed file-format semantics, ZIP64
//! structure, 255-byte components, process-reap mechanics) are not exposed here.

use super::env::{positive_seconds, positive_u32, positive_u64, positive_usize, strict_bool};
use crate::document_limits::{GIB, MIB};
use crate::mineru_api::archive::ArchiveLimits;
use crate::mineru_api::zip_scan::ScanLimits;
use std::{ffi::OsString, time::Duration};

// ---------------------------------------------------------------------------
// Environment spellings (the parent->helper child contract uses the office names).
// ---------------------------------------------------------------------------

/// Child-environment spelling for the Office helper input byte cap.
pub const OFFICE_INPUT_ENV: &str = "MINERU_OFFICE_INPUT_BYTES";
/// Child-environment spelling for the Office helper output byte cap.
pub const OFFICE_OUTPUT_ENV: &str = "MINERU_OFFICE_OUTPUT_BYTES";
/// Child-environment spelling for the Office helper stderr diagnostic cap.
pub const OFFICE_STDERR_ENV: &str = "MINERU_OFFICE_STDERR_BYTES";
/// Child-environment spelling for the Office helper wall-time cap.
pub const OFFICE_WALL_ENV: &str = "MINERU_OFFICE_WALL_SECONDS";
/// Child-environment spelling for the Unix CPU-seconds rlimit.
pub const OFFICE_CPU_ENV: &str = "MINERU_OFFICE_CPU_SECONDS";
/// Child-environment spelling for the Unix NOFILE rlimit.
pub const OFFICE_NOFILE_ENV: &str = "MINERU_OFFICE_NOFILE";
/// Child-environment spelling for the Linux RLIMIT_AS value.
pub const OFFICE_ADDRESS_SPACE_ENV: &str = "MINERU_OFFICE_ADDRESS_SPACE_BYTES";
/// Child-environment spelling for the Windows job ActiveProcessLimit.
pub const OFFICE_ACTIVE_PROCESS_ENV: &str = "MINERU_OFFICE_ACTIVE_PROCESS_LIMIT";
/// Child-environment spelling for the Windows per-process memory limit.
pub const OFFICE_PROCESS_MEMORY_ENV: &str = "MINERU_OFFICE_PROCESS_MEMORY_BYTES";
/// Child-environment spelling for the Windows job memory limit.
pub const OFFICE_JOB_MEMORY_ENV: &str = "MINERU_OFFICE_JOB_MEMORY_BYTES";
/// Child-environment spelling for the Windows per-process user time limit.
pub const OFFICE_PROCESS_TIME_ENV: &str = "MINERU_OFFICE_PROCESS_TIME_SECONDS";
/// Child-environment spelling for the Windows per-job user time limit.
pub const OFFICE_JOB_TIME_ENV: &str = "MINERU_OFFICE_JOB_TIME_SECONDS";

/// Child-environment spelling for the OOXML archive byte cap.
pub const OOXML_ARCHIVE_ENV: &str = "MINERU_OOXML_ARCHIVE_BYTES";
/// Child-environment spelling for the OOXML expanded byte cap.
pub const OOXML_EXPANDED_ENV: &str = "MINERU_OOXML_EXPANDED_BYTES";
/// Child-environment spelling for the per-XML-entry byte cap.
pub const OOXML_XML_ENTRY_ENV: &str = "MINERU_OOXML_XML_ENTRY_BYTES";
/// Child-environment spelling for the aggregate XML byte cap.
pub const OOXML_XML_TOTAL_ENV: &str = "MINERU_OOXML_XML_TOTAL_BYTES";
/// Child-environment spelling for the OOXML compression ratio cap.
pub const OOXML_RATIO_ENV: &str = "MINERU_OOXML_RATIO";
/// Child-environment spelling for the XML depth cap.
pub const OOXML_XML_DEPTH_ENV: &str = "MINERU_OOXML_XML_DEPTH";
/// Child-environment spelling for the XML event cap.
pub const OOXML_XML_EVENTS_ENV: &str = "MINERU_OOXML_XML_EVENTS";
/// Child-environment spelling for the per-element attribute cap.
pub const OOXML_XML_ATTRIBUTES_ENV: &str = "MINERU_OOXML_XML_ATTRIBUTES";
/// Child-environment spelling for the per-element namespace cap.
pub const OOXML_XML_NAMESPACES_ENV: &str = "MINERU_OOXML_XML_NAMESPACES";

/// All office-related environment names, including the OOXML names the helper re-reads once.
pub const OFFICE_ENV_NAMES: [&str; 12] = [
    OFFICE_INPUT_ENV,
    OFFICE_OUTPUT_ENV,
    OFFICE_STDERR_ENV,
    OFFICE_WALL_ENV,
    OFFICE_CPU_ENV,
    OFFICE_NOFILE_ENV,
    OFFICE_ADDRESS_SPACE_ENV,
    OFFICE_ACTIVE_PROCESS_ENV,
    OFFICE_PROCESS_MEMORY_ENV,
    OFFICE_JOB_MEMORY_ENV,
    OFFICE_PROCESS_TIME_ENV,
    OFFICE_JOB_TIME_ENV,
];

/// The OOXML child-environment names carried alongside the office limits.
pub const OOXML_ENV_NAMES: [&str; 9] = [
    OOXML_ARCHIVE_ENV,
    OOXML_EXPANDED_ENV,
    OOXML_XML_ENTRY_ENV,
    OOXML_XML_TOTAL_ENV,
    OOXML_RATIO_ENV,
    OOXML_XML_DEPTH_ENV,
    OOXML_XML_EVENTS_ENV,
    OOXML_XML_ATTRIBUTES_ENV,
    OOXML_XML_NAMESPACES_ENV,
];

// ---------------------------------------------------------------------------
// Typed overrides and resolved policy
// ---------------------------------------------------------------------------

/// Typed, crate-private CLI/environment override set for service/ingestion policy.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServiceOverrides {
    // Local VLM transport booleans (strict; rejected in remote mode).
    pub vlm_text_before_image: Option<bool>,
    pub vlm_allow_truncated_content: Option<bool>,
    pub vlm_allow_remote_images: Option<bool>,
    pub vlm_allow_private_remote_images: Option<bool>,
    // Remote API task concurrency and timing.
    pub api_max_concurrent_requests: Option<usize>,
    pub task_result_timeout: Option<Duration>,
    pub task_download_timeout: Option<Duration>,
    pub api_connect_timeout: Option<Duration>,
    pub api_acquisition_timeout: Option<Duration>,
    pub api_send_timeout: Option<Duration>,
    pub api_poll_interval: Option<Duration>,
    // Result archive and ZIP-scan capacity.
    pub archive_max_entries: Option<u64>,
    pub archive_max_ratio: Option<u64>,
    pub zip_central_cap: Option<u64>,
    pub zip_name_cap: Option<usize>,
    pub zip_depth_cap: Option<usize>,
    pub zip_total_name_cap: Option<u64>,
    pub zip_total_component_cap: Option<u64>,
    // OOXML preflight capacity.
    pub ooxml_archive_bytes: Option<u64>,
    pub ooxml_expanded_bytes: Option<u64>,
    pub ooxml_xml_entry_bytes: Option<u64>,
    pub ooxml_xml_total_bytes: Option<u64>,
    pub ooxml_ratio: Option<u64>,
    pub ooxml_xml_depth: Option<usize>,
    pub ooxml_xml_events: Option<usize>,
    pub ooxml_xml_attributes: Option<usize>,
    pub ooxml_xml_namespaces: Option<usize>,
    // Office helper resource policy (parent-enforced and child-environment).
    pub office_input_bytes: Option<usize>,
    pub office_output_bytes: Option<usize>,
    pub office_stderr_bytes: Option<usize>,
    pub office_wall_seconds: Option<u64>,
    pub office_cpu_seconds: Option<u64>,
    pub office_nofile: Option<u64>,
    pub office_address_space_bytes: Option<u64>,
    pub office_active_process_limit: Option<u32>,
    pub office_process_memory_bytes: Option<u64>,
    pub office_job_memory_bytes: Option<u64>,
    pub office_process_time_seconds: Option<u64>,
    pub office_job_time_seconds: Option<u64>,
    // Task-service lifecycle and request caps (owned by the `mineru-api` CLI).
    pub task_retention: Option<Duration>,
    pub task_cleanup_interval: Option<Duration>,
    pub server_record_cap: Option<usize>,
    pub server_file_cap: Option<u64>,
    pub server_body_cap: Option<usize>,
    pub server_text_cap: Option<usize>,
    pub server_text_total_cap: Option<usize>,
    pub server_form_fields_cap: Option<usize>,
}

/// Resolved OOXML preflight policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OoxmlLimits {
    pub archive_bytes: u64,
    pub expanded_bytes: u64,
    pub xml_entry_bytes: u64,
    pub xml_total_bytes: u64,
    pub ratio: u64,
    pub xml_depth: usize,
    pub xml_events: usize,
    pub xml_attributes: usize,
    pub xml_namespaces: usize,
    /// Operator ZIP-scan policy shared by every ZIP preflight in this lane.
    pub scan: ScanLimits,
}

impl OoxmlLimits {
    pub fn default_resolved() -> Self {
        Self {
            archive_bytes: GIB,
            expanded_bytes: 256 * MIB,
            xml_entry_bytes: 8 * MIB,
            xml_total_bytes: 32 * MIB,
            ratio: 500,
            xml_depth: 128,
            xml_events: 100_000,
            xml_attributes: 256,
            xml_namespaces: 256,
            scan: ScanLimits::from_resolved(10_000, 64 * MIB, 4 * 1024, 64, 32 * MIB, 1_000_000)
                .expect("compiled scan defaults are valid"),
        }
    }

    pub fn validate(self) -> Result<Self, String> {
        if self.archive_bytes == 0
            || self.expanded_bytes == 0
            || self.xml_entry_bytes == 0
            || self.xml_total_bytes == 0
            || self.ratio == 0
            || self.xml_depth == 0
            || self.xml_events == 0
            || self.xml_attributes == 0
            || self.xml_namespaces == 0
        {
            return Err("OOXML limits must be positive".into());
        }
        Ok(self)
    }

    /// Returns the resolved policy as explicit child-environment pairs for the office helper.
    ///
    /// The operator ZIP-scan policy is serialized with its canonical `MINERU_*` env spellings so
    /// the child preflight agrees with the parent. `zip64_cap` and `component_cap` are immutable
    /// invariants forced by `ScanLimits::from_resolved` and are deliberately not serialized.
    pub fn child_env(&self) -> Vec<(OsString, OsString)> {
        let text = |value: u64| OsString::from(value.to_string());
        vec![
            (OOXML_ARCHIVE_ENV.into(), text(self.archive_bytes)),
            (OOXML_EXPANDED_ENV.into(), text(self.expanded_bytes)),
            (OOXML_XML_ENTRY_ENV.into(), text(self.xml_entry_bytes)),
            (OOXML_XML_TOTAL_ENV.into(), text(self.xml_total_bytes)),
            (OOXML_RATIO_ENV.into(), text(self.ratio)),
            (OOXML_XML_DEPTH_ENV.into(), text(self.xml_depth as u64)),
            (OOXML_XML_EVENTS_ENV.into(), text(self.xml_events as u64)),
            (
                OOXML_XML_ATTRIBUTES_ENV.into(),
                text(self.xml_attributes as u64),
            ),
            (
                OOXML_XML_NAMESPACES_ENV.into(),
                text(self.xml_namespaces as u64),
            ),
            (
                "MINERU_ARCHIVE_MAX_ENTRIES".into(),
                text(self.scan.max_entries),
            ),
            (
                "MINERU_ZIP_SCAN_CENTRAL_CAP".into(),
                text(self.scan.central_cap),
            ),
            (
                "MINERU_ZIP_SCAN_NAME_CAP".into(),
                text(self.scan.name_cap as u64),
            ),
            (
                "MINERU_ZIP_SCAN_DEPTH_CAP".into(),
                text(self.scan.depth_cap as u64),
            ),
            (
                "MINERU_ZIP_SCAN_TOTAL_NAME_CAP".into(),
                text(self.scan.total_name_cap),
            ),
            (
                "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP".into(),
                text(self.scan.total_component_cap),
            ),
        ]
    }

    /// Writes the resolved policy into an explicit child environment before spawn.
    pub fn apply_to_child_env(&self, command: &mut tokio::process::Command) {
        for (name, value) in self.child_env() {
            command.env(name, value);
        }
    }

    /// Reads the parent-provided child environment exactly once at helper startup.
    pub fn from_child_env() -> Self {
        let read_u64 = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
        };
        let read_usize = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|value| *value > 0)
        };
        let defaults = Self::default_resolved();
        // `read_*` already filters non-positive values and the parent wrote a validated policy,
        // so `from_resolved` cannot fail on the child side. `zip64_cap`/`component_cap` stay the
        // immutable compiled values regardless of the parent policy.
        let scan = ScanLimits::from_resolved(
            read_u64("MINERU_ARCHIVE_MAX_ENTRIES").unwrap_or(defaults.scan.max_entries),
            read_u64("MINERU_ZIP_SCAN_CENTRAL_CAP").unwrap_or(defaults.scan.central_cap),
            read_usize("MINERU_ZIP_SCAN_NAME_CAP").unwrap_or(defaults.scan.name_cap),
            read_usize("MINERU_ZIP_SCAN_DEPTH_CAP").unwrap_or(defaults.scan.depth_cap),
            read_u64("MINERU_ZIP_SCAN_TOTAL_NAME_CAP").unwrap_or(defaults.scan.total_name_cap),
            read_u64("MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP")
                .unwrap_or(defaults.scan.total_component_cap),
        )
        .expect("parent-resolved ZIP scan policy is valid");
        Self {
            archive_bytes: read_u64(OOXML_ARCHIVE_ENV).unwrap_or(defaults.archive_bytes),
            expanded_bytes: read_u64(OOXML_EXPANDED_ENV).unwrap_or(defaults.expanded_bytes),
            xml_entry_bytes: read_u64(OOXML_XML_ENTRY_ENV).unwrap_or(defaults.xml_entry_bytes),
            xml_total_bytes: read_u64(OOXML_XML_TOTAL_ENV).unwrap_or(defaults.xml_total_bytes),
            ratio: read_u64(OOXML_RATIO_ENV).unwrap_or(defaults.ratio),
            xml_depth: read_usize(OOXML_XML_DEPTH_ENV).unwrap_or(defaults.xml_depth),
            xml_events: read_usize(OOXML_XML_EVENTS_ENV).unwrap_or(defaults.xml_events),
            xml_attributes: read_usize(OOXML_XML_ATTRIBUTES_ENV).unwrap_or(defaults.xml_attributes),
            xml_namespaces: read_usize(OOXML_XML_NAMESPACES_ENV).unwrap_or(defaults.xml_namespaces),
            scan,
        }
    }
}

/// Resolved Office helper resource policy, frozen at startup and stored in `OfficeWorkers`.
///
/// The same values are written into the explicit child environment so the helper reads them
/// exactly once at startup and never re-reads a drifting process environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfficeLimits {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub stderr_bytes: usize,
    pub wall: Duration,
    pub cpu_seconds: u64,
    pub nofile: u64,
    pub address_space_bytes: u64,
    pub active_process_limit: u32,
    pub process_memory_bytes: u64,
    pub job_memory_bytes: u64,
    pub process_time_seconds: u64,
    pub job_time_seconds: u64,
}

impl Default for OfficeLimits {
    fn default() -> Self {
        Self {
            input_bytes: 32 * MIB as usize,
            output_bytes: 64 * MIB as usize,
            stderr_bytes: 4096,
            wall: Duration::from_secs(180),
            cpu_seconds: 120,
            nofile: 256,
            address_space_bytes: GIB,
            active_process_limit: 8,
            process_memory_bytes: GIB,
            job_memory_bytes: GIB,
            process_time_seconds: 120,
            job_time_seconds: 120,
        }
    }
}

impl OfficeLimits {
    pub fn resolve(
        environment: &impl Fn(&str) -> Option<OsString>,
        overrides: &ServiceOverrides,
    ) -> Result<Self, String> {
        let defaults = Self::default();
        let env = parse_service_overrides(environment)?;
        let merged = merge(&env, overrides);
        let input = merged.office_input_bytes.unwrap_or(defaults.input_bytes);
        let output = merged.office_output_bytes.unwrap_or(defaults.output_bytes);
        let stderr = merged.office_stderr_bytes.unwrap_or(defaults.stderr_bytes);
        let wall = Duration::from_secs(
            merged
                .office_wall_seconds
                .unwrap_or(defaults.wall.as_secs()),
        );
        let limits = Self {
            input_bytes: input,
            output_bytes: output,
            stderr_bytes: stderr,
            wall,
            cpu_seconds: merged.office_cpu_seconds.unwrap_or(defaults.cpu_seconds),
            nofile: merged.office_nofile.unwrap_or(defaults.nofile),
            address_space_bytes: merged
                .office_address_space_bytes
                .unwrap_or(defaults.address_space_bytes),
            active_process_limit: merged
                .office_active_process_limit
                .unwrap_or(defaults.active_process_limit),
            process_memory_bytes: merged
                .office_process_memory_bytes
                .unwrap_or(defaults.process_memory_bytes),
            job_memory_bytes: merged
                .office_job_memory_bytes
                .unwrap_or(defaults.job_memory_bytes),
            process_time_seconds: merged
                .office_process_time_seconds
                .unwrap_or(defaults.process_time_seconds),
            job_time_seconds: merged
                .office_job_time_seconds
                .unwrap_or(defaults.job_time_seconds),
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<Self, String> {
        if self.input_bytes == 0
            || self.output_bytes == 0
            || self.stderr_bytes == 0
            || self.wall.is_zero()
            || self.cpu_seconds == 0
            || self.nofile == 0
            || self.address_space_bytes == 0
            || self.active_process_limit == 0
            || self.process_memory_bytes == 0
            || self.job_memory_bytes == 0
            || self.process_time_seconds == 0
            || self.job_time_seconds == 0
        {
            return Err("office limits must be positive".into());
        }
        Ok(self)
    }

    /// Returns the resolved policy as explicit child-environment pairs.
    pub fn child_env(&self) -> Vec<(OsString, OsString)> {
        let text = |value: u64| OsString::from(value.to_string());
        let value = |seconds: u64| OsString::from(seconds.to_string());
        vec![
            (OFFICE_INPUT_ENV.into(), text(self.input_bytes as u64)),
            (OFFICE_OUTPUT_ENV.into(), text(self.output_bytes as u64)),
            (OFFICE_STDERR_ENV.into(), text(self.stderr_bytes as u64)),
            (OFFICE_WALL_ENV.into(), value(self.wall.as_secs())),
            (OFFICE_CPU_ENV.into(), text(self.cpu_seconds)),
            (OFFICE_NOFILE_ENV.into(), text(self.nofile)),
            (
                OFFICE_ADDRESS_SPACE_ENV.into(),
                text(self.address_space_bytes),
            ),
            (
                OFFICE_ACTIVE_PROCESS_ENV.into(),
                text(self.active_process_limit as u64),
            ),
            (
                OFFICE_PROCESS_MEMORY_ENV.into(),
                text(self.process_memory_bytes),
            ),
            (OFFICE_JOB_MEMORY_ENV.into(), text(self.job_memory_bytes)),
            (
                OFFICE_PROCESS_TIME_ENV.into(),
                text(self.process_time_seconds),
            ),
            (OFFICE_JOB_TIME_ENV.into(), text(self.job_time_seconds)),
        ]
    }

    /// Writes the resolved policy into an explicit child environment before spawn.
    pub fn apply_to_child_env(&self, command: &mut tokio::process::Command) {
        for (name, value) in self.child_env() {
            command.env(name, value);
        }
    }

    /// Reads the parent-provided child environment exactly once at helper startup.
    pub fn from_child_env() -> Self {
        let read_u64 = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
        };
        let read_usize = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .filter(|value| *value > 0)
        };
        let read_u32 = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .filter(|value| *value > 0)
        };
        let defaults = Self::default();
        Self {
            input_bytes: read_usize(OFFICE_INPUT_ENV).unwrap_or(defaults.input_bytes),
            output_bytes: read_usize(OFFICE_OUTPUT_ENV).unwrap_or(defaults.output_bytes),
            stderr_bytes: read_usize(OFFICE_STDERR_ENV).unwrap_or(defaults.stderr_bytes),
            wall: Duration::from_secs(read_u64(OFFICE_WALL_ENV).unwrap_or(defaults.wall.as_secs())),
            cpu_seconds: read_u64(OFFICE_CPU_ENV).unwrap_or(defaults.cpu_seconds),
            nofile: read_u64(OFFICE_NOFILE_ENV).unwrap_or(defaults.nofile),
            address_space_bytes: read_u64(OFFICE_ADDRESS_SPACE_ENV)
                .unwrap_or(defaults.address_space_bytes),
            active_process_limit: read_u32(OFFICE_ACTIVE_PROCESS_ENV)
                .unwrap_or(defaults.active_process_limit),
            process_memory_bytes: read_u64(OFFICE_PROCESS_MEMORY_ENV)
                .unwrap_or(defaults.process_memory_bytes),
            job_memory_bytes: read_u64(OFFICE_JOB_MEMORY_ENV).unwrap_or(defaults.job_memory_bytes),
            process_time_seconds: read_u64(OFFICE_PROCESS_TIME_ENV)
                .unwrap_or(defaults.process_time_seconds),
            job_time_seconds: read_u64(OFFICE_JOB_TIME_ENV).unwrap_or(defaults.job_time_seconds),
        }
    }
}

/// Task-service request caps, owned by the `mineru-api` CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerLimits {
    pub record_cap: usize,
    pub file_bytes: u64,
    pub body_bytes: usize,
    pub text_bytes: usize,
    pub text_total_bytes: usize,
    pub form_fields: usize,
}

impl ServerLimits {
    pub fn default_resolved() -> Self {
        Self {
            record_cap: 32,
            file_bytes: GIB,
            body_bytes: (GIB + MIB) as usize,
            text_bytes: 64 * 1024,
            text_total_bytes: 256 * 1024,
            form_fields: 32,
        }
    }

    pub fn resolve(
        environment: &impl Fn(&str) -> Option<OsString>,
        overrides: &ServiceOverrides,
    ) -> Result<Self, String> {
        let defaults = Self::default_resolved();
        let env = parse_service_overrides(environment)?;
        let merged = merge(&env, overrides);
        let limits = Self {
            record_cap: merged.server_record_cap.unwrap_or(defaults.record_cap),
            file_bytes: merged.server_file_cap.unwrap_or(defaults.file_bytes),
            body_bytes: merged.server_body_cap.unwrap_or(defaults.body_bytes),
            text_bytes: merged.server_text_cap.unwrap_or(defaults.text_bytes),
            text_total_bytes: merged
                .server_text_total_cap
                .unwrap_or(defaults.text_total_bytes),
            form_fields: merged
                .server_form_fields_cap
                .unwrap_or(defaults.form_fields),
        };
        limits.validate()?;
        Ok(limits)
    }

    pub fn validate(self) -> Result<Self, String> {
        if self.record_cap == 0
            || self.file_bytes == 0
            || self.body_bytes == 0
            || self.text_bytes == 0
            || self.text_total_bytes == 0
            || self.form_fields == 0
        {
            return Err("task-service limits must be positive".into());
        }
        Ok(self)
    }
}

/// Fully resolved Phase-1B policy snapshot, frozen once per parent CLI startup.
#[derive(Clone, Debug)]
pub struct ResolvedService {
    pub vlm_text_before_image: bool,
    pub vlm_allow_truncated_content: bool,
    pub vlm_allow_remote_images: bool,
    pub vlm_allow_private_remote_images: bool,
    pub remote_concurrency: usize,
    pub task_result_timeout: Duration,
    pub task_download_timeout: Duration,
    pub api_connect_timeout: Duration,
    pub api_acquisition_timeout: Duration,
    pub api_send_timeout: Duration,
    pub api_poll_interval: Duration,
    pub task_retention: Duration,
    pub task_cleanup_interval: Duration,
    pub archive: ArchiveLimits,
    pub scan: ScanLimits,
    pub ooxml: OoxmlLimits,
    pub office: OfficeLimits,
    pub server: ServerLimits,
}

fn merge(base: &ServiceOverrides, cli: &ServiceOverrides) -> ServiceOverrides {
    fn pick<T: Clone>(base: &Option<T>, cli: &Option<T>) -> Option<T> {
        cli.clone().or_else(|| base.clone())
    }
    ServiceOverrides {
        vlm_text_before_image: pick(&base.vlm_text_before_image, &cli.vlm_text_before_image),
        vlm_allow_truncated_content: pick(
            &base.vlm_allow_truncated_content,
            &cli.vlm_allow_truncated_content,
        ),
        vlm_allow_remote_images: pick(&base.vlm_allow_remote_images, &cli.vlm_allow_remote_images),
        vlm_allow_private_remote_images: pick(
            &base.vlm_allow_private_remote_images,
            &cli.vlm_allow_private_remote_images,
        ),
        api_max_concurrent_requests: pick(
            &base.api_max_concurrent_requests,
            &cli.api_max_concurrent_requests,
        ),
        task_result_timeout: pick(&base.task_result_timeout, &cli.task_result_timeout),
        task_download_timeout: pick(&base.task_download_timeout, &cli.task_download_timeout),
        api_connect_timeout: pick(&base.api_connect_timeout, &cli.api_connect_timeout),
        api_acquisition_timeout: pick(&base.api_acquisition_timeout, &cli.api_acquisition_timeout),
        api_send_timeout: pick(&base.api_send_timeout, &cli.api_send_timeout),
        api_poll_interval: pick(&base.api_poll_interval, &cli.api_poll_interval),
        task_retention: pick(&base.task_retention, &cli.task_retention),
        task_cleanup_interval: pick(&base.task_cleanup_interval, &cli.task_cleanup_interval),
        archive_max_entries: pick(&base.archive_max_entries, &cli.archive_max_entries),
        archive_max_ratio: pick(&base.archive_max_ratio, &cli.archive_max_ratio),
        zip_central_cap: pick(&base.zip_central_cap, &cli.zip_central_cap),
        zip_name_cap: pick(&base.zip_name_cap, &cli.zip_name_cap),
        zip_depth_cap: pick(&base.zip_depth_cap, &cli.zip_depth_cap),
        zip_total_name_cap: pick(&base.zip_total_name_cap, &cli.zip_total_name_cap),
        zip_total_component_cap: pick(&base.zip_total_component_cap, &cli.zip_total_component_cap),
        ooxml_archive_bytes: pick(&base.ooxml_archive_bytes, &cli.ooxml_archive_bytes),
        ooxml_expanded_bytes: pick(&base.ooxml_expanded_bytes, &cli.ooxml_expanded_bytes),
        ooxml_xml_entry_bytes: pick(&base.ooxml_xml_entry_bytes, &cli.ooxml_xml_entry_bytes),
        ooxml_xml_total_bytes: pick(&base.ooxml_xml_total_bytes, &cli.ooxml_xml_total_bytes),
        ooxml_ratio: pick(&base.ooxml_ratio, &cli.ooxml_ratio),
        ooxml_xml_depth: pick(&base.ooxml_xml_depth, &cli.ooxml_xml_depth),
        ooxml_xml_events: pick(&base.ooxml_xml_events, &cli.ooxml_xml_events),
        ooxml_xml_attributes: pick(&base.ooxml_xml_attributes, &cli.ooxml_xml_attributes),
        ooxml_xml_namespaces: pick(&base.ooxml_xml_namespaces, &cli.ooxml_xml_namespaces),
        office_input_bytes: pick(&base.office_input_bytes, &cli.office_input_bytes),
        office_output_bytes: pick(&base.office_output_bytes, &cli.office_output_bytes),
        office_stderr_bytes: pick(&base.office_stderr_bytes, &cli.office_stderr_bytes),
        office_wall_seconds: pick(&base.office_wall_seconds, &cli.office_wall_seconds),
        office_cpu_seconds: pick(&base.office_cpu_seconds, &cli.office_cpu_seconds),
        office_nofile: pick(&base.office_nofile, &cli.office_nofile),
        office_address_space_bytes: pick(
            &base.office_address_space_bytes,
            &cli.office_address_space_bytes,
        ),
        office_active_process_limit: pick(
            &base.office_active_process_limit,
            &cli.office_active_process_limit,
        ),
        office_process_memory_bytes: pick(
            &base.office_process_memory_bytes,
            &cli.office_process_memory_bytes,
        ),
        office_job_memory_bytes: pick(&base.office_job_memory_bytes, &cli.office_job_memory_bytes),
        office_process_time_seconds: pick(
            &base.office_process_time_seconds,
            &cli.office_process_time_seconds,
        ),
        office_job_time_seconds: pick(&base.office_job_time_seconds, &cli.office_job_time_seconds),
        server_record_cap: pick(&base.server_record_cap, &cli.server_record_cap),
        server_file_cap: pick(&base.server_file_cap, &cli.server_file_cap),
        server_body_cap: pick(&base.server_body_cap, &cli.server_body_cap),
        server_text_cap: pick(&base.server_text_cap, &cli.server_text_cap),
        server_text_total_cap: pick(&base.server_text_total_cap, &cli.server_text_total_cap),
        server_form_fields_cap: pick(&base.server_form_fields_cap, &cli.server_form_fields_cap),
    }
}

/// Strictly parses the frozen service environment into typed overrides.
pub fn parse_service_overrides(
    lookup: &impl Fn(&str) -> Option<OsString>,
) -> Result<ServiceOverrides, String> {
    Ok(ServiceOverrides {
        vlm_text_before_image: strict_bool(
            lookup("MINERU_VLM_TEXT_BEFORE_IMAGE"),
            "MINERU_VLM_TEXT_BEFORE_IMAGE",
        )?,
        vlm_allow_truncated_content: strict_bool(
            lookup("MINERU_VLM_ALLOW_TRUNCATED_CONTENT"),
            "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
        )?,
        vlm_allow_remote_images: strict_bool(
            lookup("MINERU_VLM_ALLOW_REMOTE_IMAGES"),
            "MINERU_VLM_ALLOW_REMOTE_IMAGES",
        )?,
        vlm_allow_private_remote_images: strict_bool(
            lookup("MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES"),
            "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
        )?,
        api_max_concurrent_requests: positive_usize(
            lookup("MINERU_API_MAX_CONCURRENT_REQUESTS"),
            "MINERU_API_MAX_CONCURRENT_REQUESTS",
        )?,
        task_result_timeout: positive_seconds(
            lookup("MINERU_TASK_RESULT_TIMEOUT_SECONDS"),
            "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
        )?,
        task_download_timeout: positive_seconds(
            lookup("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS"),
            "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
        )?,
        task_retention: positive_seconds(
            lookup("MINERU_API_TASK_RETENTION_SECONDS"),
            "MINERU_API_TASK_RETENTION_SECONDS",
        )?,
        task_cleanup_interval: positive_seconds(
            lookup("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS"),
            "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
        )?,
        api_connect_timeout: positive_seconds(
            lookup("MINERU_API_CONNECT_TIMEOUT_SECONDS"),
            "MINERU_API_CONNECT_TIMEOUT_SECONDS",
        )?,
        api_acquisition_timeout: positive_seconds(
            lookup("MINERU_API_ACQUISITION_TIMEOUT_SECONDS"),
            "MINERU_API_ACQUISITION_TIMEOUT_SECONDS",
        )?,
        api_send_timeout: positive_seconds(
            lookup("MINERU_API_SEND_TIMEOUT_SECONDS"),
            "MINERU_API_SEND_TIMEOUT_SECONDS",
        )?,
        api_poll_interval: positive_seconds(
            lookup("MINERU_API_POLL_INTERVAL_SECONDS"),
            "MINERU_API_POLL_INTERVAL_SECONDS",
        )?,
        archive_max_entries: positive_u64(
            lookup("MINERU_ARCHIVE_MAX_ENTRIES"),
            "MINERU_ARCHIVE_MAX_ENTRIES",
        )?,
        archive_max_ratio: positive_u64(
            lookup("MINERU_ARCHIVE_MAX_RATIO"),
            "MINERU_ARCHIVE_MAX_RATIO",
        )?,
        zip_central_cap: positive_u64(
            lookup("MINERU_ZIP_SCAN_CENTRAL_CAP"),
            "MINERU_ZIP_SCAN_CENTRAL_CAP",
        )?,
        zip_name_cap: positive_usize(
            lookup("MINERU_ZIP_SCAN_NAME_CAP"),
            "MINERU_ZIP_SCAN_NAME_CAP",
        )?,
        zip_depth_cap: positive_usize(
            lookup("MINERU_ZIP_SCAN_DEPTH_CAP"),
            "MINERU_ZIP_SCAN_DEPTH_CAP",
        )?,
        zip_total_name_cap: positive_u64(
            lookup("MINERU_ZIP_SCAN_TOTAL_NAME_CAP"),
            "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
        )?,
        zip_total_component_cap: positive_u64(
            lookup("MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP"),
            "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
        )?,
        ooxml_archive_bytes: positive_u64(
            lookup("MINERU_OOXML_ARCHIVE_BYTES"),
            "MINERU_OOXML_ARCHIVE_BYTES",
        )?,
        ooxml_expanded_bytes: positive_u64(
            lookup("MINERU_OOXML_EXPANDED_BYTES"),
            "MINERU_OOXML_EXPANDED_BYTES",
        )?,
        ooxml_xml_entry_bytes: positive_u64(
            lookup("MINERU_OOXML_XML_ENTRY_BYTES"),
            "MINERU_OOXML_XML_ENTRY_BYTES",
        )?,
        ooxml_xml_total_bytes: positive_u64(
            lookup("MINERU_OOXML_XML_TOTAL_BYTES"),
            "MINERU_OOXML_XML_TOTAL_BYTES",
        )?,
        ooxml_ratio: positive_u64(lookup("MINERU_OOXML_RATIO"), "MINERU_OOXML_RATIO")?,
        ooxml_xml_depth: positive_usize(
            lookup("MINERU_OOXML_XML_DEPTH"),
            "MINERU_OOXML_XML_DEPTH",
        )?,
        ooxml_xml_events: positive_usize(
            lookup("MINERU_OOXML_XML_EVENTS"),
            "MINERU_OOXML_XML_EVENTS",
        )?,
        ooxml_xml_attributes: positive_usize(
            lookup("MINERU_OOXML_XML_ATTRIBUTES"),
            "MINERU_OOXML_XML_ATTRIBUTES",
        )?,
        ooxml_xml_namespaces: positive_usize(
            lookup("MINERU_OOXML_XML_NAMESPACES"),
            "MINERU_OOXML_XML_NAMESPACES",
        )?,
        office_input_bytes: positive_usize(lookup(OFFICE_INPUT_ENV), OFFICE_INPUT_ENV)?,
        office_output_bytes: positive_usize(lookup(OFFICE_OUTPUT_ENV), OFFICE_OUTPUT_ENV)?,
        office_stderr_bytes: positive_usize(lookup(OFFICE_STDERR_ENV), OFFICE_STDERR_ENV)?,
        office_wall_seconds: positive_u64(lookup(OFFICE_WALL_ENV), OFFICE_WALL_ENV)?,
        office_cpu_seconds: positive_u64(lookup(OFFICE_CPU_ENV), OFFICE_CPU_ENV)?,
        office_nofile: positive_u64(lookup(OFFICE_NOFILE_ENV), OFFICE_NOFILE_ENV)?,
        office_address_space_bytes: positive_u64(
            lookup(OFFICE_ADDRESS_SPACE_ENV),
            OFFICE_ADDRESS_SPACE_ENV,
        )?,
        office_active_process_limit: positive_u32(
            lookup(OFFICE_ACTIVE_PROCESS_ENV),
            OFFICE_ACTIVE_PROCESS_ENV,
        )?,
        office_process_memory_bytes: positive_u64(
            lookup(OFFICE_PROCESS_MEMORY_ENV),
            OFFICE_PROCESS_MEMORY_ENV,
        )?,
        office_job_memory_bytes: positive_u64(
            lookup(OFFICE_JOB_MEMORY_ENV),
            OFFICE_JOB_MEMORY_ENV,
        )?,
        office_process_time_seconds: positive_u64(
            lookup(OFFICE_PROCESS_TIME_ENV),
            OFFICE_PROCESS_TIME_ENV,
        )?,
        office_job_time_seconds: positive_u64(lookup(OFFICE_JOB_TIME_ENV), OFFICE_JOB_TIME_ENV)?,
        server_record_cap: positive_usize(
            lookup("MINERU_API_RECORD_CAP"),
            "MINERU_API_RECORD_CAP",
        )?,
        server_file_cap: positive_u64(lookup("MINERU_API_FILE_CAP"), "MINERU_API_FILE_CAP")?,
        server_body_cap: positive_usize(lookup("MINERU_API_BODY_CAP"), "MINERU_API_BODY_CAP")?,
        server_text_cap: positive_usize(lookup("MINERU_API_TEXT_CAP"), "MINERU_API_TEXT_CAP")?,
        server_text_total_cap: positive_usize(
            lookup("MINERU_API_TEXT_TOTAL_CAP"),
            "MINERU_API_TEXT_TOTAL_CAP",
        )?,
        server_form_fields_cap: positive_usize(
            lookup("MINERU_API_FORM_FIELDS_CAP"),
            "MINERU_API_FORM_FIELDS_CAP",
        )?,
    })
}

/// Resolves Phase-1B policy with precedence compiled default -> frozen environment -> explicit CLI.
pub fn resolve_service(
    environment: &impl Fn(&str) -> Option<OsString>,
    cli: &ServiceOverrides,
    document_limits: crate::DocumentLimitPolicy,
) -> Result<ResolvedService, String> {
    let env = parse_service_overrides(environment)?;
    let merged = merge(&env, cli);

    let remote_concurrency = merged.api_max_concurrent_requests.unwrap_or(3);
    if remote_concurrency == 0 || remote_concurrency > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(
            "MINERU_API_MAX_CONCURRENT_REQUESTS must be positive and at most the tokio semaphore capacity"
                .into(),
        );
    }
    let task_result_timeout = merged
        .task_result_timeout
        .unwrap_or_else(|| Duration::from_secs(3600));
    let task_download_timeout = merged
        .task_download_timeout
        .unwrap_or_else(|| Duration::from_secs(600));
    let api_connect_timeout = merged
        .api_connect_timeout
        .unwrap_or_else(|| Duration::from_secs(10));
    let api_acquisition_timeout = merged
        .api_acquisition_timeout
        .unwrap_or_else(|| Duration::from_secs(60));
    let api_send_timeout = merged
        .api_send_timeout
        .unwrap_or_else(|| Duration::from_secs(300));
    let api_poll_interval = merged
        .api_poll_interval
        .unwrap_or_else(|| Duration::from_secs(1));
    let task_retention = merged
        .task_retention
        .unwrap_or_else(|| Duration::from_secs(24 * 60 * 60));
    let task_cleanup_interval = merged
        .task_cleanup_interval
        .unwrap_or_else(|| Duration::from_secs(300));
    if task_result_timeout.is_zero() || task_download_timeout.is_zero() {
        return Err("task timing must be positive".into());
    }
    if task_retention.is_zero() || task_cleanup_interval.is_zero() {
        return Err("task lifecycle timing must be positive".into());
    }

    let scan = scan_limits(&merged)?;
    let ooxml_defaults = OoxmlLimits::default_resolved();
    let ooxml = OoxmlLimits {
        archive_bytes: merged
            .ooxml_archive_bytes
            .unwrap_or(ooxml_defaults.archive_bytes),
        expanded_bytes: merged
            .ooxml_expanded_bytes
            .unwrap_or(ooxml_defaults.expanded_bytes),
        xml_entry_bytes: merged
            .ooxml_xml_entry_bytes
            .unwrap_or(ooxml_defaults.xml_entry_bytes),
        xml_total_bytes: merged
            .ooxml_xml_total_bytes
            .unwrap_or(ooxml_defaults.xml_total_bytes),
        ratio: merged.ooxml_ratio.unwrap_or(ooxml_defaults.ratio),
        xml_depth: merged.ooxml_xml_depth.unwrap_or(ooxml_defaults.xml_depth),
        xml_events: merged.ooxml_xml_events.unwrap_or(ooxml_defaults.xml_events),
        xml_attributes: merged
            .ooxml_xml_attributes
            .unwrap_or(ooxml_defaults.xml_attributes),
        xml_namespaces: merged
            .ooxml_xml_namespaces
            .unwrap_or(ooxml_defaults.xml_namespaces),
        scan,
    }
    .validate()?;

    let archive = archive_limits(document_limits, &merged, scan)?;
    let office_defaults = OfficeLimits::default();
    let office = OfficeLimits {
        input_bytes: merged
            .office_input_bytes
            .unwrap_or(office_defaults.input_bytes),
        output_bytes: merged
            .office_output_bytes
            .unwrap_or(office_defaults.output_bytes),
        stderr_bytes: merged
            .office_stderr_bytes
            .unwrap_or(office_defaults.stderr_bytes),
        wall: Duration::from_secs(
            merged
                .office_wall_seconds
                .unwrap_or(office_defaults.wall.as_secs()),
        ),
        cpu_seconds: merged
            .office_cpu_seconds
            .unwrap_or(office_defaults.cpu_seconds),
        nofile: merged.office_nofile.unwrap_or(office_defaults.nofile),
        address_space_bytes: merged
            .office_address_space_bytes
            .unwrap_or(office_defaults.address_space_bytes),
        active_process_limit: merged
            .office_active_process_limit
            .unwrap_or(office_defaults.active_process_limit),
        process_memory_bytes: merged
            .office_process_memory_bytes
            .unwrap_or(office_defaults.process_memory_bytes),
        job_memory_bytes: merged
            .office_job_memory_bytes
            .unwrap_or(office_defaults.job_memory_bytes),
        process_time_seconds: merged
            .office_process_time_seconds
            .unwrap_or(office_defaults.process_time_seconds),
        job_time_seconds: merged
            .office_job_time_seconds
            .unwrap_or(office_defaults.job_time_seconds),
    }
    .validate()?;

    Ok(ResolvedService {
        vlm_text_before_image: merged.vlm_text_before_image.unwrap_or(false),
        vlm_allow_truncated_content: merged.vlm_allow_truncated_content.unwrap_or(false),
        vlm_allow_remote_images: merged.vlm_allow_remote_images.unwrap_or(false),
        vlm_allow_private_remote_images: merged.vlm_allow_private_remote_images.unwrap_or(false),
        remote_concurrency,
        task_result_timeout,
        task_download_timeout,
        api_connect_timeout,
        api_acquisition_timeout,
        api_send_timeout,
        api_poll_interval,
        task_retention,
        task_cleanup_interval,
        archive,
        scan,
        ooxml,
        office,
        server: ServerLimits::resolve(environment, &merged)?,
    })
}

fn archive_limits(
    policy: crate::DocumentLimitPolicy,
    merged: &ServiceOverrides,
    scan: ScanLimits,
) -> Result<ArchiveLimits, String> {
    let defaults = ArchiveLimits::default();
    let entries = merged.archive_max_entries.unwrap_or(defaults.max_entries);
    let ratio = merged.archive_max_ratio.unwrap_or(defaults.max_ratio);
    ArchiveLimits::from_document_limits_with_operator(policy, entries, ratio, scan)
}

fn scan_limits(merged: &ServiceOverrides) -> Result<ScanLimits, String> {
    let defaults = ArchiveLimits::default().scan;
    ScanLimits::from_resolved(
        merged.archive_max_entries.unwrap_or(defaults.max_entries),
        merged.zip_central_cap.unwrap_or(defaults.central_cap),
        merged.zip_name_cap.unwrap_or(defaults.name_cap),
        merged.zip_depth_cap.unwrap_or(defaults.depth_cap),
        merged.zip_total_name_cap.unwrap_or(defaults.total_name_cap),
        merged
            .zip_total_component_cap
            .unwrap_or(defaults.total_component_cap),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_map<'a>(values: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn all_names() -> Vec<&'static str> {
        vec![
            "MINERU_VLM_TEXT_BEFORE_IMAGE",
            "MINERU_VLM_ALLOW_TRUNCATED_CONTENT",
            "MINERU_VLM_ALLOW_REMOTE_IMAGES",
            "MINERU_VLM_ALLOW_PRIVATE_REMOTE_IMAGES",
            "MINERU_API_MAX_CONCURRENT_REQUESTS",
            "MINERU_TASK_RESULT_TIMEOUT_SECONDS",
            "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS",
            "MINERU_API_CONNECT_TIMEOUT_SECONDS",
            "MINERU_API_ACQUISITION_TIMEOUT_SECONDS",
            "MINERU_API_SEND_TIMEOUT_SECONDS",
            "MINERU_API_POLL_INTERVAL_SECONDS",
            "MINERU_API_TASK_RETENTION_SECONDS",
            "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
            "MINERU_ARCHIVE_MAX_ENTRIES",
            "MINERU_ARCHIVE_MAX_RATIO",
            "MINERU_ZIP_SCAN_CENTRAL_CAP",
            "MINERU_ZIP_SCAN_NAME_CAP",
            "MINERU_ZIP_SCAN_DEPTH_CAP",
            "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
            "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
            "MINERU_OOXML_ARCHIVE_BYTES",
            "MINERU_OOXML_EXPANDED_BYTES",
            "MINERU_OOXML_XML_ENTRY_BYTES",
            "MINERU_OOXML_XML_TOTAL_BYTES",
            "MINERU_OOXML_RATIO",
            "MINERU_OOXML_XML_DEPTH",
            "MINERU_OOXML_XML_EVENTS",
            "MINERU_OOXML_XML_ATTRIBUTES",
            "MINERU_OOXML_XML_NAMESPACES",
            OFFICE_INPUT_ENV,
            OFFICE_OUTPUT_ENV,
            OFFICE_STDERR_ENV,
            OFFICE_WALL_ENV,
            OFFICE_CPU_ENV,
            OFFICE_NOFILE_ENV,
            OFFICE_ADDRESS_SPACE_ENV,
            OFFICE_ACTIVE_PROCESS_ENV,
            OFFICE_PROCESS_MEMORY_ENV,
            OFFICE_JOB_MEMORY_ENV,
            OFFICE_PROCESS_TIME_ENV,
            OFFICE_JOB_TIME_ENV,
            "MINERU_API_RECORD_CAP",
            "MINERU_API_FILE_CAP",
            "MINERU_API_BODY_CAP",
            "MINERU_API_TEXT_CAP",
            "MINERU_API_TEXT_TOTAL_CAP",
            "MINERU_API_FORM_FIELDS_CAP",
        ]
    }

    #[test]
    fn parse_service_overrides_is_strict_about_boundaries() {
        let values = [
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "17"),
            ("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "1800"),
            ("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "300"),
            ("MINERU_API_CONNECT_TIMEOUT_SECONDS", "5"),
            ("MINERU_API_ACQUISITION_TIMEOUT_SECONDS", "30"),
            ("MINERU_API_SEND_TIMEOUT_SECONDS", "120"),
            ("MINERU_API_POLL_INTERVAL_SECONDS", "2"),
            ("MINERU_API_TASK_RETENTION_SECONDS", "100"),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", "200"),
            ("MINERU_ARCHIVE_MAX_ENTRIES", "7"),
            ("MINERU_ARCHIVE_MAX_RATIO", "11"),
            ("MINERU_ZIP_SCAN_CENTRAL_CAP", "13"),
            ("MINERU_ZIP_SCAN_NAME_CAP", "17"),
            ("MINERU_ZIP_SCAN_DEPTH_CAP", "19"),
            ("MINERU_ZIP_SCAN_TOTAL_NAME_CAP", "23"),
            ("MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP", "29"),
            ("MINERU_OOXML_ARCHIVE_BYTES", "31"),
            ("MINERU_OOXML_EXPANDED_BYTES", "37"),
            ("MINERU_OOXML_XML_ENTRY_BYTES", "41"),
            ("MINERU_OOXML_XML_TOTAL_BYTES", "43"),
            ("MINERU_OOXML_RATIO", "47"),
            ("MINERU_OOXML_XML_DEPTH", "53"),
            ("MINERU_OOXML_XML_EVENTS", "59"),
            ("MINERU_OOXML_XML_ATTRIBUTES", "61"),
            ("MINERU_OOXML_XML_NAMESPACES", "67"),
            (OFFICE_INPUT_ENV, "71"),
            (OFFICE_OUTPUT_ENV, "73"),
            (OFFICE_STDERR_ENV, "79"),
            (OFFICE_WALL_ENV, "83"),
            (OFFICE_CPU_ENV, "89"),
            (OFFICE_NOFILE_ENV, "97"),
            (OFFICE_ADDRESS_SPACE_ENV, "101"),
            (OFFICE_ACTIVE_PROCESS_ENV, "103"),
            (OFFICE_PROCESS_MEMORY_ENV, "107"),
            (OFFICE_JOB_MEMORY_ENV, "109"),
            (OFFICE_PROCESS_TIME_ENV, "113"),
            (OFFICE_JOB_TIME_ENV, "127"),
            ("MINERU_API_RECORD_CAP", "131"),
            ("MINERU_API_FILE_CAP", "137"),
            ("MINERU_API_BODY_CAP", "139"),
            ("MINERU_API_TEXT_CAP", "149"),
            ("MINERU_API_TEXT_TOTAL_CAP", "151"),
            ("MINERU_API_FORM_FIELDS_CAP", "157"),
        ];
        let overrides = parse_service_overrides(&lookup_map(&values)).unwrap();
        assert_eq!(overrides.api_max_concurrent_requests, Some(17));
        assert_eq!(
            overrides.task_result_timeout,
            Some(Duration::from_secs(1800))
        );
        assert_eq!(
            overrides.task_download_timeout,
            Some(Duration::from_secs(300))
        );
        assert_eq!(overrides.archive_max_entries, Some(7));
        assert_eq!(overrides.zip_depth_cap, Some(19));
        assert_eq!(overrides.ooxml_xml_namespaces, Some(67));
        assert_eq!(overrides.office_wall_seconds, Some(83));
        assert_eq!(overrides.office_active_process_limit, Some(103));
        assert_eq!(overrides.server_record_cap, Some(131));
        assert_eq!(overrides.server_form_fields_cap, Some(157));
    }

    #[test]
    fn parse_service_overrides_rejects_malformed_and_non_finite_values() {
        for (name, value) in [
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "0"),
            ("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "0"),
            ("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "-1"),
            ("MINERU_API_CONNECT_TIMEOUT_SECONDS", "bad"),
            ("MINERU_API_ACQUISITION_TIMEOUT_SECONDS", "1.5"),
            ("MINERU_API_SEND_TIMEOUT_SECONDS", "1e3"),
            ("MINERU_API_POLL_INTERVAL_SECONDS", ""),
            ("MINERU_API_TASK_RETENTION_SECONDS", "0"),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", "0"),
            ("MINERU_ARCHIVE_MAX_ENTRIES", "0"),
            ("MINERU_ARCHIVE_MAX_RATIO", "-0"),
            ("MINERU_ZIP_SCAN_CENTRAL_CAP", "+5"),
            ("MINERU_ZIP_SCAN_NAME_CAP", "1__0"),
            ("MINERU_ZIP_SCAN_DEPTH_CAP", "text"),
            ("MINERU_ZIP_SCAN_TOTAL_NAME_CAP", "0"),
            (
                "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
                "18446744073709551616",
            ),
            ("MINERU_OOXML_ARCHIVE_BYTES", "0"),
            ("MINERU_OOXML_EXPANDED_BYTES", "  "),
            ("MINERU_OOXML_XML_ENTRY_BYTES", "0"),
            ("MINERU_OOXML_XML_TOTAL_BYTES", "0"),
            ("MINERU_OOXML_RATIO", "0"),
            ("MINERU_OOXML_XML_DEPTH", "0"),
            ("MINERU_OOXML_XML_EVENTS", "0"),
            ("MINERU_OOXML_XML_ATTRIBUTES", "0"),
            ("MINERU_OOXML_XML_NAMESPACES", "0"),
            (OFFICE_INPUT_ENV, "0"),
            (OFFICE_OUTPUT_ENV, "0"),
            (OFFICE_STDERR_ENV, "0"),
            (OFFICE_WALL_ENV, "0"),
            (OFFICE_CPU_ENV, "0"),
            (OFFICE_NOFILE_ENV, "0"),
            (OFFICE_ADDRESS_SPACE_ENV, "0"),
            (OFFICE_ACTIVE_PROCESS_ENV, "0"),
            (OFFICE_ACTIVE_PROCESS_ENV, "4294967296"),
            (OFFICE_PROCESS_MEMORY_ENV, "0"),
            (OFFICE_JOB_MEMORY_ENV, "0"),
            (OFFICE_PROCESS_TIME_ENV, "0"),
            (OFFICE_JOB_TIME_ENV, "0"),
            ("MINERU_API_RECORD_CAP", "0"),
            ("MINERU_API_FILE_CAP", "0"),
            ("MINERU_API_BODY_CAP", "0"),
            ("MINERU_API_TEXT_CAP", "0"),
            ("MINERU_API_TEXT_TOTAL_CAP", "0"),
            ("MINERU_API_FORM_FIELDS_CAP", "0"),
        ] {
            let entry = [(name, value)];
            assert!(
                parse_service_overrides(&lookup_map(&entry)).is_err(),
                "{name}={value} must be rejected"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_service_overrides_rejects_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        for name in [
            "MINERU_ARCHIVE_MAX_ENTRIES",
            OFFICE_CPU_ENV,
            "MINERU_API_RECORD_CAP",
        ] {
            let lookup =
                |candidate: &str| (candidate == name).then(|| OsString::from_vec(vec![0xff]));
            assert!(parse_service_overrides(&lookup).is_err(), "{name}");
        }
    }

    fn render(resolved: &ResolvedService, name: &str) -> String {
        match name {
            "MINERU_API_MAX_CONCURRENT_REQUESTS" => resolved.remote_concurrency.to_string(),
            "MINERU_TASK_RESULT_TIMEOUT_SECONDS" => {
                resolved.task_result_timeout.as_secs().to_string()
            }
            "MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS" => {
                resolved.task_download_timeout.as_secs().to_string()
            }
            "MINERU_API_CONNECT_TIMEOUT_SECONDS" => {
                resolved.api_connect_timeout.as_secs().to_string()
            }
            "MINERU_API_ACQUISITION_TIMEOUT_SECONDS" => {
                resolved.api_acquisition_timeout.as_secs().to_string()
            }
            "MINERU_API_SEND_TIMEOUT_SECONDS" => resolved.api_send_timeout.as_secs().to_string(),
            "MINERU_API_POLL_INTERVAL_SECONDS" => resolved.api_poll_interval.as_secs().to_string(),
            "MINERU_API_TASK_RETENTION_SECONDS" => resolved.task_retention.as_secs().to_string(),
            "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS" => {
                resolved.task_cleanup_interval.as_secs().to_string()
            }
            "MINERU_ARCHIVE_MAX_ENTRIES" => resolved.archive.max_entries.to_string(),
            "MINERU_ARCHIVE_MAX_RATIO" => resolved.archive.max_ratio.to_string(),
            "MINERU_ZIP_SCAN_CENTRAL_CAP" => resolved.scan.central_cap.to_string(),
            "MINERU_ZIP_SCAN_NAME_CAP" => resolved.scan.name_cap.to_string(),
            "MINERU_ZIP_SCAN_DEPTH_CAP" => resolved.scan.depth_cap.to_string(),
            "MINERU_ZIP_SCAN_TOTAL_NAME_CAP" => resolved.scan.total_name_cap.to_string(),
            "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP" => resolved.scan.total_component_cap.to_string(),
            "MINERU_OOXML_ARCHIVE_BYTES" => resolved.ooxml.archive_bytes.to_string(),
            "MINERU_OOXML_EXPANDED_BYTES" => resolved.ooxml.expanded_bytes.to_string(),
            "MINERU_OOXML_XML_ENTRY_BYTES" => resolved.ooxml.xml_entry_bytes.to_string(),
            "MINERU_OOXML_XML_TOTAL_BYTES" => resolved.ooxml.xml_total_bytes.to_string(),
            "MINERU_OOXML_RATIO" => resolved.ooxml.ratio.to_string(),
            "MINERU_OOXML_XML_DEPTH" => resolved.ooxml.xml_depth.to_string(),
            "MINERU_OOXML_XML_EVENTS" => resolved.ooxml.xml_events.to_string(),
            "MINERU_OOXML_XML_ATTRIBUTES" => resolved.ooxml.xml_attributes.to_string(),
            "MINERU_OOXML_XML_NAMESPACES" => resolved.ooxml.xml_namespaces.to_string(),
            OFFICE_INPUT_ENV => resolved.office.input_bytes.to_string(),
            OFFICE_OUTPUT_ENV => resolved.office.output_bytes.to_string(),
            OFFICE_STDERR_ENV => resolved.office.stderr_bytes.to_string(),
            OFFICE_WALL_ENV => resolved.office.wall.as_secs().to_string(),
            OFFICE_CPU_ENV => resolved.office.cpu_seconds.to_string(),
            OFFICE_NOFILE_ENV => resolved.office.nofile.to_string(),
            OFFICE_ADDRESS_SPACE_ENV => resolved.office.address_space_bytes.to_string(),
            OFFICE_ACTIVE_PROCESS_ENV => resolved.office.active_process_limit.to_string(),
            OFFICE_PROCESS_MEMORY_ENV => resolved.office.process_memory_bytes.to_string(),
            OFFICE_JOB_MEMORY_ENV => resolved.office.job_memory_bytes.to_string(),
            OFFICE_PROCESS_TIME_ENV => resolved.office.process_time_seconds.to_string(),
            OFFICE_JOB_TIME_ENV => resolved.office.job_time_seconds.to_string(),
            _ => unreachable!("unexpected knob {name}"),
        }
    }

    /// Table-driven precedence proof: compiled default -> frozen environment -> explicit CLI,
    /// with strict malformed-env rejection for every Phase-1B knob. The expected default string
    /// is rendered from the resolved no-config policy so the table cannot drift from the
    /// compiled defaults.
    #[test]
    fn every_service_knob_obeys_default_env_cli_precedence_and_strictness() {
        const TABLE: &[(&str, &str, &str)] = &[
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "5", "7"),
            ("MINERU_TASK_RESULT_TIMEOUT_SECONDS", "3601", "3602"),
            ("MINERU_TASK_RESULT_DOWNLOAD_TIMEOUT_SECONDS", "601", "602"),
            ("MINERU_API_CONNECT_TIMEOUT_SECONDS", "11", "12"),
            ("MINERU_API_ACQUISITION_TIMEOUT_SECONDS", "61", "62"),
            ("MINERU_API_SEND_TIMEOUT_SECONDS", "301", "302"),
            ("MINERU_API_POLL_INTERVAL_SECONDS", "2", "3"),
            ("MINERU_API_TASK_RETENTION_SECONDS", "86401", "86402"),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", "301", "302"),
            ("MINERU_ARCHIVE_MAX_ENTRIES", "200", "300"),
            ("MINERU_ARCHIVE_MAX_RATIO", "1001", "1002"),
            ("MINERU_ZIP_SCAN_CENTRAL_CAP", "401", "402"),
            ("MINERU_ZIP_SCAN_NAME_CAP", "500", "600"),
            ("MINERU_ZIP_SCAN_DEPTH_CAP", "65", "66"),
            ("MINERU_ZIP_SCAN_TOTAL_NAME_CAP", "701", "702"),
            ("MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP", "801", "802"),
            ("MINERU_OOXML_ARCHIVE_BYTES", "901", "902"),
            ("MINERU_OOXML_EXPANDED_BYTES", "1001", "1002"),
            ("MINERU_OOXML_XML_ENTRY_BYTES", "1101", "1102"),
            ("MINERU_OOXML_XML_TOTAL_BYTES", "1201", "1202"),
            ("MINERU_OOXML_RATIO", "501", "502"),
            ("MINERU_OOXML_XML_DEPTH", "129", "130"),
            ("MINERU_OOXML_XML_EVENTS", "131", "132"),
            ("MINERU_OOXML_XML_ATTRIBUTES", "133", "134"),
            ("MINERU_OOXML_XML_NAMESPACES", "135", "136"),
            (OFFICE_INPUT_ENV, "137", "138"),
            (OFFICE_OUTPUT_ENV, "139", "140"),
            (OFFICE_STDERR_ENV, "4097", "4098"),
            (OFFICE_WALL_ENV, "181", "182"),
            (OFFICE_CPU_ENV, "121", "122"),
            (OFFICE_NOFILE_ENV, "257", "258"),
            (OFFICE_ADDRESS_SPACE_ENV, "259", "260"),
            (OFFICE_ACTIVE_PROCESS_ENV, "9", "10"),
            (OFFICE_PROCESS_MEMORY_ENV, "261", "262"),
            (OFFICE_JOB_MEMORY_ENV, "263", "264"),
            (OFFICE_PROCESS_TIME_ENV, "265", "266"),
            (OFFICE_JOB_TIME_ENV, "267", "268"),
        ];
        let policy = crate::DocumentLimitPolicy::defaults();
        let defaults = resolve_service(&|_| None, &ServiceOverrides::default(), policy).unwrap();
        for (name, env_value, cli_value) in TABLE {
            let env_entry = [(*name, *env_value)];
            let cli_entry = [(*name, *cli_value)];
            let bad_entry = [(*name, "bad")];
            let env_only = lookup_map(&env_entry);
            let cli_only = lookup_map(&cli_entry);
            let cli = parse_service_overrides(&cli_only).unwrap();
            // Compiled default wins when nothing is configured.
            assert_eq!(
                render(
                    &resolve_service(&|_| None, &ServiceOverrides::default(), policy).unwrap(),
                    name
                ),
                render(&defaults, name),
                "{name} default"
            );
            // Frozen environment wins over the compiled default.
            assert_eq!(
                render(
                    &resolve_service(&env_only, &ServiceOverrides::default(), policy).unwrap(),
                    name
                ),
                *env_value,
                "{name} environment"
            );
            // Explicit CLI wins over the frozen environment.
            assert_eq!(
                render(&resolve_service(&env_only, &cli, policy).unwrap(), name),
                *cli_value,
                "{name} CLI over environment"
            );
            // Malformed environment values fail before any work.
            let malformed = lookup_map(&bad_entry);
            let error =
                resolve_service(&malformed, &ServiceOverrides::default(), policy).unwrap_err();
            assert!(error.contains(name), "{name}: {error}");
        }
    }

    #[test]
    fn office_limits_resolve_default_env_cli_and_child_env_round_trip() {
        let defaults = OfficeLimits::default();
        let env = lookup_map(&[(OFFICE_INPUT_ENV, "5"), (OFFICE_WALL_ENV, "7")]);
        let overrides = parse_service_overrides(&lookup_map(&[(OFFICE_OUTPUT_ENV, "9")])).unwrap();
        let resolved = OfficeLimits::resolve(&env, &overrides).unwrap();
        assert_eq!(resolved.input_bytes, 5);
        assert_eq!(resolved.wall, Duration::from_secs(7));
        assert_eq!(resolved.output_bytes, 9);
        assert_eq!(resolved.stderr_bytes, defaults.stderr_bytes);

        let mut command = tokio::process::Command::new("true");
        resolved.apply_to_child_env(&mut command);
        let encoded = resolved.child_env();
        assert!(
            encoded
                .iter()
                .any(|(name, value)| name == OFFICE_INPUT_ENV && value.to_str() == Some("5"))
        );
        assert!(
            encoded
                .iter()
                .any(|(name, value)| name == OFFICE_WALL_ENV && value.to_str() == Some("7"))
        );
        assert!(
            encoded
                .iter()
                .any(|(name, value)| name == OFFICE_OUTPUT_ENV && value.to_str() == Some("9"))
        );

        // The OOXML policy including the operator ZIP-scan caps round-trips through the child env.
        let scan = ScanLimits::from_resolved(5, 6, 7, 8, 9, 10).unwrap();
        let ooxml = OoxmlLimits {
            archive_bytes: 11,
            expanded_bytes: 12,
            xml_entry_bytes: 13,
            xml_total_bytes: 14,
            ratio: 15,
            xml_depth: 16,
            xml_events: 17,
            xml_attributes: 18,
            xml_namespaces: 19,
            scan,
        };
        // `from_child_env` reads the live process env, so serialize the round-trip and restore.
        static ENV_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
            std::sync::LazyLock::new(|| std::sync::Mutex::new(()));
        let _guard = ENV_LOCK.lock().unwrap();
        let names = [
            OOXML_ARCHIVE_ENV,
            OOXML_EXPANDED_ENV,
            OOXML_XML_ENTRY_ENV,
            OOXML_XML_TOTAL_ENV,
            OOXML_RATIO_ENV,
            OOXML_XML_DEPTH_ENV,
            OOXML_XML_EVENTS_ENV,
            OOXML_XML_ATTRIBUTES_ENV,
            OOXML_XML_NAMESPACES_ENV,
            "MINERU_ARCHIVE_MAX_ENTRIES",
            "MINERU_ZIP_SCAN_CENTRAL_CAP",
            "MINERU_ZIP_SCAN_NAME_CAP",
            "MINERU_ZIP_SCAN_DEPTH_CAP",
            "MINERU_ZIP_SCAN_TOTAL_NAME_CAP",
            "MINERU_ZIP_SCAN_TOTAL_COMPONENT_CAP",
        ];
        let saved: Vec<_> = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in ooxml.child_env() {
            // SAFETY: the test environment is serialized by ENV_LOCK in this single-threaded section.
            unsafe { std::env::set_var(name, value) };
        }
        let read = OoxmlLimits::from_child_env();
        for (name, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
        assert_eq!(read, ooxml);
        assert_eq!(read.scan, scan);
        assert_eq!(read.scan.zip64_cap, scan.zip64_cap);
        assert_eq!(read.scan.component_cap, scan.component_cap);
    }

    #[test]
    fn server_limits_resolve_defaults_and_strictness() {
        let resolved = ServerLimits::resolve(&|_| None, &ServiceOverrides::default()).unwrap();
        assert_eq!(resolved.record_cap, 32);
        assert_eq!(resolved.file_bytes, 1024 * 1024 * 1024);
        assert!(
            ServerLimits::resolve(
                &lookup_map(&[("MINERU_API_RECORD_CAP", "0")]),
                &ServiceOverrides::default()
            )
            .is_err()
        );
    }

    #[test]
    fn archive_and_scan_accept_values_above_old_clamps() {
        let policy = crate::DocumentLimitPolicy::defaults();
        // Entries and scan caps above the old 100_000/4096/64 defaults are accepted. The ratio
        // stays representable so the checked `compressed * ratio` computation cannot overflow.
        let cli = parse_service_overrides(&lookup_map(&[
            ("MINERU_ARCHIVE_MAX_ENTRIES", "4294967296"),
            ("MINERU_ARCHIVE_MAX_RATIO", "500000000"),
            ("MINERU_ZIP_SCAN_CENTRAL_CAP", "18446744073709551615"),
            ("MINERU_ZIP_SCAN_NAME_CAP", "18446744073709551615"),
            ("MINERU_ZIP_SCAN_DEPTH_CAP", "18446744073709551615"),
        ]))
        .unwrap();
        let resolved = resolve_service(&|_| None, &cli, policy).unwrap();
        assert_eq!(resolved.archive.max_entries, 4_294_967_296);
        assert_eq!(resolved.archive.max_ratio, 500_000_000);
        assert_eq!(resolved.scan.central_cap, u64::MAX);
        assert_eq!(resolved.scan.name_cap, usize::MAX);
        assert_eq!(resolved.scan.depth_cap, usize::MAX);
        // A ratio that overflows the checked arithmetic is a representability error, not a clamp.
        let overflowing = parse_service_overrides(&lookup_map(&[(
            "MINERU_ARCHIVE_MAX_RATIO",
            "18446744073709551615",
        )]))
        .unwrap();
        assert!(resolve_service(&|_| None, &overflowing, policy).is_err());
    }

    #[test]
    fn parse_only_reads_known_names() {
        let names = std::cell::RefCell::new(HashSet::new());
        parse_service_overrides(&|name| {
            assert!(all_names().contains(&name), "unexpected env name {name}");
            names.borrow_mut().insert(name.to_owned());
            None
        })
        .unwrap();
        assert_eq!(names.borrow().len(), all_names().len());
    }

    use std::collections::HashSet;
}
