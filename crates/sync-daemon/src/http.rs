//! HTTP server with WebSocket upgrade for incoming peer connections.
//!
//! Runs an axum HTTP server that handles WebSocket upgrades on `/sync`
//! and provides a health check endpoint at `/health`. Upgraded connections
//! are sent through an mpsc channel to the main event loop.

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::info;

/// An upgraded WebSocket connection paired with its client address.
pub type IncomingWebSocket = (WebSocket, SocketAddr);

/// Start the HTTP server on the given address, sending upgraded WebSocket
/// connections through the channel for the main loop to handle.
pub async fn serve(listen_addr: &str, ws_tx: mpsc::Sender<IncomingWebSocket>) {
    let listener = TcpListener::bind(listen_addr)
        .await
        .expect("Failed to bind HTTP server");

    info!("HTTP server listening on {}", listen_addr);

    serve_on_listener(listener, ws_tx).await;
}

/// Run the HTTP server on an existing TCP listener.
///
/// Useful for tests that need to bind to a random port before starting.
pub async fn serve_on_listener(listener: TcpListener, ws_tx: mpsc::Sender<IncomingWebSocket>) {
    let app = Router::new()
        .route("/sync", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(ws_tx);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("HTTP server failed");
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(ws_tx): State<mpsc::Sender<IncomingWebSocket>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        if ws_tx.send((socket, addr)).await.is_err() {
            tracing::error!("Failed to send WebSocket connection to main loop");
        }
    })
}

async fn health_handler() -> &'static str {
    "OK"
}
