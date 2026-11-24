use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::web::models::{UnlockReelRequest, WebEvent};
use crate::web::routes::auth::extract_user;
use crate::web::AppState;

pub async fn list_reels(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let user = match extract_user(&headers, &state) {
        Some(u) => u,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Не авторизован"})),
            );
        }
    };

    match state.db.list_reels_for_user(&user.id) {
        Ok(reels) => (StatusCode::OK, Json(serde_json::to_value(reels).unwrap())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

pub async fn get_reel(
    Path(reel_id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    match state.db.get_reel(&reel_id) {
        Ok(Some(reel)) => (StatusCode::OK, Json(serde_json::to_value(reel).unwrap())),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Рилс не найден"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

pub async fn unlock_reel(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<UnlockReelRequest>,
) -> impl IntoResponse {
    let user = match extract_user(&headers, &state) {
        Some(u) => u,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Не авторизован"})),
            );
        }
    };

    let reel = match state.db.get_reel(&payload.reel_id) {
        Ok(Some(r)) => r,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Рилс не найден"})),
            );
        }
    };

    if reel.is_unlocked {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "message": "Рилс уже разблокирован",
                "reel": reel,
                "credits": user.credits,
            })),
        );
    }

    if user.credits < 1 {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(serde_json::json!({"error": "Недостаточно кредитов. Пополните баланс"})),
        );
    }

    // Deduct 1 credit
    let new_credits = match state.db.update_credits(&user.id, -1) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Ошибка обновления баланса: {e}")})),
            );
        }
    };

    let _ = state.db.unlock_reel(&reel.id, &user.id);
    let updated_reel = state.db.get_reel(&reel.id).ok().flatten().unwrap_or(reel);

    // Broadcast balance update
    state.queue.broadcast(WebEvent::BalanceUpdated {
        user_id: user.id.clone(),
        new_balance: new_credits,
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "message": "Рилс успешно разблокирован для скачивания!",
            "reel": updated_reel,
            "credits": new_credits,
        })),
    )
}

/// Stream video file with HTTP 206 Partial Content support for smooth seeking in browsers
pub async fn stream_video(
    Path(reel_id): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let reel = match state.db.get_reel(&reel_id) {
        Ok(Some(r)) => r,
        _ => {
            return (StatusCode::NOT_FOUND, "Video not found").into_response();
        }
    };

    let file_path = PathBuf::from(&reel.file_path);
    if !file_path.exists() {
        return (StatusCode::NOT_FOUND, "Video file missing on disk").into_response();
    }

    let mut file = match std::fs::File::open(&file_path) {
        Ok(f) => f,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot open file").into_response(),
    };

    let total_len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot read metadata").into_response(),
    };

    let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // e.g. "bytes=0-1024" or "bytes=1024-"
        if let Some(spec) = range_str.strip_prefix("bytes=") {
            let parts: Vec<&str> = spec.split('-').collect();
            let start: u64 = parts[0].parse().unwrap_or(0);
            let end: u64 = if parts.len() > 1 && !parts[1].is_empty() {
                parts[1].parse().unwrap_or(total_len - 1).min(total_len - 1)
            } else {
                // Return up to 4MB chunks for fast streaming
                (start + 4 * 1024 * 1024 - 1).min(total_len - 1)
            };

            if start <= end && start < total_len {
                let length = end - start + 1;
                if file.seek(SeekFrom::Start(start)).is_ok() {
                    let mut buffer = vec![0u8; length as usize];
                    if file.read_exact(&mut buffer).is_ok() {
                        let res = Response::builder()
                            .status(StatusCode::PARTIAL_CONTENT)
                            .header(header::CONTENT_TYPE, "video/mp4")
                            .header(header::ACCEPT_RANGES, "bytes")
                            .header(header::CONTENT_LENGTH, length.to_string())
                            .header(
                                header::CONTENT_RANGE,
                                format!("bytes {start}-{end}/{total_len}"),
                            )
                            .body(Body::from(buffer))
                            .unwrap();
                        return res;
                    }
                }
            }
        }
    }

    // Default: full file stream
    match std::fs::read(&file_path) {
        Ok(data) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "video/mp4")
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CONTENT_LENGTH, total_len.to_string())
            .body(Body::from(data))
            .unwrap(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Read error").into_response(),
    }
}
