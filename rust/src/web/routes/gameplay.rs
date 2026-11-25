//! Gameplay presets advertised to the studio UI.

use axum::response::IntoResponse;
use axum::Json;

use crate::config;
use crate::vod::gameplay;

/// `GET /api/gameplay` -> `{ enabled, presets: [{key,label}] }`.
pub async fn list_presets() -> impl IntoResponse {
    let presets: Vec<serde_json::Value> = gameplay::presets()
        .iter()
        .map(|p| serde_json::json!({ "key": p.key(), "label": p.label() }))
        .collect();
    Json(serde_json::json!({
        "enabled": config::gameplay_enabled(),
        "presets": presets,
    }))
}
