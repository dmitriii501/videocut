use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{broadcast, mpsc};

use crate::config;
use crate::live_pipeline::{self, LiveJob};
use crate::pipeline::{self, ReelJob};
use crate::utils::{duration, log};
use crate::web::db::Database;
use crate::web::models::{Job, Reel, WebEvent};

#[derive(Clone)]
pub struct JobTask {
    pub job: Job,
    pub count: usize,
    pub should_stop: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct JobQueue {
    #[allow(dead_code)]
    db: Database,
    tx: mpsc::Sender<JobTask>,
    pub events_tx: broadcast::Sender<WebEvent>,
    pub active_stops: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl JobQueue {
    pub fn new(db: Database) -> (Self, mpsc::Receiver<JobTask>) {
        let (tx, rx) = mpsc::channel::<JobTask>(64);
        let (events_tx, _) = broadcast::channel::<WebEvent>(256);
        let queue = Self {
            db,
            tx,
            events_tx,
            active_stops: Arc::new(Mutex::new(HashMap::new())),
        };
        (queue, rx)
    }

    pub async fn submit(&self, task: JobTask) -> Result<()> {
        if let Ok(mut map) = self.active_stops.lock() {
            map.insert(task.job.id.clone(), task.should_stop.clone());
        }
        self.tx.send(task).await.map_err(|e| anyhow::anyhow!("queue send failed: {e}"))
    }

    pub fn cancel_job(&self, job_id: &str) {
        if let Ok(map) = self.active_stops.lock() {
            if let Some(stop) = map.get(job_id) {
                stop.store(true, Ordering::SeqCst);
                log(format!("[worker] signaled stop for job {job_id}"));
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WebEvent> {
        self.events_tx.subscribe()
    }

    pub fn broadcast(&self, event: WebEvent) {
        let _ = self.events_tx.send(event);
    }
}

/// Background worker loop that executes jobs sequentially to prevent OOM
pub async fn run_worker(
    mut rx: mpsc::Receiver<JobTask>,
    db: Database,
    events_tx: broadcast::Sender<WebEvent>,
    active_stops: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
) {
    log("[worker] background queue worker started");
    while let Some(task) = rx.recv().await {
        let job_id = task.job.id.clone();
        let user_id = task.job.user_id.clone();
        let mode = task.job.mode.clone();
        let stream_url = task.job.stream_url.clone();
        let count = task.count;
        let subtitle_lang = task.job.subtitle_lang.clone();
        let should_stop = task.should_stop.clone();

        log(format!("[worker] processing job {job_id} ({mode}): {stream_url}"));

        // Update DB to processing
        let _ = db.update_job_status(&job_id, "processing", None, None);
        let _ = db.update_job_progress(&job_id, 5, "Инициализация...");
        let _ = events_tx.send(WebEvent::JobStatus {
            job_id: job_id.clone(),
            status: "processing".to_string(),
            error: None,
        });

        // Run in a blocking thread because ffmpeg/whisper are CPU-bound and synchronous
        let db_clone = db.clone();
        let events_clone = events_tx.clone();
        let job_id_clone = job_id.clone();
        let user_id_for_thread = user_id.clone();

        let result = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>> {
            let out_dir = config::output().join(&job_id_clone);
            std::fs::create_dir_all(&out_dir)?;

            let jid = job_id_clone.clone();
            let db_inner = db_clone.clone();
            let ev_inner = events_clone.clone();

            let progress_cb = Arc::new(move |msg: &str| {
                log(format!("[job {jid}] {msg}"));
                // Compute heuristic progress percent from message
                let percent = if msg.contains("download") || msg.contains("скачиван") {
                    20
                } else if msg.contains("scan") || msg.contains("аудио") || msg.contains("чат") || msg.contains("хайп") {
                    45
                } else if msg.contains("whisper") || msg.contains("субтитр") || msg.contains("транскрип") {
                    70
                } else if msg.contains("render") || msg.contains("рендер") {
                    85
                } else if msg.contains("done") || msg.contains("готов") {
                    95
                } else {
                    50
                };
                let _ = db_inner.update_job_progress(&jid, percent, msg);
                let _ = ev_inner.send(WebEvent::JobProgress {
                    job_id: jid.clone(),
                    stage: msg.to_string(),
                    percent,
                });
            });

            if mode == "live" {
                let jid_live = job_id_clone.clone();
                let uid_live = user_id_for_thread.clone();
                let db_live = db_clone.clone();
                let ev_live = events_clone.clone();

                let on_reel_cb: live_pipeline::OnReel = Arc::new(move |reel_path: &Path| {
                    if let Ok(reel) = parse_and_store_reel(&db_live, &jid_live, &uid_live, reel_path) {
                        let _ = ev_live.send(WebEvent::ReelCreated {
                            job_id: jid_live.clone(),
                            reel,
                        });
                    }
                });

                let lj = LiveJob {
                    url: stream_url.clone(),
                    out_dir,
                    max_reels: count,
                    should_stop: Some(should_stop),
                    on_reel: Some(on_reel_cb),
                    subtitle_lang,
                    ..Default::default()
                };
                live_pipeline::run_live(&lj)
            } else {
                // VOD highlights mode
                let rj = ReelJob {
                    source_url: stream_url.clone(),
                    auto_count: count.max(1),
                    out_dir,
                    channel: stream_url.clone(),
                    subtitle_lang,
                    progress: Some(progress_cb),
                    ..Default::default()
                };
                pipeline::run_job(&rj)
            }
        })
        .await;

        let now = Utc::now().to_rfc3339();
        match result {
            Ok(Ok(reel_paths)) => {
                log(format!("[worker] job {job_id} finished, produced {} reels", reel_paths.len()));
                for path in &reel_paths {
                    if let Ok(reel) = parse_and_store_reel(&db, &job_id, &user_id, path) {
                        let _ = events_tx.send(WebEvent::ReelCreated {
                            job_id: job_id.clone(),
                            reel,
                        });
                    }
                }
                let _ = db.update_job_progress(&job_id, 100, "Готово!");
                let _ = db.update_job_status(&job_id, "completed", None, Some(&now));
                let _ = events_tx.send(WebEvent::JobStatus {
                    job_id: job_id.clone(),
                    status: "completed".to_string(),
                    error: None,
                });
            }
            Ok(Err(err)) => {
                let err_str = err.to_string();
                log(format!("[worker] job {job_id} failed: {err_str}"));
                let _ = db.update_job_status(&job_id, "failed", Some(&err_str), Some(&now));
                let _ = events_tx.send(WebEvent::JobStatus {
                    job_id: job_id.clone(),
                    status: "failed".to_string(),
                    error: Some(err_str),
                });
            }
            Err(join_err) => {
                let err_str = join_err.to_string();
                log(format!("[worker] job {job_id} panicked: {err_str}"));
                let _ = db.update_job_status(&job_id, "failed", Some(&err_str), Some(&now));
                let _ = events_tx.send(WebEvent::JobStatus {
                    job_id: job_id.clone(),
                    status: "failed".to_string(),
                    error: Some(err_str),
                });
            }
        }

        if let Ok(mut map) = active_stops.lock() {
            map.remove(&job_id);
        }
    }
}

/// Helper: parse generated `.mp4` and its sidecar `.txt`, insert into DB
fn parse_and_store_reel(
    db: &Database,
    job_id: &str,
    user_id: &str,
    path: &Path,
) -> Result<Reel> {
    let sidecar_path = path.with_extension("txt");
    let mut title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Новый рилс").to_string();
    let mut description = String::new();
    let mut tags = "#shorts #clips #stream".to_string();

    if sidecar_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&sidecar_path) {
            let lines: Vec<&str> = content.lines().collect();
            if let Some(first) = lines.first() {
                title = first.trim_start_matches('#').trim().to_string();
            }
            if lines.len() > 2 {
                description = lines[1..lines.len() - 1].join("\n").trim().to_string();
            }
            if let Some(last) = lines.last() {
                if last.contains('#') {
                    tags = last.trim().to_string();
                }
            }
        }
    }

    let dur = duration(path);
    let reel_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    let reel = Reel {
        id: reel_id,
        job_id: job_id.to_string(),
        user_id: user_id.to_string(),
        title,
        description,
        tags,
        file_path: path.display().to_string(),
        duration_secs: dur,
        hype_score: 0.95,
        is_unlocked: false,
        created_at: now,
    };

    db.create_reel(&reel)?;
    Ok(reel)
}
