mod config;
mod error;
mod middleware;
mod njalla;
mod webhook;

use anyhow::Result;
use axum::{middleware as axum_middleware, serve, Router};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use crate::config::Config;
use crate::webhook::routes;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize configuration
    let config = Config::from_env()?;

    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("Starting Njalla webhook provider");
    info!(
        "Listening on {}:{}",
        config.webhook_host, config.webhook_port
    );

    // Create Njalla client
    let njalla_client = njalla::Client::new(&config.njalla_api_token)?;

    // Build the application
    let app = Router::new()
        .merge(routes::create_routes(njalla_client, config.clone()))
        .layer(axum_middleware::from_fn(middleware::error_handling_middleware))
        .layer(axum_middleware::from_fn(middleware::logging_middleware))
        .layer(TraceLayer::new_for_http());

    // Create socket address
    let addr = SocketAddr::new(config.webhook_host.parse()?, config.webhook_port);

    // Start the server
    let listener = TcpListener::bind(addr).await?;
    info!("Server started on {}", addr);

    serve(listener, app).await?;

    Ok(())
}
