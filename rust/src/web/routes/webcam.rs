//! Webcam panel: whether the studio can detect a webcam on its own, the
//! fallback box, and a one-frame preview used to seed the manual editor.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::config;
use crate::vod::webcam::DEFAULT_PANEL;
use crate::web::routes::auth::extract_user;
use crate::web::AppState;

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

#[derive(Debug, Deserialize)]
pub struct PreviewRequest {
    pub stream_url: String,
}

/// `POST /api/webcam/preview { stream_url }` -> `{ auto, frame, box, source, error }`.
///
/// Grabs a single frame from the link and, when JEV is configured, asks it for
/// the face box. `frame` is a `data:image/jpeg;base64,...` URL the studio paints
/// under the editor, so the operator adjusts the real picture instead of a grid.
/// A grab failure still returns `auto`/`error` so the editor keeps working.
pub async fn preview(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<PreviewRequest>,
) -> impl IntoResponse {
    if extract_user(&headers, &state).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Войдите в аккаунт"})),
        );
    }

    let url = payload.stream_url.trim().to_string();
    if url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Укажите ссылку на стрим"})),
        );
    }

    let auto = config::typesafe_api_key().is_some();

    // Frame grab + JEV are blocking (yt-dlp/ffmpeg/HTTP), so keep them off the
    // async runtime.
    let result = tokio::task::spawn_blocking(move || {
        let frame = crate::vod::fetch::grab_url_frame(&url)?;
        let panel = if auto {
            crate::vod::webcam::detect_from_jpeg(&frame)
        } else {
            None
        };
        Ok::<(Vec<u8>, Option<crate::vod::webcam::Panel>), anyhow::Error>((frame, panel))
    })
    .await;

    match result {
        Ok(Ok((frame, panel))) => {
            use base64::Engine as _;
            let data = format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&frame)
            );
            let box_vals = panel.as_ref().map(|p| vec![p.x0, p.y0, p.x1, p.y1]);
            let source = panel
                .as_ref()
                .map(|p| p.source.clone())
                .unwrap_or_else(|| "none".to_string());
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "auto": auto,
                    "frame": data,
                    "box": box_vals,
                    "source": source,
                })),
            )
        }
        Ok(Err(e)) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "auto": auto,
                "frame": serde_json::Value::Null,
                "box": serde_json::Value::Null,
                "error": e.to_string(),
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "auto": auto,
                "error": format!("preview task failed: {e}"),
            })),
        ),
    }
}
