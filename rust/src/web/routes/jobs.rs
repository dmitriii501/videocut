use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use uuid::Uuid;

use crate::web::models::{CreateJobRequest, Job};
use crate::web::queue::JobTask;
use crate::web::routes::auth::extract_user;
use crate::web::AppState;

pub async fn create_job(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<CreateJobRequest>,
) -> impl IntoResponse {
    let user = match extract_user(&headers, &state) {
        Some(u) => u,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Войдите в аккаунт, чтобы запустить нарезку"})),
            );
        }
    };

    let stream_url = payload.stream_url.trim().to_string();
    if stream_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Укажите ссылку на стрим или видео (Twitch / YouTube)"})),
        );
    }

    // Determine channel name from url
    let channel_name = if stream_url.contains("twitch.tv/") {
        stream_url
            .split("twitch.tv/")
            .nth(1)
            .unwrap_or("twitch")
            .split('/')
            .next()
            .unwrap_or("twitch")
            .to_string()
    } else if stream_url.contains("youtube.com") || stream_url.contains("youtu.be") {
        "youtube".to_string()
    } else {
        "stream".to_string()
    };

    let is_live_channel = stream_url.contains("twitch.tv/")
        && !stream_url.contains("/videos/")
        && !stream_url.contains("/clip/")
        && !stream_url.contains("clips.twitch.tv");

    let actual_mode = if is_live_channel {
        "live".to_string()
    } else {
        payload.mode.clone()
    };

    let job_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let job = Job {
        id: job_id.clone(),
        user_id: user.id.clone(),
        stream_url: stream_url.clone(),
        channel_name,
        mode: actual_mode,
        gameplay_preset: payload.gameplay_preset.clone(),
        subtitle_lang: payload.subtitle_lang.clone(),
        status: "queued".to_string(),
        progress_percent: 0,
        stage: "В очереди на обработку".to_string(),
        error_message: None,
        created_at: now,
        finished_at: None,
    };

    if let Err(e) = state.db.create_job(&job) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Ошибка базы: {e}")})),
        );
    }

    let should_stop = Arc::new(AtomicBool::new(false));
    let task = JobTask {
        job: job.clone(),
        count: payload.count.max(1),
        banner: payload.banner,
        should_stop,
    };

    if let Err(e) = state.queue.submit(task).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Ошибка отправки задачи в очередь: {e}")})),
        );
    }

    (StatusCode::CREATED, Json(serde_json::to_value(job).unwrap()))
}

pub async fn list_jobs(
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

    match state.db.list_jobs(&user.id) {
        Ok(jobs) => (StatusCode::OK, Json(serde_json::to_value(jobs).unwrap())),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

pub async fn get_job(
    headers: HeaderMap,
    Path(job_id): Path<String>,
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

    // Check if job exists and belongs to the user
    let job = match state.db.get_job(&job_id) {
        Ok(Some(j)) if j.user_id == user.id => j,
        Ok(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Задача не найдена"})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            );
        }
    };

    let reels = state.db.list_reels_for_job(&job_id).unwrap_or_default();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "job": job,
            "reels": reels,
        })),
    )
}

pub async fn stop_job(
    headers: HeaderMap,
    Path(job_id): Path<String>,
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

    let job = match state.db.get_job(&job_id) {
        Ok(Some(j)) => j,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Задача не найдена"})),
            );
        }
    };

    if job.user_id != user.id {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Нет доступа к этой задаче"})),
        );
    }

    state.queue.cancel_job(&job_id);

    let now = Utc::now().to_rfc3339();
    let _ = state.db.update_job_status(&job_id, "stopped", Some("Остановлено пользователем"), Some(&now));
    state.queue.broadcast(crate::web::models::WebEvent::JobStatus {
        job_id: job_id.clone(),
        status: "stopped".to_string(),
        error: Some("Остановлено пользователем".to_string()),
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({"success": true, "message": "Задача остановлена", "job_id": job_id})),
    )
}
