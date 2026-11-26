//! Whether the studio can detect a webcam on its own, and the fallback box.

use axum::response::IntoResponse;
use axum::Json;

use crate::config;
use crate::vod::webcam::DEFAULT_PANEL;

/// `GET /api/webcam` -> `{ auto, box: [x0, y0, x1, y1] }`.
///
/// `auto` is true only when a TypeSafe key is configured. The box is the
/// prepared fallback (right-hand strip) the studio can move or redraw.
pub async fn defaults() -> impl IntoResponse {
    let (x0, y0, x1, y1) = DEFAULT_PANEL;
    Json(serde_json::json!({
        "auto": config::typesafe_api_key().is_some(),
        "box": [x0, y0, x1, y1],
    }))
}
