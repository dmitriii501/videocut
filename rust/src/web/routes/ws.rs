use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;

use crate::web::AppState;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.queue.subscribe();
    while let Ok(event) = rx.recv().await {
        if let Ok(msg_text) = serde_json::to_string(&event) {
            if socket.send(Message::Text(msg_text.into())).await.is_err() {
                // Client disconnected
                break;
            }
        }
    }
}
