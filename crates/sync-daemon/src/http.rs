//! Health check HTTP server.
//!
//! Provides a `/health` endpoint so load balancers and monitoring tools can
//! verify the daemon is running. Enabled by passing `--health-listen` on startup.

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tracing::info;

/// Start the health HTTP server on the given address.
///
/// Blocks until the server terminates (which should only happen on shutdown).
pub async fn serve_health(listen_addr: &str) {
    let listener = TcpListener::bind(listen_addr)
        .await
        .expect("Failed to bind health HTTP server");

    info!("Health server listening on {}", listen_addr);

    let app = Router::new().route("/health", get(health_handler));

    axum::serve(listener, app)
        .await
        .expect("Health HTTP server failed");
}

async fn health_handler() -> &'static str {
    "OK"
}
