mod backend;
mod error;
mod framing;
mod message;
mod proxy;
mod state;
mod venv;

use clap::Parser;
use proxy::LspProxy;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Enable debug protocol logging (dumps JSON-RPC messages to stderr)
    #[arg(long)]
    debug_protocol: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // ログファイルの設定（日次ローテーション）
    let log_dir = "/tmp";
    let log_file_prefix = "pyright-lsp-proxy";

    // RollingFileAppender を使用してログファイルを作成
    // Rotation::NEVER で日次ローテーションなし（単一ファイル）
    let file_appender = RollingFileAppender::new(Rotation::NEVER, log_dir, log_file_prefix);

    // tracing 初期化（ファイルに出力）
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false) // ファイル出力なのでANSIカラーコードを無効化
                .with_target(true) // モジュール名を表示
                .with_thread_ids(true), // スレッドIDを表示
        )
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("pyright_lsp_proxy=debug")),
        )
        .init();

    tracing::info!(
        debug_protocol = args.debug_protocol,
        log_dir = log_dir,
        log_file = format!("{}/{}", log_dir, log_file_prefix),
        "Starting pyright-lsp-proxy"
    );

    // プロキシを起動
    let mut proxy = LspProxy::new(args.debug_protocol);
    proxy.run().await?;

    Ok(())
}
