pub mod db;
pub mod models;
pub mod queue;
pub mod routes;
pub mod ui;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::Router;
use tower_http::cors::CorsLayer;

use crate::config;
use crate::utils::log;
use crate::web::db::Database;
use crate::web::queue::{run_worker, JobQueue};

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub queue: JobQueue,
}

pub fn run(bind_addr: String, port: u16) -> Result<i32> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(async move {
        // Initialize SQLite DB
        let db_path = config::cache().join("chik.db");
        let db = Database::open(&db_path)?;
        log(format!("ЧИК база данных: {}", db_path.display()));

        // Initialize Queue & background worker
        let (queue, worker_rx) = JobQueue::new(db.clone());

        // Spawn background worker
        let w_db = db.clone();
        let w_q = queue.clone();
        tokio::spawn(async move {
            let worker_events_tx = w_q.events_tx.clone();
            run_worker(worker_rx, w_db, worker_events_tx).await;
        });

        let state = AppState {
            db,
            queue,
        };

        let app = Router::new()
            .route("/", get(ui::index))
            .route("/api/auth/register", post(routes::auth::register))
            .route("/api/auth/login", post(routes::auth::login))
            .route("/api/auth/me", get(routes::auth::me))
            .route("/api/jobs", post(routes::jobs::create_job).get(routes::jobs::list_jobs))
            .route("/api/jobs/{id}", get(routes::jobs::get_job))
            .route("/api/reels", get(routes::reels::list_reels))
            .route("/api/reels/{id}", get(routes::reels::get_reel))
            .route("/api/reels/{id}/video", get(routes::reels::stream_video))
            .route("/api/reels/unlock", post(routes::reels::unlock_reel))
            .route("/api/billing/packages", get(routes::billing::list_packages))
            .route("/api/billing/topup", post(routes::billing::topup))
            .route("/api/ws", get(routes::ws::ws_handler))
            .layer(CorsLayer::permissive())
            .with_state(state);

        let addr: SocketAddr = format!("{bind_addr}:{port}")
            .parse()
            .with_context(|| format!("invalid bind address {bind_addr}:{port}"))?;

        println!();
        println!("============================================================");
        println!("  ✂️  ЧИК / CHIK — Автопилот вирусных нарезок");
        println!("  Веб-платформа запущена на: http://{}:{}", if bind_addr == "0.0.0.0" { "localhost" } else { &bind_addr }, port);
        println!("============================================================");
        println!();

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;

        Ok::<i32, anyhow::Error>(0)
    })
}
