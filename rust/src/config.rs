//! Global configuration, resolved from the environment with sane defaults.
//!
//! Nothing here is hard-coded into call sites: modules ask `config::*` for the
//! value and the operator overrides it via env vars.

use std::env;
use std::path::PathBuf;

fn env_str(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn env_list(key: &str, default: &[&str]) -> Vec<String> {
    match env::var(key) {
        Ok(v) => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => default.iter().map(|s| s.to_string()).collect(),
    }
}

/// Project root (parent of the `rust` crate directory).
pub fn root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().map(PathBuf::from).unwrap_or(manifest)
}

pub fn cache() -> PathBuf {
    env_opt("STREAMREELS_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("cache"))
}

pub fn output() -> PathBuf {
    env_opt("STREAMREELS_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| root().join("out"))
}

pub fn panels() -> PathBuf {
    cache().join("panels")
}

pub fn translate_cache() -> PathBuf {
    cache().join("translate")
}

// --- Final reel canvas -------------------------------------------------------

pub fn reel_w() -> u32 {
    env_parse("REEL_W", 720)
}

pub fn reel_h() -> u32 {
    env_parse("REEL_H", 1280)
}

pub fn fps() -> u32 {
    env_parse("REEL_FPS", 30)
}

pub fn max_source_height() -> u32 {
    env_parse("MAX_SOURCE_HEIGHT", 720)
}

// --- Render tuning -----------------------------------------------------------

pub fn video_crf() -> u32 {
    env_parse("REEL_CRF", 20)
}

pub fn video_preset() -> String {
    env_str("REEL_PRESET", "veryfast")
}

pub fn audio_bitrate() -> String {
    env_str("REEL_AUDIO_BITRATE", "128k")
}

pub fn blur_radius() -> u32 {
    env_parse("REEL_BLUR_RADIUS", 20)
}

pub fn blur_power() -> u32 {
    env_parse("REEL_BLUR_POWER", 2)
}

// --- Whisper -----------------------------------------------------------------

pub fn whisper_model() -> String {
    env_str("WHISPER_MODEL", "large-v3-turbo")
}

/// `None` means autodetect.
pub fn whisper_lang() -> Option<String> {
    env_opt("WHISPER_LANG")
}

pub fn whisper_threads() -> i32 {
    env_parse("WHISPER_THREADS", 4)
}

/// How many whisper runs may execute simultaneously (RAM bound). The model is
/// shared, but each active run needs working memory for its audio.
pub fn whisper_concurrency() -> usize {
    env_parse::<usize>("STREAMREELS_WHISPER_CONCURRENCY", 1).max(1)
}

// --- Subtitles ---------------------------------------------------------------

pub fn subtitle_font() -> String {
    env_str("SUBTITLE_FONT", "Noto Sans")
}

pub fn subtitle_words_per_caption() -> usize {
    env_parse("SUBTITLE_WORDS", 4)
}

// --- Banner (mid-roll promo) -------------------------------------------------

/// Whether to inject the animated CHIK promo banner mid-clip by default.
pub fn banner_enabled() -> bool {
    env_parse("STREAMREELS_BANNER", true)
}

/// Site line shown on the banner (e.g. "chik.cut").
pub fn banner_site() -> String {
    env_str("STREAMREELS_BANNER_SITE", "chik.cut")
}

/// On-screen duration of the banner, in seconds.
pub fn banner_duration() -> f64 {
    env_parse("STREAMREELS_BANNER_DURATION", 3.0)
}

/// Optional explicit font file for banner text (else auto-detected).
pub fn banner_font_file() -> Option<String> {
    env_opt("STREAMREELS_BANNER_FONT")
}

// --- Description -------------------------------------------------------------

pub fn default_hashtags() -> Vec<String> {
    env_list("STREAMREELS_DEFAULT_HASHTAGS", &["#стрим", "#twitch"])
}

pub fn describe_title_max() -> usize {
    env_parse("STREAMREELS_TITLE_MAX", 70)
}

pub fn describe_desc_max() -> usize {
    env_parse("STREAMREELS_DESC_MAX", 400)
}

pub fn describe_caption_max() -> usize {
    env_parse("STREAMREELS_CAPTION_MAX", 1000)
}

pub fn llm_url() -> String {
    env_str("STREAMREELS_LLM_URL", "https://api.openai.com/v1")
}

pub fn llm_model() -> String {
    env_str("STREAMREELS_LLM_MODEL", "gpt-4o-mini")
}

pub fn llm_temperature() -> f64 {
    env_parse("STREAMREELS_LLM_TEMPERATURE", 0.7)
}

pub fn llm_api_key() -> Option<String> {
    env_opt("STREAMREELS_LLM_API_KEY").or_else(|| env_opt("OPENAI_API_KEY"))
}

// --- Webcam panel detection (TypeSafe) --------------------------------------

pub fn typesafe_url() -> String {
    env_str("TYPESAFE_URL", "https://api.typesafe.ai")
}

pub fn typesafe_model() -> String {
    env_str("TYPESAFE_MODEL", "jev-latest")
}

pub fn typesafe_api_key() -> Option<String> {
    env_opt("TYPESAFE_API_KEY")
}

pub fn typesafe_samples() -> usize {
    env_parse("TYPESAFE_SAMPLES", 3)
}

// --- Telegram ----------------------------------------------------------------

pub fn tg_token() -> Option<String> {
    env_opt("STREAMREELS_TG_TOKEN")
}

pub fn tg_chat_id() -> Option<i64> {
    env_opt("STREAMREELS_TG_CHAT_ID").and_then(|v| v.parse().ok())
}

pub fn tg_allowed() -> Vec<i64> {
    match env_opt("STREAMREELS_TG_ALLOWED") {
        Some(v) => v.split(',').filter_map(|s| s.trim().parse().ok()).collect(),
        None => tg_chat_id().into_iter().collect(),
    }
}
