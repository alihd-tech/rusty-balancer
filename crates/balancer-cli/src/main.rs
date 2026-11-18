use anyhow::{Context, Result};
use balancer_config::Config;
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
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
    if let Some(tls) = &config.tls { balancer_tls::validate(tls)?; }
    if matches!(cli.command, Some(Command::Validate)) {
        println!("configuration is valid");
        return Ok(());
    }
    let address = config.listen;
    let tls = config.tls.clone();
    let listener = std::net::TcpListener::bind(address).context("bind HTTP listener")?;
    listener.set_nonblocking(true)?;
    let server = balancer_http::build(config);
    let http_handle = axum_server::Handle::new();
    let https_handle = axum_server::Handle::new();
    let http_shutdown = http_handle.clone();
    let https_shutdown = https_handle.clone();
    let signal = tokio::spawn(async move {
        shutdown().await;
        http_shutdown.graceful_shutdown(Some(Duration::from_secs(30)));
        https_shutdown.graceful_shutdown(Some(Duration::from_secs(30)));
    });
    let app = if let Some(cfg) = &tls {
        if cfg.redirect_http { balancer_http::redirect_router(cfg) } else { server.router.clone() }
    } else { server.router.clone() };
    tracing::info!(address = %listener.local_addr()?, "HTTP listener bound");
    let http = async {
        axum_server::from_tcp(listener)?.handle(http_handle)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await.context("HTTP server")
    };
    let https = async {
        if let Some(cfg) = tls {
            let router = balancer_http::https_router(server.router.clone(), &cfg);
            balancer_tls::serve(cfg, router, https_handle).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::try_join!(http, https);
    signal.abort();
    result?;
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
