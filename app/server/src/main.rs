//! `pocketskynet` — the server binary.
//!
//! Deliberately thin: parse flags, bind, serve. Everything worth testing lives
//! in the library, and the desktop app in `gui/` drives the same `bind`/`serve`
//! pair so the two cannot drift apart.

use clap::Parser;
use pocketskynet_server::config::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let log_filter = cli.log.clone();
    let (cfg, secret) = cli.resolve()?;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(log_filter))
        .with_target(false)
        .init();

    // Rendered before `bind` takes ownership of the config, and printed after,
    // so the two banners appear together.
    let storage = pocketskynet_server::storage_banner(&cfg);

    let bound = pocketskynet_server::bind(cfg, secret).await?;

    // Printed, not logged: this is the one line the operator is waiting for,
    // and a log filter should never be able to hide it.
    println!(
        "{}",
        pocketskynet_server::connect_banner(bound.addr, bound.scheme, bound.redirect_port)
    );
    println!("{storage}");

    bound.serve(shutdown_signal()).await?;

    tracing::info!("shutdown complete");
    Ok(())
}

/// Resolve on Ctrl-C, and on `SIGTERM` where there is one — a container is
/// stopped with `SIGTERM`, and ignoring it would turn every deploy into a hard
/// kill with an unflushed log.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // No signal handler available: never resolve, so Ctrl-C still wins
            // the select rather than the process exiting immediately.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => tracing::info!("interrupt received; draining connections"),
        _ = terminate => tracing::info!("termination requested; draining connections"),
    }
}
