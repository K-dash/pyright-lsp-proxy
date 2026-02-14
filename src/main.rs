mod backend;
mod backend_pool;
mod error;
mod framing;
mod message;
mod proxy;
mod state;
mod venv;

use clap::Parser;
use proxy::LspProxy;
use std::path::PathBuf;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Optional path to log file (default: stderr only)
    /// Can also be set via PYRIGHT_LSP_PROXY_LOG_FILE environment variable
    #[arg(long, env = "PYRIGHT_LSP_PROXY_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Maximum number of concurrent backend processes (default: 4)
    /// Can also be set via PYRIGHT_LSP_PROXY_MAX_BACKENDS environment variable
    #[arg(long, env = "PYRIGHT_LSP_PROXY_MAX_BACKENDS", default_value = "4")]
    max_backends: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Initialize logging (default: stderr, --log-file adds file output)
    if let Some(log_path) = &args.log_file {
        // File output specified: stderr + file
        let file_appender = RollingFileAppender::new(
            Rotation::NEVER,
            log_path.parent().unwrap_or(std::path::Path::new(".")),
            log_path
                .file_name()
                .unwrap_or(std::ffi::OsStr::new("pyright-lsp-proxy.log")),
        );

        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true),
            )
            .with(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true),
            )
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("pyright_lsp_proxy=debug")),
            )
            .init();

        tracing::info!(
            log_file = %log_path.display(),
            "Starting pyright-lsp-proxy (logging to stderr and file)"
        );
    } else {
        // Default: stderr only
        tracing_subscriber::registry()
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true),
            )
            .with(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("pyright_lsp_proxy=debug")),
            )
            .init();

        tracing::info!("Starting pyright-lsp-proxy (logging to stderr only)");
    }

    // Start proxy
    let mut proxy = LspProxy::new(args.max_backends);
    proxy.run().await?;

    Ok(())
}
