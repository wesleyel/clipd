//! clipd — an HTTP bridge to the macOS pasteboard, so a phone on the same LAN
//! can push text or images straight into the Mac's clipboard.

mod clipboard;
mod imaging;
mod routes;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::clipboard::ClipboardHandle;
use crate::routes::AppState;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Address to bind. Defaults to every interface so the phone can reach it.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::UNSPECIFIED))]
    bind: IpAddr,

    #[arg(long, default_value_t = 14756)]
    port: u16,

    /// Shared secret required in the `X-Clipd-Token` header or a `token=`
    /// query parameter. Auth is off when unset.
    #[arg(long, env = "CLIPD_TOKEN")]
    token: Option<String>,

    /// Largest accepted request body, in MiB.
    #[arg(long, default_value_t = 32)]
    max_body_mb: usize,

    /// Post a macOS notification whenever the clipboard is replaced.
    #[arg(long)]
    notify: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("clipd=info")),
        )
        .init();

    let state = Arc::new(AppState {
        clipboard: ClipboardHandle::spawn()?,
        token: cli.token,
        notify: cli.notify,
    });
    if state.token.is_none() {
        tracing::warn!("running without --token: anyone on this network can write the clipboard");
    }

    let app = routes::router(state, cli.max_body_mb * 1024 * 1024);
    let addr = SocketAddr::new(cli.bind, cli.port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("clipd listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
