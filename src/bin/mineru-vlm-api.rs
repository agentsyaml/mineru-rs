use clap::Parser;
use std::sync::Arc;
use std::{
    ffi::OsString,
    future::{Future, pending},
    io::{IsTerminal, Read, stderr},
    net::IpAddr,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};
#[path = "support/event_sink.rs"]
#[allow(dead_code)]
mod event_sink;
use event_sink::{EventSink, LogLevel};
#[path = "support/official_env.rs"]
mod official_env;
use official_env::{Decimal, RouteEnv, decimal, nonnegative_decimal, snapshot_route_env};

#[derive(Parser)]
#[command(about = "MinerU mixed vlm-http-client task service")]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, default_value_t = 8000)]
    port: u16,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let level = match LogLevel::from_env() {
        Ok(level) => level,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let is_tty = stderr().is_terminal();
    let sink = Arc::new(EventSink::new(stderr(), is_tty, level));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let env = snapshot_startup_env(cfg!(target_os = "macos"), |name| std::env::var_os(name))?;
        if !args.host.is_loopback() && !env.public_bind_exposed {
            return Err("--host must be a loopback IP address".into());
        }
        let config = mineru::vlm_api::ServiceConfig::new(
            env.concurrency,
            env.output_root,
            env.route.route,
            env.route.formula,
            env.route.table,
        )?
        .public_policy(env.public_bind_exposed, env.allow_public_http_client)
        .task_lifecycle(env.retention, env.cleanup_interval)?
        .progress_callback(sink.callback());
        Ok(tokio::runtime::Runtime::new()?.block_on(async move {
            let listener = tokio::net::TcpListener::bind((args.host, args.port)).await?;
            mineru::vlm_api::serve(listener, config, shutdown(env.shutdown_on_stdin_eof)).await
        })?)
    })();
    if let Err(error) = &result {
        sink.fail(&error.to_string());
    }
    sink.finish();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn ctrl_c_shutdown() {
    if tokio::signal::ctrl_c().await.is_err() {
        pending::<()>().await;
    }
}

#[cfg(unix)]
async fn sigterm_shutdown() {
    let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        pending::<()>().await;
        return;
    };
    if signal.recv().await.is_none() {
        pending::<()>().await;
    }
}

async fn process_shutdown() {
    #[cfg(unix)]
    tokio::select! {
        () = ctrl_c_shutdown() => (),
        () = sigterm_shutdown() => (),
    }
    #[cfg(not(unix))]
    ctrl_c_shutdown().await;
}

fn read_until_eof_or_error(reader: &mut impl Read) {
    let mut bytes = [0; 1024];
    while reader.read(&mut bytes).is_ok_and(|count| count != 0) {}
}

fn stdin_watcher(enabled: bool) -> Option<tokio::sync::oneshot::Receiver<()>> {
    enabled.then(|| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new().spawn(move || {
            let mut stdin = std::io::stdin().lock();
            read_until_eof_or_error(&mut stdin);
            let _ = sender.send(());
        });
        receiver
    })
}

async fn stdin_shutdown(receiver: tokio::sync::oneshot::Receiver<()>) {
    if receiver.await.is_err() {
        pending::<()>().await;
    }
}

fn shutdown(enabled: bool) -> impl Future<Output = ()> {
    let stdin = stdin_watcher(enabled);
    async move {
        tokio::select! {
            () = process_shutdown() => (),
            () = async { match stdin { Some(receiver) => stdin_shutdown(receiver).await, None => pending::<()>().await } } => (),
        }
    }
}

struct StartupEnv {
    output_root: PathBuf,
    concurrency: usize,
    retention: Duration,
    cleanup_interval: Duration,
    public_bind_exposed: bool,
    allow_public_http_client: bool,
    shutdown_on_stdin_eof: bool,
    route: RouteEnv,
}

const STARTUP_NAMES: [&str; 12] = [
    "MINERU_API_OUTPUT_ROOT",
    "MINERU_API_MAX_CONCURRENT_REQUESTS",
    "MINERU_API_TASK_RETENTION_SECONDS",
    "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS",
    "MINERU_API_PUBLIC_BIND_EXPOSED",
    "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT",
    "MINERU_API_SHUTDOWN_ON_STDIN_EOF",
    "MINERU_PROCESSING_WINDOW_SIZE",
    "MINERU_PDF_RENDER_THREADS",
    "MINERU_PDF_RENDER_TIMEOUT",
    "MINERU_FORMULA_ENABLE",
    "MINERU_TABLE_ENABLE",
];

fn api_flag(value: Option<&OsString>) -> bool {
    value
        .and_then(|v| v.to_str())
        .is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

fn service_concurrency(darwin: bool, value: Option<&OsString>) -> Result<usize, String> {
    if darwin {
        return Ok(1);
    }
    match value {
        None => Ok(3),
        Some(value) => match decimal(value, usize::MAX as u64) {
            Decimal::Positive(value) => Ok(value as usize),
            Decimal::Invalid | Decimal::NonPositive => {
                Err("MINERU_API_MAX_CONCURRENT_REQUESTS must be positive".into())
            }
        },
    }
}

fn snapshot_startup_env(
    darwin: bool,
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> Result<StartupEnv, String> {
    let values = STARTUP_NAMES.map(&mut lookup);
    let get = |name: &'static str| {
        values[STARTUP_NAMES.iter().position(|&n| n == name).unwrap()].as_ref()
    };
    let route = snapshot_route_env(|name| {
        STARTUP_NAMES
            .iter()
            .position(|&n| n == name)
            .and_then(|i| values[i].clone())
    });
    let concurrency = service_concurrency(darwin, get("MINERU_API_MAX_CONCURRENT_REQUESTS"))?;
    let retention = get("MINERU_API_TASK_RETENTION_SECONDS")
        .and_then(|v| nonnegative_decimal(v, u64::MAX))
        .unwrap_or(86400);
    let cleanup_interval = match get("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS") {
        Some(v) => match decimal(v, u64::MAX) {
            Decimal::Positive(v) => v,
            _ => 300,
        },
        None => 300,
    };
    Ok(StartupEnv {
        output_root: get("MINERU_API_OUTPUT_ROOT")
            .cloned()
            .map(Into::into)
            .unwrap_or_else(|| PathBuf::from("./output")),
        concurrency,
        retention: Duration::from_secs(retention),
        cleanup_interval: Duration::from_secs(cleanup_interval),
        public_bind_exposed: api_flag(get("MINERU_API_PUBLIC_BIND_EXPOSED")),
        allow_public_http_client: api_flag(get("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT")),
        shutdown_on_stdin_eof: api_flag(get("MINERU_API_SHUTDOWN_ON_STDIN_EOF")),
        route,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, io::Cursor};

    fn snapshot(values: &[(&str, &str)]) -> StartupEnv {
        let values: HashMap<_, _> = values
            .iter()
            .map(|&(k, v)| (k, OsString::from(v)))
            .collect();
        snapshot_startup_env(false, |name| values.get(name).cloned()).unwrap()
    }

    #[test]
    fn defaults_and_parsers() {
        let env = snapshot(&[]);
        assert_eq!(env.output_root, PathBuf::from("./output"));
        assert_eq!(env.concurrency, 3);
        assert_eq!(env.retention, Duration::from_secs(86400));
        assert_eq!(env.cleanup_interval, Duration::from_secs(300));
        assert!(
            !env.public_bind_exposed && !env.allow_public_http_client && !env.shutdown_on_stdin_eof
        );
        let env = snapshot(&[
            ("MINERU_API_OUTPUT_ROOT", ""),
            ("MINERU_API_MAX_CONCURRENT_REQUESTS", "+1_024"),
            ("MINERU_API_TASK_RETENTION_SECONDS", "-0"),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", "12"),
            ("MINERU_API_PUBLIC_BIND_EXPOSED", "YES"),
            ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "on"),
            ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "1"),
            ("MINERU_PROCESSING_WINDOW_SIZE", "7"),
            ("MINERU_PDF_RENDER_THREADS", "8"),
            ("MINERU_PDF_RENDER_TIMEOUT", "9"),
            ("MINERU_FORMULA_ENABLE", "TRUE"),
            ("MINERU_TABLE_ENABLE", "false"),
        ]);
        assert_eq!(env.output_root, PathBuf::new());
        assert_eq!(env.concurrency, 1024);
        assert_eq!(env.retention, Duration::ZERO);
        assert_eq!(env.cleanup_interval, Duration::from_secs(12));
        assert!(
            env.public_bind_exposed && env.allow_public_http_client && env.shutdown_on_stdin_eof
        );
        assert_eq!(env.route.route.processing_window_size, 7);
        assert_eq!(env.route.route.render_workers, 8);
        assert_eq!(env.route.route.render_timeout, Duration::from_secs(9));
        assert_eq!(env.route.formula, Some(true));
        assert_eq!(env.route.table, Some(false));
    }

    #[test]
    fn fallback_range_and_no_trim_flags() {
        let env = snapshot(&[
            ("MINERU_API_TASK_RETENTION_SECONDS", "-2"),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", "0"),
            ("MINERU_API_PUBLIC_BIND_EXPOSED", " true "),
            ("MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT", "bad"),
            ("MINERU_API_SHUTDOWN_ON_STDIN_EOF", "True"),
            ("MINERU_PROCESSING_WINDOW_SIZE", "bad"),
            ("MINERU_PDF_RENDER_THREADS", "0"),
            ("MINERU_PDF_RENDER_TIMEOUT", "-0"),
        ]);
        assert_eq!(env.retention, Duration::from_secs(86400));
        assert_eq!(env.cleanup_interval, Duration::from_secs(300));
        assert!(
            !env.public_bind_exposed && !env.allow_public_http_client && env.shutdown_on_stdin_eof
        );
        assert_eq!(env.route.route.processing_window_size, 64);
        assert_eq!(env.route.route.render_workers, 3);
        assert_eq!(env.route.route.render_timeout, Duration::from_secs(300));
        let huge = "999999999999999999999999999999";
        let env = snapshot(&[
            ("MINERU_API_TASK_RETENTION_SECONDS", huge),
            ("MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS", huge),
        ]);
        assert_eq!(env.retention, Duration::from_secs(u64::MAX));
        assert_eq!(env.cleanup_interval, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn reads_only_the_twelve_names_once_before_errors() {
        let mut successful_counts = HashMap::new();
        snapshot_startup_env(false, |name| {
            assert!(STARTUP_NAMES.contains(&name));
            *successful_counts.entry(name.to_owned()).or_insert(0) += 1;
            None
        })
        .unwrap();
        assert!(
            STARTUP_NAMES
                .iter()
                .all(|name| successful_counts.get(*name) == Some(&1))
        );
        let mut counts = HashMap::new();
        let result = snapshot_startup_env(false, |name| {
            assert!(STARTUP_NAMES.contains(&name));
            *counts.entry(name.to_owned()).or_insert(0) += 1;
            (name == "MINERU_API_MAX_CONCURRENT_REQUESTS").then(|| "bad".into())
        });
        assert_eq!(
            result.err().as_deref(),
            Some("MINERU_API_MAX_CONCURRENT_REQUESTS must be positive")
        );
        assert!(
            STARTUP_NAMES
                .iter()
                .all(|name| counts.get(*name) == Some(&1))
        );
        let mut darwin_counts = HashMap::new();
        assert_eq!(
            snapshot_startup_env(true, |name| {
                assert!(STARTUP_NAMES.contains(&name));
                *darwin_counts.entry(name.to_owned()).or_insert(0) += 1;
                if name == "MINERU_API_MAX_CONCURRENT_REQUESTS" {
                    Some("bad".into())
                } else {
                    None
                }
            })
            .unwrap()
            .concurrency,
            1
        );
        assert!(
            STARTUP_NAMES
                .iter()
                .all(|name| darwin_counts.get(*name) == Some(&1))
        );
    }

    #[test]
    fn service_concurrency_maps_values() {
        assert_eq!(service_concurrency(false, None), Ok(3));
        for value in ["0", "-0", "-2", "bad"] {
            assert!(service_concurrency(false, Some(&value.into())).is_err());
        }
        assert_eq!(service_concurrency(false, Some(&"1_024".into())), Ok(1024));
        assert_eq!(
            service_concurrency(false, Some(&"999999999999999999999999999999".into())),
            Ok(usize::MAX)
        );
    }

    #[test]
    fn watcher_reads_through_eof_and_errors() {
        let mut bytes = Cursor::new(b"ordinary input".to_vec());
        read_until_eof_or_error(&mut bytes);
        assert_eq!(bytes.position(), 14);
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken"))
            }
        }
        read_until_eof_or_error(&mut Broken);
    }

    #[tokio::test]
    async fn stdin_receiver_validation_and_disabled_watcher() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(()).unwrap();
        tokio::time::timeout(Duration::from_millis(20), stdin_shutdown(receiver))
            .await
            .unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        drop(sender);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), stdin_shutdown(receiver))
                .await
                .is_err()
        );
        assert!(stdin_watcher(false).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_values_follow_fallback_rules() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![0xff]);
        let result = snapshot_startup_env(false, |name| {
            (name == "MINERU_API_MAX_CONCURRENT_REQUESTS").then(|| bad.clone())
        });
        assert!(result.is_err());
        let env = snapshot_startup_env(false, |name| match name {
            "MINERU_API_OUTPUT_ROOT"
            | "MINERU_API_TASK_RETENTION_SECONDS"
            | "MINERU_API_TASK_CLEANUP_INTERVAL_SECONDS"
            | "MINERU_API_PUBLIC_BIND_EXPOSED"
            | "MINERU_API_ALLOW_PUBLIC_HTTP_CLIENT"
            | "MINERU_API_SHUTDOWN_ON_STDIN_EOF"
            | "MINERU_PROCESSING_WINDOW_SIZE"
            | "MINERU_PDF_RENDER_THREADS"
            | "MINERU_PDF_RENDER_TIMEOUT"
            | "MINERU_FORMULA_ENABLE"
            | "MINERU_TABLE_ENABLE" => Some(bad.clone()),
            _ => None,
        })
        .unwrap();
        assert_eq!(env.output_root.into_os_string(), bad);
        assert_eq!(env.retention, Duration::from_secs(86400));
        assert_eq!(env.cleanup_interval, Duration::from_secs(300));
        assert!(
            !env.public_bind_exposed && !env.allow_public_http_client && !env.shutdown_on_stdin_eof
        );
        assert_eq!(env.route.route.processing_window_size, 64);
        assert_eq!(env.route.route.render_workers, 3);
        assert_eq!(env.route.route.render_timeout, Duration::from_secs(300));
        assert_eq!(env.route.formula, Some(false));
        assert_eq!(env.route.table, Some(false));
    }
}
