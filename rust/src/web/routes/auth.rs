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
    // Per-user random salt + iterated SHA-256 (no extra crate needed).
    let salt = format!(
        "{}{}",
        Uuid::new_v4().simple(),
        Uuid::new_v4().simple()
    );
    let digest = iterate_hash(&salt, password);
    format!("v1${salt}${digest}")
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn iterate_hash(salt: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(password.as_bytes());
    let mut out = hasher.finalize();
    for _ in 0..100_000 {
        let mut h = Sha256::new();
        h.update(out);
        h.update(salt.as_bytes());
        out = h.finalize();
    }
    to_hex(&out)
}

fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Verify a password against a stored hash, supporting both the current
/// `v1$salt$hash` format and the legacy static-salt format.
pub fn verify_password(stored: &str, password: &str) -> bool {
    if let Some(rest) = stored.strip_prefix("v1$") {
        if let Some((salt, expected)) = rest.split_once('$') {
            return constant_eq(&iterate_hash(salt, password), expected);
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(b"chik_salt_2026_");
    hasher.update(password.as_bytes());
    constant_eq(&format!("{:x}", hasher.finalize()), stored)
}

/// Extract a bearer token from the Authorization header, or (for media
/// elements that cannot set headers) from a `?token=` query parameter.
pub fn token_from(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    if let Some(auth_header) = headers.get("Authorization").and_then(|v| v.to_str().ok()) {
        if let Some(stripped) = auth_header.strip_prefix("Bearer ") {
            return Some(stripped.to_string());
        }
        if !auth_header.is_empty() {
            return Some(auth_header.to_string());
        }
    }
    query_token.map(|t| t.to_string())
}

pub fn extract_user(headers: &HeaderMap, state: &AppState) -> Option<User> {
    extract_user_q(headers, None, state)
}

pub fn extract_user_q(headers: &HeaderMap, query_token: Option<&str>, state: &AppState) -> Option<User> {
    let token = token_from(headers, query_token)?;
    state.db.get_user_by_token(&token).ok()?
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

    if !verify_password(&user.password_hash, &payload.password) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Неверный email или пароль"})),
        );
    }

    // Opportunistically upgrade legacy hashes to the salted format.
    if !user.password_hash.starts_with("v1$") {
        let _ = state
            .db
            .update_password_hash(&user.id, &hash_password(&payload.password));
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

/// Invalidate the current session token server-side.
pub async fn logout(
    headers: HeaderMap,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if let Some(token) = token_from(&headers, None) {
        let _ = state.db.delete_session(&token);
    }
    (StatusCode::OK, Json(serde_json::json!({"success": true})))
}
