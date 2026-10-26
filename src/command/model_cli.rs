use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mineru models",
    about = "Acquire and verify the pinned layout model (model acquisition only; not inference)."
)]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Print the pinned model manifest without network access.
    Show,
    /// Download the pinned model artifacts into an explicit cache directory.
    Download {
        #[arg(long, required = true)]
        cache_dir: PathBuf,
        /// Do not access the network; fail if artifacts are not already cached.
        #[arg(long)]
        offline: bool,
    },
    /// Verify cached artifacts in an explicit cache directory, without network access.
    Verify {
        #[arg(long, required = true)]
        cache_dir: PathBuf,
    },
}

pub(super) async fn run(argv: Vec<std::ffi::OsString>) -> i32 {
    let args =
        std::iter::once(std::ffi::OsString::from("mineru models")).chain(argv.into_iter().skip(1));
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() { 2 } else { 0 };
            return error.print().map(|_| code).unwrap_or(2);
        }
    };
    let result = match cli.action {
        Action::Show => {
            let manifest = crate::models::manifest();
            println!(
                "repository: {}\nrevision: {}",
                manifest.repository, manifest.revision
            );
            println!(
                "layout: {} ({} bytes, ~204 MiB, sha256 {})",
                manifest.layout.path, manifest.layout.size, manifest.layout.sha256
            );
            println!(
                "config: {} ({} bytes, sha256 {})",
                manifest.layout_config.path,
                manifest.layout_config.size,
                manifest.layout_config.sha256
            );
            println!("This command acquires model files; it does not implement inference.");
            Ok(())
        }
        Action::Download { cache_dir, offline } => {
            let policy = if offline {
                crate::models::DownloadPolicy::Offline
            } else {
                crate::models::DownloadPolicy::Auto
            };
            acquire(cache_dir, policy, false).await
        }
        Action::Verify { cache_dir } => {
            acquire(cache_dir, crate::models::DownloadPolicy::Offline, true).await
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

async fn acquire(
    cache_dir: PathBuf,
    policy: crate::models::DownloadPolicy,
    verify: bool,
) -> Result<(), String> {
    let cache = crate::models::ModelCache::new(cache_dir, policy).map_err(|e| e.to_string())?;
    let path = if verify {
        cache.verify_layout().await
    } else {
        cache.ensure_layout().await
    }
    .map_err(|e| e.to_string())?;
    println!(
        "{}: {}",
        if verify { "verified" } else { "ready" },
        path.display()
    );
    Ok(())
}
