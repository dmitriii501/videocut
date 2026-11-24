use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

use crate::web::models::{PricingPackage, TopupRequest, WebEvent};
use crate::web::routes::auth::extract_user;
use crate::web::AppState;

pub fn available_packages() -> Vec<PricingPackage> {
    vec![
        PricingPackage {
            id: "pack_starter".to_string(),
            name: "Старт".to_string(),
            credits: 20,
            price_rub: 490,
            is_popular: false,
            description: "20 вирусных рилсов (24.5 ₽ за штуку). Хватит на 2-3 насыщенных стрима.".to_string(),
        },
        PricingPackage {
            id: "pack_streamer".to_string(),
            name: "Стример-Пак".to_string(),
            credits: 60,
            price_rub: 1190,
            is_popular: true,
            description: "60 сочных роликов (19.8 ₽ за штуку). Самый популярный тариф среди постоянных стримеров.".to_string(),
        },
        PricingPackage {
            id: "pack_pro".to_string(),
            name: "Про-Контент".to_string(),
            credits: 150,
            price_rub: 2490,
            is_popular: false,
            description: "150 рилсов (16.6 ₽ за штуку) + Приоритет в очереди и безлимитный Live-мониторинг.".to_string(),
        },
    ]
}

pub async fn list_packages() -> impl IntoResponse {
    (StatusCode::OK, Json(available_packages()))
}

pub async fn topup(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(payload): Json<TopupRequest>,
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

    let pack = match available_packages().into_iter().find(|p| p.id == payload.package_id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Неверный тарифный пакет"})),
            );
        }
    };

    // In production, create payment link (YooKassa / Tinkoff / Crypto / Stripe).
    // For fast prototyping, grant credits and return success confirmation.
    let new_credits = match state.db.update_credits(&user.id, pack.credits) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Ошибка пополнения: {e}")})),
            );
        }
    };

    state.queue.broadcast(WebEvent::BalanceUpdated {
        user_id: user.id.clone(),
        new_balance: new_credits,
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "message": format!("Пакет '{}' активирован! Начислено +{} кредитов.", pack.name, pack.credits),
            "credits": new_credits,
        })),
    )
}
