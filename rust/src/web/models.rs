use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub credits: i64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPublic {
    pub id: String,
    pub email: String,
    pub credits: i64,
    pub created_at: String,
}

impl From<User> for UserPublic {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            email: u.email,
            credits: u.credits,
            created_at: u.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub user_id: String,
    pub stream_url: String,
    pub channel_name: String,
    pub mode: String, // "auto", "live", "segments"
    pub gameplay_preset: String,
    pub subtitle_lang: Option<String>,
    pub status: String, // "queued", "processing", "completed", "failed", "stopped"
    pub progress_percent: i32,
    pub stage: String,
    pub error_message: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reel {
    pub id: String,
    pub job_id: String,
    pub user_id: String,
    pub title: String,
    pub description: String,
    pub tags: String,
    #[serde(skip_serializing)]
    pub file_path: String,
    pub duration_secs: f64,
    pub hype_score: f64,
    pub is_unlocked: bool,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingPackage {
    pub id: String,
    pub name: String,
    pub credits: i64,
    pub price_rub: i64,
    pub is_popular: bool,
    pub description: String,
}

/// A pending/paid top-up order. `amount_kopecks` is what the acquirer charges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub user_id: String,
    pub package_id: String,
    pub credits: i64,
    pub amount_kopecks: i64,
    /// "pending", "paid", "canceled", "failed"
    pub status: String,
    pub tbank_payment_id: Option<String>,
    pub payment_url: Option<String>,
    pub created_at: String,
    pub paid_at: Option<String>,
}

// --- Request / Response DTOs ---

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserPublic,
}

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub stream_url: String,
    #[serde(default = "default_mode")]
    pub mode: String, // "auto" or "live"
    #[serde(default = "default_count")]
    pub count: usize, // e.g. 3
    #[serde(default = "default_gameplay")]
    pub gameplay_preset: String, // "minecraft", "subway", "gta", "asmr", "none"
    pub subtitle_lang: Option<String>,
    /// Override the mid-roll promo banner (None = service default).
    #[serde(default)]
    pub banner: Option<bool>,
    /// Manual webcam box `x0,y0,x1,y1` in 0..1. Absent = detect, or the right-hand fallback.
    #[serde(default)]
    pub webcam: Option<String>,
}

fn default_mode() -> String {
    "auto".to_string()
}
fn default_count() -> usize {
    3
}
fn default_gameplay() -> String {
    // Opt-in: a job without an explicit preset keeps the webcam band.
    "none".to_string()
}

#[derive(Debug, Deserialize)]
pub struct UnlockReelRequest {
    pub reel_id: String,
}

#[derive(Debug, Deserialize)]
pub struct TopupRequest {
    pub package_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WebEvent {
    #[serde(rename = "job_progress")]
    JobProgress {
        job_id: String,
        stage: String,
        percent: i32,
    },
    #[serde(rename = "job_status")]
    JobStatus {
        job_id: String,
        status: String,
        error: Option<String>,
    },
    #[serde(rename = "reel_created")]
    ReelCreated {
        job_id: String,
        reel: Reel,
    },
    #[serde(rename = "balance_updated")]
    BalanceUpdated {
        user_id: String,
        new_balance: i64,
    },
}
