use std::collections::BTreeMap;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use uuid::Uuid;

use crate::config;
use crate::utils::log;
use crate::web::models::{Order, PricingPackage, TopupRequest, WebEvent};
use crate::web::routes::auth::extract_user;
use crate::web::tbank;
use crate::web::AppState;

pub fn available_packages() -> Vec<PricingPackage> {
    vec![
        PricingPackage {
            id: "pack_starter".to_string(),
            name: "Старт".to_string(),
            credits: 20,
            price_rub: 490,
            is_popular: false,
            description: "20 клипов, 24.5 ₽ за штуку.".to_string(),
        },
        PricingPackage {
            id: "pack_streamer".to_string(),
            name: "Стример-Пак".to_string(),
            credits: 60,
            price_rub: 1190,
            is_popular: true,
            description: "60 клипов, 19.8 ₽ за штуку.".to_string(),
        },
        PricingPackage {
            id: "pack_pro".to_string(),
            name: "Про-Контент".to_string(),
            credits: 150,
            price_rub: 2490,
            is_popular: false,
            description: "150 клипов, 16.6 ₽ за штуку.".to_string(),
        },
    ]
}

pub async fn list_packages() -> impl IntoResponse {
    let mut resp = serde_json::json!({ "packages": available_packages() });
    // Surface whether real acquiring is on, so the SPA can label the flow.
    resp["payments_enabled"] = serde_json::json!(config::tbank_enabled());
    (StatusCode::OK, Json(resp))
}

fn bad_request(msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

/// Create an order and start a T-Bank payment.
///
/// With acquiring configured, returns `{ payment_url }` and the SPA redirects
/// the user there. Without keys (local dev), falls back to granting credits
/// immediately so the flow stays testable.
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
        None => return bad_request("Неверный тарифный пакет"),
    };

    // --- Dev fallback: no acquiring keys configured ---
    if !config::tbank_enabled() {
        let cap = config::demo_credit_cap();
        if cap > 0 && user.credits >= cap {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": format!("Демо-лимит: максимум {cap} кредитов. Настройте платежи для покупки пакетов."),
                    "credits": user.credits,
                })),
            );
        }
        // Never exceed the cap even if this pack would push past it.
        let grant = if cap > 0 {
            pack.credits.min((cap - user.credits).max(0))
        } else {
            pack.credits
        };
        if grant <= 0 {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({
                    "error": format!("Демо-лимит: максимум {cap} кредитов."),
                    "credits": user.credits,
                })),
            );
        }
        let new_credits = match state.db.update_credits(&user.id, grant) {
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
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": true,
                "demo": true,
                "message": format!("Пакет '{}' активирован! Начислено +{} кредитов.", pack.name, grant),
                "credits": new_credits,
            })),
        );
    }

    // --- Real acquiring: create order + Init ---
    let order_id = format!("ord_{}", Uuid::new_v4().simple());
    let order = Order {
        id: order_id.clone(),
        user_id: user.id.clone(),
        package_id: pack.id.clone(),
        credits: pack.credits,
        amount_kopecks: pack.price_rub * 100,
        status: "pending".to_string(),
        tbank_payment_id: None,
        payment_url: None,
        created_at: Utc::now().to_rfc3339(),
        paid_at: None,
    };

    if let Err(e) = state.db.create_order(&order) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Ошибка создания заказа: {e}")})),
        );
    }

    let base = config::public_url();
    let description = format!("ЧИК — пакет '{}' ({} кредитов)", pack.name, pack.credits);
    let init = tbank::init_payment(
        &order_id,
        order.amount_kopecks,
        &description,
        &format!("{base}/api/billing/tbank/notification"),
        &format!("{base}/pricing?order={order_id}&paid=1"),
        &format!("{base}/pricing?order={order_id}&paid=0"),
    );

    match init {
        Ok(res) => {
            if let Err(e) = state
                .db
                .set_order_payment(&order_id, &res.payment_id, &res.payment_url)
            {
                log(format!("не удалось сохранить реквизиты заказа {order_id}: {e}"));
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "order_id": order_id,
                    "payment_url": res.payment_url,
                    "amount_kopecks": order.amount_kopecks,
                })),
            )
        }
        Err(e) => {
            let _ = state.db.set_order_status(&order_id, "failed");
            log(format!("T-Bank Init провалился для {order_id}: {e}"));
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Платёжный сервис недоступен: {e}")})),
            )
        }
    }
}

/// T-Bank server notification (webhook). Must answer 200 with body `OK`
/// whenever the notification is accepted, otherwise T-Bank keeps retrying.
///
/// Security: verify the token, then re-query the authoritative state via
/// GetState before crediting — never trust the form body alone.
pub async fn tbank_notification(
    State(state): State<AppState>,
    form: String,
) -> impl IntoResponse {
    // Body arrives as application/x-www-form-urlencoded.
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(form.as_bytes()) {
        fields.insert(k.into_owned(), v.into_owned());
    }

    if !tbank::verify_notification(&fields) {
        log("T-Bank notification: неверная подпись Token");
        return (StatusCode::OK, "ERROR");
    }

    let order_id = match fields.get("OrderId") {
        Some(v) => v.clone(),
        None => {
            log("T-Bank notification: нет OrderId");
            return (StatusCode::OK, "ERROR");
        }
    };
    let payment_id = match fields.get("PaymentId") {
        Some(v) => v.clone(),
        None => {
            log(format!("T-Bank notification: нет PaymentId для {order_id}"));
            return (StatusCode::OK, "ERROR");
        }
    };

    let order = match state.db.get_order(&order_id) {
        Ok(Some(o)) => o,
        Ok(None) => {
            log(format!("T-Bank notification: неизвестный заказ {order_id}"));
            return (StatusCode::OK, "ERROR");
        }
        Err(e) => {
            log(format!("T-Bank notification: ошибка БД {e}"));
            return (StatusCode::OK, "ERROR");
        }
    };

    // Re-check the payment state server-to-server.
    let status = match tbank::get_state(&payment_id) {
        Ok((s, _)) => s,
        Err(e) => {
            // Transient acquirer error: ask T-Bank to retry later.
            log(format!("T-Bank GetState провалился для {payment_id}: {e}"));
            return (StatusCode::INTERNAL_SERVER_ERROR, "RETRY");
        }
    };

    if tbank::status_is_paid(&status) {
        match state.db.fulfill_order(&order_id, &Utc::now().to_rfc3339()) {
            Ok((balance, credited_now)) => {
                if credited_now {
                    log(format!(
                        "Заказ {order_id} оплачен ({status}): +{} кредитов, баланс {balance}",
                        order.credits
                    ));
                    state.queue.broadcast(WebEvent::BalanceUpdated {
                        user_id: order.user_id.clone(),
                        new_balance: balance,
                    });
                }
            }
            Err(e) => {
                log(format!("T-Bank fulfill_order ошибка для {order_id}: {e}"));
                return (StatusCode::INTERNAL_SERVER_ERROR, "RETRY");
            }
        }
    } else if matches!(status.as_str(), "REJECTED" | "CANCELED" | "DEADLINE_EXPIRED") {
        let _ = state.db.set_order_status(&order_id, "canceled");
        log(format!("Заказ {order_id} не оплачен: {status}"));
    } else {
        log(format!("Заказ {order_id}: промежуточный статус {status}"));
    }

    (StatusCode::OK, "OK")
}

#[derive(serde::Deserialize)]
pub struct OrderQuery {
    pub order: String,
}

/// Order status for the SPA to poll after returning from the pay page.
/// Does not credit anything — crediting happens only via the verified webhook.
pub async fn order_status(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<OrderQuery>,
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

    match state.db.get_order(&q.order) {
        Ok(Some(o)) if o.user_id == user.id => (
            StatusCode::OK,
            Json(serde_json::json!({
                "order_id": o.id,
                "status": o.status,
                "credits": o.credits,
                "balance": user.credits,
            })),
        ),
        Ok(Some(_)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Заказ не найден"})),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Заказ не найден"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Ошибка БД: {e}")})),
        ),
    }
}
