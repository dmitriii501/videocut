use std::io::{Read, Seek, SeekFrom};
use std::path::{Path as FsPath, PathBuf};
use std::process::Command;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::utils::log;
use crate::web::models::{UnlockReelRequest, WebEvent};
use crate::web::routes::auth::{extract_user, extract_user_q};
use crate::web::AppState;

/// Seconds of a locked reel that are safe to expose as a preview/thumbnail.
const PREVIEW_SECS: u32 = 3;

#[derive(Debug, Deserialize)]
pub struct VideoQuery {
    pub token: Option<String>,
}

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

    match state.db.get_reel(&reel_id) {
        Ok(Some(reel)) if reel.user_id == user.id => {
            (StatusCode::OK, Json(serde_json::to_value(reel).unwrap()))
        }
        Ok(Some(_)) | Ok(None) => (
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
        Ok(Some(r)) if r.user_id == user.id => r,
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

/// Stream video file with HTTP 206 Partial Content support for smooth seeking in browsers.
/// Accepts the session token either as a bearer header or a `?token=` query param
/// (media elements cannot set custom headers).
pub async fn stream_video(
    Path(reel_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<VideoQuery>,
    State(state): State<AppState>,
) -> Response {
    let user = match extract_user_q(&headers, query.token.as_deref(), &state) {
        Some(u) => u,
        None => return (StatusCode::UNAUTHORIZED, "Не авторизован").into_response(),
    };

    let reel = match state.db.get_reel(&reel_id) {
        Ok(Some(r)) if r.user_id == user.id => r,
        _ => {
            return (StatusCode::NOT_FOUND, "Video not found").into_response();
        }
    };

    let full_path = PathBuf::from(&reel.file_path);
    if !full_path.exists() {
        return (StatusCode::NOT_FOUND, "Video file missing on disk").into_response();
    }

    // Locked reels must not stream the full paid file (a direct URL would be a
    // free download). Serve a short cached preview instead; unlocking swaps in
    // the real file on the next request.
    let file_path = if reel.is_unlocked {
        full_path.clone()
    } else {
        match preview_clip(&full_path) {
            Some(p) => p,
            None => {
                return (StatusCode::FORBIDDEN, "Разблокируйте ролик, чтобы смотреть целиком").into_response();
            }
        }
    };

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

/// Return a short, cached preview of `source` (first `PREVIEW_SECS` seconds),
/// used to render locked-reel thumbnails without exposing the paid file.
fn preview_clip(source: &FsPath) -> Option<PathBuf> {
    let meta = std::fs::metadata(source).ok()?;
    let out = source.with_file_name(format!(
        ".preview-{}-{}.mp4",
        PREVIEW_SECS,
        meta.len()
    ));
    if out.exists() && std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false) {
        return Some(out);
    }
    // Re-encode just the opening seconds; `-an` keeps it tiny (thumbnail only).
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(source)
        .args([
            "-t",
            &PREVIEW_SECS.to_string(),
            "-an",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "30",
            "-movflags",
            "+faststart",
        ])
        .arg(&out)
        .status();
    match status {
        Ok(s) if s.success() => Some(out),
        Ok(s) => {
            log(format!("preview: ffmpeg failed ({s}) for {}", source.display()));
            None
        }
        Err(e) => {
            log(format!("preview: cannot run ffmpeg ({e})"));
            None
        }
    }
}
