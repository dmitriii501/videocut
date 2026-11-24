use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::web::models::{AuthResponse, LoginRequest, RegisterRequest, User, UserPublic};
use crate::web::AppState;

pub fn hash_password(password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"chik_salt_2026_");
    hasher.update(password.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn extract_user(headers: &HeaderMap, state: &AppState) -> Option<User> {
    let auth_header = headers.get("Authorization")?.to_str().ok()?;
    let token = if let Some(stripped) = auth_header.strip_prefix("Bearer ") {
        stripped
    } else {
        auth_header
    };
    state.db.get_user_by_token(token).ok()?
}

pub async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> impl IntoResponse {
    let email = payload.email.trim().to_lowercase();
    if email.is_empty() || payload.password.len() < 4 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Введите корректный email и пароль от 4 символов"})),
        );
    }

    if let Ok(Some(_)) = state.db.get_user_by_email(&email) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Пользователь с таким email уже зарегистрирован"})),
        );
    }

    let user_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let user = User {
        id: user_id.clone(),
        email: email.clone(),
        password_hash: hash_password(&payload.password),
        credits: 5, // 5 бесплатных кредитов на старте
        created_at: now.clone(),
    };

    if let Err(e) = state.db.create_user(&user) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Ошибка базы: {e}")})),
        );
    }

    let token = Uuid::new_v4().to_string();
    let _ = state.db.create_session(&token, &user_id, &now);

    (
        StatusCode::OK,
        Json(serde_json::to_value(AuthResponse {
            token,
            user: UserPublic::from(user),
        }).unwrap()),
    )
}

pub async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let email = payload.email.trim().to_lowercase();
    let user = match state.db.get_user_by_email(&email) {
        Ok(Some(u)) => u,
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Неверный email или пароль"})),
            );
        }
    };

    if user.password_hash != hash_password(&payload.password) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Неверный email или пароль"})),
        );
    }

    let token = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let _ = state.db.create_session(&token, &user.id, &now);

    (
        StatusCode::OK,
        Json(serde_json::to_value(AuthResponse {
            token,
            user: UserPublic::from(user),
        }).unwrap()),
    )
}

pub async fn me(
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

    (
        StatusCode::OK,
        Json(serde_json::to_value(UserPublic::from(user)).unwrap()),
    )
}
