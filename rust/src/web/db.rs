use std::path::Path;
use std::sync::{Arc, Mutex};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::web::models::{Job, Reel, User};

#[allow(dead_code)]
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database at {}", path.display()))?;
        
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                email TEXT UNIQUE NOT NULL,
                password_hash TEXT NOT NULL,
                credits INTEGER NOT NULL DEFAULT 5,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                token TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                stream_url TEXT NOT NULL,
                channel_name TEXT NOT NULL,
                mode TEXT NOT NULL,
                gameplay_preset TEXT NOT NULL,
                subtitle_lang TEXT,
                status TEXT NOT NULL,
                progress_percent INTEGER NOT NULL DEFAULT 0,
                stage TEXT NOT NULL DEFAULT '',
                error_message TEXT,
                created_at TEXT NOT NULL,
                finished_at TEXT
            );

            CREATE TABLE IF NOT EXISTS reels (
                id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                description TEXT NOT NULL,
                tags TEXT NOT NULL,
                file_path TEXT NOT NULL,
                duration_secs REAL NOT NULL DEFAULT 0.0,
                hype_score REAL NOT NULL DEFAULT 0.0,
                is_unlocked INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_jobs_user_id ON jobs(user_id);
            CREATE INDEX IF NOT EXISTS idx_reels_job_id ON reels(job_id);
            CREATE INDEX IF NOT EXISTS idx_reels_user_id ON reels(user_id);
            "#
        )?;
        Ok(())
    }

    // --- Users & Sessions ---

    pub fn create_user(&self, user: &User) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO users (id, email, password_hash, credits, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![user.id, user.email, user.password_hash, user.credits, user.created_at],
        )?;
        Ok(())
    }

    pub fn get_user_by_email(&self, email: &str) -> Result<Option<User>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, email, password_hash, credits, created_at FROM users WHERE email = ?1",
        )?;
        let mut rows = stmt.query(params![email])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                email: row.get(1)?,
                password_hash: row.get(2)?,
                credits: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    #[allow(dead_code)]
    pub fn get_user_by_id(&self, id: &str) -> Result<Option<User>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, email, password_hash, credits, created_at FROM users WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                email: row.get(1)?,
                password_hash: row.get(2)?,
                credits: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn create_session(&self, token: &str, user_id: &str, created_at: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (token, user_id, created_at) VALUES (?1, ?2, ?3)",
            params![token, user_id, created_at],
        )?;
        Ok(())
    }

    pub fn get_user_by_token(&self, token: &str) -> Result<Option<User>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT u.id, u.email, u.password_hash, u.credits, u.created_at
            FROM users u
            JOIN sessions s ON s.user_id = u.id
            WHERE s.token = ?1
            "#,
        )?;
        let mut rows = stmt.query(params![token])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                email: row.get(1)?,
                password_hash: row.get(2)?,
                credits: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_credits(&self, user_id: &str, delta: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE users SET credits = credits + ?1 WHERE id = ?2",
            params![delta, user_id],
        )?;
        let mut stmt = conn.prepare("SELECT credits FROM users WHERE id = ?1")?;
        let credits: i64 = stmt.query_row(params![user_id], |r| r.get(0))?;
        Ok(credits)
    }

    // --- Jobs ---

    pub fn create_job(&self, job: &Job) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO jobs (
                id, user_id, stream_url, channel_name, mode, gameplay_preset,
                subtitle_lang, status, progress_percent, stage, error_message,
                created_at, finished_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            "#,
            params![
                job.id,
                job.user_id,
                job.stream_url,
                job.channel_name,
                job.mode,
                job.gameplay_preset,
                job.subtitle_lang,
                job.status,
                job.progress_percent,
                job.stage,
                job.error_message,
                job.created_at,
                job.finished_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_job_progress(&self, job_id: &str, percent: i32, stage: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET progress_percent = ?1, stage = ?2 WHERE id = ?3",
            params![percent, stage, job_id],
        )?;
        Ok(())
    }

    pub fn update_job_status(
        &self,
        job_id: &str,
        status: &str,
        error: Option<&str>,
        finished_at: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE jobs SET status = ?1, error_message = ?2, finished_at = ?3 WHERE id = ?4",
            params![status, error, finished_at, job_id],
        )?;
        Ok(())
    }

    pub fn get_job(&self, job_id: &str) -> Result<Option<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, user_id, stream_url, channel_name, mode, gameplay_preset,
                   subtitle_lang, status, progress_percent, stage, error_message,
                   created_at, finished_at
            FROM jobs WHERE id = ?1
            "#,
        )?;
        let mut rows = stmt.query(params![job_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Job {
                id: row.get(0)?,
                user_id: row.get(1)?,
                stream_url: row.get(2)?,
                channel_name: row.get(3)?,
                mode: row.get(4)?,
                gameplay_preset: row.get(5)?,
                subtitle_lang: row.get(6)?,
                status: row.get(7)?,
                progress_percent: row.get(8)?,
                stage: row.get(9)?,
                error_message: row.get(10)?,
                created_at: row.get(11)?,
                finished_at: row.get(12)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self, user_id: &str) -> Result<Vec<Job>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, user_id, stream_url, channel_name, mode, gameplay_preset,
                   subtitle_lang, status, progress_percent, stage, error_message,
                   created_at, finished_at
            FROM jobs WHERE user_id = ?1
            ORDER BY created_at DESC
            "#,
        )?;
        let mut rows = stmt.query(params![user_id])?;
        let mut list = Vec::new();
        while let Some(row) = rows.next()? {
            list.push(Job {
                id: row.get(0)?,
                user_id: row.get(1)?,
                stream_url: row.get(2)?,
                channel_name: row.get(3)?,
                mode: row.get(4)?,
                gameplay_preset: row.get(5)?,
                subtitle_lang: row.get(6)?,
                status: row.get(7)?,
                progress_percent: row.get(8)?,
                stage: row.get(9)?,
                error_message: row.get(10)?,
                created_at: row.get(11)?,
                finished_at: row.get(12)?,
            });
        }
        Ok(list)
    }

    // --- Reels ---

    pub fn create_reel(&self, reel: &Reel) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO reels (
                id, job_id, user_id, title, description, tags,
                file_path, duration_secs, hype_score, is_unlocked, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            "#,
            params![
                reel.id,
                reel.job_id,
                reel.user_id,
                reel.title,
                reel.description,
                reel.tags,
                reel.file_path,
                reel.duration_secs,
                reel.hype_score,
                if reel.is_unlocked { 1 } else { 0 },
                reel.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_reel(&self, reel_id: &str) -> Result<Option<Reel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, job_id, user_id, title, description, tags,
                   file_path, duration_secs, hype_score, is_unlocked, created_at
            FROM reels WHERE id = ?1
            "#,
        )?;
        let mut rows = stmt.query(params![reel_id])?;
        if let Some(row) = rows.next()? {
            let is_unlocked: i32 = row.get(9)?;
            Ok(Some(Reel {
                id: row.get(0)?,
                job_id: row.get(1)?,
                user_id: row.get(2)?,
                title: row.get(3)?,
                description: row.get(4)?,
                tags: row.get(5)?,
                file_path: row.get(6)?,
                duration_secs: row.get(7)?,
                hype_score: row.get(8)?,
                is_unlocked: is_unlocked != 0,
                created_at: row.get(10)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_reels_for_user(&self, user_id: &str) -> Result<Vec<Reel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, job_id, user_id, title, description, tags,
                   file_path, duration_secs, hype_score, is_unlocked, created_at
            FROM reels WHERE user_id = ?1
            ORDER BY created_at DESC
            "#,
        )?;
        let mut rows = stmt.query(params![user_id])?;
        let mut list = Vec::new();
        while let Some(row) = rows.next()? {
            let is_unlocked: i32 = row.get(9)?;
            list.push(Reel {
                id: row.get(0)?,
                job_id: row.get(1)?,
                user_id: row.get(2)?,
                title: row.get(3)?,
                description: row.get(4)?,
                tags: row.get(5)?,
                file_path: row.get(6)?,
                duration_secs: row.get(7)?,
                hype_score: row.get(8)?,
                is_unlocked: is_unlocked != 0,
                created_at: row.get(10)?,
            });
        }
        Ok(list)
    }

    pub fn list_reels_for_job(&self, job_id: &str) -> Result<Vec<Reel>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, job_id, user_id, title, description, tags,
                   file_path, duration_secs, hype_score, is_unlocked, created_at
            FROM reels WHERE job_id = ?1
            ORDER BY created_at DESC
            "#,
        )?;
        let mut rows = stmt.query(params![job_id])?;
        let mut list = Vec::new();
        while let Some(row) = rows.next()? {
            let is_unlocked: i32 = row.get(9)?;
            list.push(Reel {
                id: row.get(0)?,
                job_id: row.get(1)?,
                user_id: row.get(2)?,
                title: row.get(3)?,
                description: row.get(4)?,
                tags: row.get(5)?,
                file_path: row.get(6)?,
                duration_secs: row.get(7)?,
                hype_score: row.get(8)?,
                is_unlocked: is_unlocked != 0,
                created_at: row.get(10)?,
            });
        }
        Ok(list)
    }

    pub fn unlock_reel(&self, reel_id: &str, user_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let affected = conn.execute(
            "UPDATE reels SET is_unlocked = 1 WHERE id = ?1 AND user_id = ?2 AND is_unlocked = 0",
            params![reel_id, user_id],
        )?;
        Ok(affected > 0)
    }
}
