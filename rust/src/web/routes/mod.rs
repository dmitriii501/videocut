//! Every HTTP route of the web platform, wired behind one entry point:
//! [`router`]. The handlers stay private to this module, so callers depend on
//! the route table, not on 17 individual functions.

mod auth;
mod billing;
mod jobs;
mod reels;
mod ws;

use axum::routing::{get, post};
use axum::Router;

use crate::web::ui;
use crate::web::AppState;

/// Build the full route table. The returned router still needs its state
/// (`.with_state`) and any layers the caller wants to apply.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(ui::index))
        .route("/studio", get(ui::index))
        .route("/app", get(ui::index))
        .route("/reels", get(ui::index))
        .route("/pricing", get(ui::index))
        .route("/api/auth/register", post(auth::register))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/me", get(auth::me))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/jobs", post(jobs::create_job).get(jobs::list_jobs))
        .route("/api/jobs/{id}", get(jobs::get_job))
        .route("/api/jobs/{id}/stop", post(jobs::stop_job))
        .route("/api/reels", get(reels::list_reels))
        .route("/api/reels/{id}", get(reels::get_reel))
        .route("/api/reels/{id}/video", get(reels::stream_video))
        .route("/api/reels/unlock", post(reels::unlock_reel))
        .route("/api/billing/packages", get(billing::list_packages))
        .route("/api/billing/topup", post(billing::topup))
        .route("/api/billing/order", get(billing::order_status))
        .route(
            "/api/billing/tbank/notification",
            post(billing::tbank_notification),
        )
        .route("/api/ws", get(ws::ws_handler))
}
