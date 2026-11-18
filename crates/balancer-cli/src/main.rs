use anyhow::{Context, Result};
use balancer_config::Config;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "rusty-balancer", version, about)]
struct Cli {
    #[arg(short, long, default_value = "configs/balancer.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command { Serve, Validate }

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    if matches!(cli.command, Some(Command::Validate)) {
        println!("configuration is valid");
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(config.listen).await.context("bind listener")?;
    let server = balancer_http::build(config);
    tracing::info!(address = %listener.local_addr()?, "Rusty Balancer started");
    axum::serve(listener, server.router.clone().into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown()).await.context("HTTP server")?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = sigterm.recv() => {},
                }
            }
            Err(_) => { let _ = tokio::signal::ctrl_c().await; }
        }
    }
    #[cfg(not(unix))]
    { let _ = tokio::signal::ctrl_c().await; }
    tracing::info!("shutdown signal received; draining connections");
}
