mod hass;
use anyhow::Result;
use axum::{Router, serve};
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServiceExt, transport::stdio};
use tokio::net::TcpListener;
use tokio::signal::ctrl_c;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tracing::info;
use tracing_subscriber::{self, EnvFilter};

use crate::hass::Hass;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Http {
        #[arg(long, default_value = "127.0.0.1:8987")]
        bind: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let logger = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()));

    match Cli::parse().command {
        None => {
            logger.with_writer(std::io::stderr).with_ansi(false).init();
            info!("Starting MCP server over STDIO");
            let service = Hass::new()
                .await
                .serve(stdio())
                .await
                .inspect_err(|e| tracing::error!("serving error: {:?}", e))?;
            service.waiting().await?;
        }
        Some(Command::Http { bind }) => {
            logger.init();
            info!("Starting MCP server over HTTP");
            let ct = CancellationToken::new();
            let hass = Hass::new().await;
            let service = StreamableHttpService::new(
                move || Ok(hass.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
            );
            let router = Router::new()
                .nest_service("/mcp", service)
                .layer(CorsLayer::very_permissive());
            let tcp_listener = TcpListener::bind(&bind).await?;
            info!("Listening on http://{bind}/mcp");
            serve(tcp_listener, router)
                .with_graceful_shutdown(async move {
                    ctrl_c().await.unwrap();
                    ct.cancel();
                })
                .await?;
        }
    }
    Ok(())
}
