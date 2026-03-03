use crate::oracle::GasOracle;
use axum::{
    Json, Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

#[derive(Clone)]
pub struct AppState {
    pub tx_broadcaster: broadcast::Sender<String>,
    pub gas_oracle: Arc<GasOracle>,
}

#[derive(Deserialize, Serialize)]
pub struct ApiTransaction {
    pub hash: String,
    pub tx_type: i16,
    pub sender: Option<String>,
    pub receiver: Option<String>,
    pub value_wei: String,
    pub gas_limit: i64,
    pub gas_price_or_max_fee_wei: Option<String>,
    pub max_priority_fee_wei: Option<String>,
    pub input_len: i32,
    pub first_seen_at: DateTime<Utc>,
    pub is_private: bool,
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

async fn websocket(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.tx_broadcaster.subscribe();
    info!("New WebSocket client connected");

    loop {
        match rx.recv().await {
            Ok(tx_json) => {
                if socket.send(Message::Text(tx_json)).await.is_err() {
                    info!("WebSocket client disconnected");
                    break;
                }
            }
            Err(e) => {
                warn!("Error receiving from broadcast channel: {}", e);
                break;
            }
        }
    }
}

/// Get gas prices
async fn get_gas_oracle(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let estimates = state.gas_oracle.get_estimates().await;
    (StatusCode::OK, Json(estimates))
}

pub fn create_router(app_state: Arc<AppState>) -> Router {
    info!(target: "crawler::api", "Creating API router");
    Router::new()
        .route("/ws", get(websocket_handler))
        .route("/api/gas/oracle", get(get_gas_oracle))
        .with_state(app_state)
}
