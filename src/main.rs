mod hass;
use anyhow::Result;
use clap::{Parser, Subcommand};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServiceExt, transport::stdio};
use tracing::info;
use tracing_subscriber::{self, EnvFilter};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Stdio,
    Http {
        #[arg(long, default_value = "127.0.0.1:8987")]
        bind: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Stdio => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()),
                )
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .init();
            info!("Starting MCP server over STDIO");
            let service = hass::Hass::new()
                .await
                .serve(stdio())
                .await
                .inspect_err(|e| tracing::error!("serving error: {:?}", e))?;
            service.waiting().await?;
        }
        Command::Http { bind } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()),
                )
                .init();
            info!("Starting MCP server over HTTP");
            let ct = tokio_util::sync::CancellationToken::new();
            let service = StreamableHttpService::new(
                || {
                    Ok(tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(hass::Hass::new())
                    }))
                },
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
            );
            let router = axum::Router::new().nest_service("/mcp", service);
            let tcp_listener = tokio::net::TcpListener::bind(&bind).await?;
            info!("Listening on http://{bind}/mcp");
            axum::serve(tcp_listener, router)
                .with_graceful_shutdown(async move {
                    tokio::signal::ctrl_c().await.unwrap();
                    ct.cancel();
                })
                .await?;
        }
    }

    Ok(())
}
