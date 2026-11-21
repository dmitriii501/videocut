//! Telegram bot: accept a stream/VOD URL and send back finished reels.
//!
//! * Long-polls `getUpdates` on the main thread (no webhook needed).
//! * All heavy work runs in a **single** worker thread consuming a queue, so only
//!   one whisper job is ever in flight (this box OOMs with two in parallel).
//! * A message that contains a URL is classified:
//!     - Twitch channel (`twitch.tv/<name>`, no `/videos/`) -> live mode.
//!     - Twitch VOD / clip / YouTube -> VOD mode.
//! * `/stop` aborts the current live job (sets an atomic flag that `run_live`
//!   polls through `LiveJob.should_stop`).

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::config;
use crate::live_pipeline::{run_live, LiveJob};
use crate::pipeline::{run_job, ReelJob};
use crate::telegram;
use crate::utils::{ensure_dir, log, timestamp};

/// Minimal URL matcher: `http(s)://` followed by non-whitespace.
fn find_url(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("http") {
        let start = i + pos;
        let rest = &text[start..];
        let schem_len = if rest.to_lowercase().starts_with("https://") {
            8
        } else if rest.to_lowercase().starts_with("http://") {
            7
        } else {
            i = start + 4;
            continue;
        };
        let after = &text[start + schem_len..];
        let end = after
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(idx, _)| idx)
            .unwrap_or(after.len());
        let url = &text[start..start + schem_len + end];
        if !url.is_empty() {
            let trimmed = url.trim_end_matches(&['.', ',', ')', '>', ']'][..]);
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        i = start + schem_len;
    }
    None
}

/// Return `"live"` or `"vod"` for a URL.
pub fn classify(url: &str) -> &'static str {
    let u = url.to_lowercase();
    if u.contains("twitch.tv/") {
        let tail = u.split_once("twitch.tv/").map(|(_, t)| t).unwrap_or("");
        if u.contains("/videos/") || u.contains("/clip/") || u.contains("clips.twitch.tv") {
            return "vod";
        }
        let head = tail.split('/').next().unwrap_or("").split('?').next().unwrap_or("");
        if !head.is_empty() {
            return "live";
        }
    }
    "vod"
}

fn env_int(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(default)
}

fn default_lang() -> Option<String> {
    let lang = std::env::var("STREAMREELS_BOT_LANG")
        .unwrap_or_else(|_| "ru".to_string())
        .trim()
        .to_lowercase();
    if lang.is_empty() { None } else { Some(lang) }
}

fn allowed_chats() -> HashSet<i64> {
    let mut ids: HashSet<i64> = config::tg_allowed().into_iter().collect();
    if let Some(id) = config::tg_chat_id() {
        ids.insert(id);
    }
    ids
}

#[derive(Debug, Clone)]
struct Task {
    chat: i64,
    url: String,
    kind: &'static str,
    stop: Arc<AtomicBool>,
}

/// Shared state handed to the worker thread.
struct Shared {
    q: Mutex<VecDeque<Task>>,
    cvar: Condvar,
    current: Mutex<Option<Task>>,
    current_desc: Mutex<String>,
    lang: Option<String>,
    auto: i64,
    max_live: i64,
}

impl Shared {
    fn push(&self, task: Task) {
        self.q.lock().unwrap().push_back(task);
        self.cvar.notify_one();
    }

    fn qsize(&self) -> usize {
        self.q.lock().unwrap().len()
    }

    fn send_sidecar(video: &Path, chat: i64) {
        let sidecar = video.with_extension("txt");
        let mut caption = String::new();
        if sidecar.exists() {
            if let Ok(text) = std::fs::read_to_string(&sidecar) {
                caption = text.trim().to_string();
            }
        }
        telegram::send_video(video, &caption, Some(chat), 0.0);
    }

    fn run_vod(&self, task: &Task) {
        *self.current_desc.lock().unwrap() = format!("VOD {}", task.url);
        telegram::send_message(
            &format!("Взял в работу: {}\nИщу {} лучших моментов…", task.url, self.auto),
            Some(task.chat),
            0.0,
        );
        let out_dir = ensure_dir(&config::output().join("bot").join(format!("{}-vod", timestamp())))
            .unwrap_or_else(|_| config::output());
        let job = ReelJob {
            source_url: task.url.clone(),
            segments: Vec::new(),
            auto_count: self.auto.max(0) as usize,
            out_dir,
            channel: task.url.clone(),
            subtitle_lang: self.lang.clone(),
            ..Default::default()
        };
        let produced = match run_job(&job) {
            Ok(p) => p,
            Err(exc) => {
                telegram::send_message(&format!("Не получилось: {exc}"), Some(task.chat), 0.0);
                return;
            }
        };
        if produced.is_empty() {
            telegram::send_message("Моментов не нашёл.", Some(task.chat), 0.0);
            return;
        }
        telegram::send_message(
            &format!("Готово: {} ролик(ов), отправляю…", produced.len()),
            Some(task.chat),
            0.0,
        );
        for video in produced {
            if task.stop.load(Ordering::SeqCst) {
                telegram::send_message("Остановлено.", Some(task.chat), 0.0);
                return;
            }
            Self::send_sidecar(&video, task.chat);
        }
    }

    fn run_live_task(&self, task: &Task) {
        *self.current_desc.lock().unwrap() = format!("live {}", task.url);
        telegram::send_message(
            &format!(
                "Смотрю стрим: {}\nСделаю {} ролика и пришлю. Отменить — /stop",
                task.url, self.max_live
            ),
            Some(task.chat),
            0.0,
        );
        let out_dir = ensure_dir(&config::output().join("bot").join(format!("{}-live", timestamp())))
            .unwrap_or_else(|_| config::output());
        let chat = task.chat;
        let stop = task.stop.clone();
        let on_reel: Arc<dyn Fn(&Path) + Send + Sync> = Arc::new(move |video: &Path| {
            if !stop.load(Ordering::SeqCst) {
                Shared::send_sidecar(video, chat);
            }
        });
        let job = LiveJob {
            url: task.url.clone(),
            out_dir,
            max_reels: self.max_live.max(0) as usize,
            subtitle_lang: self.lang.clone(),
            should_stop: Some(task.stop.clone()),
            on_reel: Some(on_reel),
            ..Default::default()
        };
        let produced = match run_live(&job) {
            Ok(p) => p,
            Err(exc) => {
                telegram::send_message(&format!("Не получилось: {exc}"), Some(task.chat), 0.0);
                return;
            }
        };
        if task.stop.load(Ordering::SeqCst) {
            telegram::send_message("Стрим остановлен по команде /stop.", Some(task.chat), 0.0);
        } else if produced.is_empty() {
            telegram::send_message("Не поймал ни одного момента, попробуй ещё.", Some(task.chat), 0.0);
        } else {
            telegram::send_message(
                &format!("Готово: {} ролик(ов) со стрима.", produced.len()),
                Some(task.chat),
                0.0,
            );
        }
    }

    fn worker_loop(self: Arc<Self>) {
        loop {
            let task = {
                let mut guard = self.q.lock().unwrap();
                while guard.is_empty() {
                    guard = self.cvar.wait(guard).unwrap();
                }
                match guard.pop_front() {
                    Some(t) => t,
                    None => return,
                }
            };
            *self.current.lock().unwrap() = Some(task.clone());
            if task.kind == "live" {
                self.run_live_task(&task);
            } else {
                self.run_vod(&task);
            }
            *self.current.lock().unwrap() = None;
            *self.current_desc.lock().unwrap() = "idle".to_string();
        }
    }
}

const HELP: &str = "Привет! Пришли ссылку — сделаю вертикальные рилсы с русскими \
субтитрами.\n\n\
• Ссылка на Twitch-канал → смотрю live и ловлю моменты\n\
• Ссылка на VOD (Twitch/YouTube) → нарезаю лучшие моменты\n\n\
Команды:\n\
/status — что сейчас в работе\n\
/stop — остановить текущий live\n\
/help — эта справка";

pub struct Bot {
    allowed: HashSet<i64>,
    lock_chat: Option<i64>,
    shared: Arc<Shared>,
    offset: Option<i64>,
}

impl Bot {
    pub fn new() -> Self {
        Bot {
            allowed: allowed_chats(),
            lock_chat: None,
            shared: Arc::new(Shared {
                q: Mutex::new(VecDeque::new()),
                cvar: Condvar::new(),
                current: Mutex::new(None),
                current_desc: Mutex::new("idle".to_string()),
                lang: default_lang(),
                auto: env_int("STREAMREELS_BOT_AUTO", 3),
                max_live: env_int("STREAMREELS_BOT_LIVE_REELS", 3),
            }),
            offset: None,
        }
    }

    fn authorized(&mut self, chat: i64) -> bool {
        if !self.allowed.is_empty() {
            return self.allowed.contains(&chat);
        }
        if self.lock_chat.is_none() {
            self.lock_chat = Some(chat);
            log(format!(
                "bot: no allowlist; locked to chat {chat} (set STREAMREELS_TG_ALLOWED to change)"
            ));
            return true;
        }
        self.lock_chat == Some(chat)
    }

    fn handle(&mut self, update: &Value) {
        let msg = update.get("message").or_else(|| update.get("edited_message"));
        let msg = match msg {
            Some(m) if m.is_object() => m,
            _ => return,
        };
        let chat = match msg.get("chat").and_then(|c| c.get("id")).and_then(|v| v.as_i64()) {
            Some(c) => c,
            None => return,
        };
        if !self.authorized(chat) {
            log(format!("bot: ignoring unauthorized chat {chat}"));
            return;
        }
        let text = msg
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if text.is_empty() {
            telegram::send_message("Пришли ссылку на стрим или VOD.", Some(chat), 0.0);
            return;
        }

        let low = text.to_lowercase();
        if low.starts_with("/start") || low.starts_with("/help") {
            telegram::send_message(HELP, Some(chat), 0.0);
            return;
        }
        if low.starts_with("/status") {
            let current = self.shared.current.lock().unwrap().clone();
            match current {
                None => {
                    let pending = self.shared.qsize();
                    let tail = if pending > 0 {
                        format!(" В очереди: {pending}.")
                    } else {
                        String::new()
                    };
                    telegram::send_message(&format!("Сейчас ничего не делаю.{tail}"), Some(chat), 0.0);
                }
                Some(_) => {
                    let desc = self.shared.current_desc.lock().unwrap().clone();
                    telegram::send_message(&format!("В работе: {desc}"), Some(chat), 0.0);
                }
            }
            return;
        }
        if low.starts_with("/stop") {
            let current = self.shared.current.lock().unwrap().clone();
            match current {
                Some(task) if task.kind == "live" => {
                    task.stop.store(true, Ordering::SeqCst);
                    telegram::send_message("Останавливаю стрим…", Some(chat), 0.0);
                }
                _ => {
                    telegram::send_message("Сейчас нечего останавливать.", Some(chat), 0.0);
                }
            }
            return;
        }

        let url = match find_url(&text) {
            Some(u) => u,
            None => {
                telegram::send_message(
                    &format!("Не вижу ссылку. Пришли URL стрима или VOD.\n\n{HELP}"),
                    Some(chat),
                    0.0,
                );
                return;
            }
        };
        let kind = classify(&url);
        let task = Task {
            chat,
            url: url.clone(),
            kind,
            stop: Arc::new(AtomicBool::new(false)),
        };
        self.shared.push(task);
        let ahead = self.shared.qsize().saturating_sub(1);
        let wait = if ahead > 0 {
            format!(" Перед тобой в очереди: {ahead}.")
        } else {
            String::new()
        };
        let label = if kind == "live" { "live-стрим" } else { "VOD" };
        telegram::send_message(
            &format!("Принял ({label}): {url}.{wait} Начну, как освобожусь."),
            Some(chat),
            0.0,
        );
    }

    pub fn run(mut self) -> Result<()> {
        let worker = self.shared.clone();
        thread::spawn(move || Shared::worker_loop(worker));
        log("bot: polling for messages (Ctrl-C to stop)");
        if self.allowed.is_empty() {
            log("bot: WARNING no STREAMREELS_TG_ALLOWED/CHAT_ID; will lock to the first chat that writes");
        }
        loop {
            let updates = match telegram::get_updates(self.offset, 50) {
                Ok(u) => u,
                Err(exc) => {
                    log(format!("bot: getUpdates failed: {exc}"));
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            for upd in updates {
                if let Some(id) = upd.get("update_id").and_then(|v| v.as_i64()) {
                    self.offset = Some(id + 1);
                }
                self.handle(&upd);
            }
        }
    }
}

pub fn main() -> i32 {
    if let Err(exc) = Bot::new().run() {
        log(format!("bot: failed: {exc}"));
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_channel_is_live() {
        assert_eq!(classify("https://twitch.tv/kaicenat"), "live");
        assert_eq!(classify("https://www.twitch.tv/xqc"), "live");
    }

    #[test]
    fn classify_vod_and_clip() {
        assert_eq!(classify("https://twitch.tv/videos/123456"), "vod");
        assert_eq!(classify("https://twitch.tv/name/clip/slug"), "vod");
        assert_eq!(classify("https://youtu.be/abc"), "vod");
    }

    #[test]
    fn find_url_extracts_and_trims() {
        assert_eq!(
            find_url("смотри https://twitch.tv/foo, круто").as_deref(),
            Some("https://twitch.tv/foo")
        );
        assert_eq!(find_url("no link here"), None);
    }
}
