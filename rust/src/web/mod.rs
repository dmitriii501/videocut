pub mod db;
pub mod models;
pub mod queue;
pub mod routes;
pub mod tbank;
pub mod ui;

use std::net::SocketAddr;

use anyhow::{Context, Result};
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
            let w_stops = w_q.active_stops.clone();
            run_worker(worker_rx, w_db, worker_events_tx, w_stops).await;
        });

        let state = AppState {
            db,
            queue,
        };

        let app = routes::router()
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
