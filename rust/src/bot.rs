//! Telegram bot: accept a stream/VOD URL and send back finished reels.
//!
//! * Long-polls `getUpdates` on the main thread (no webhook needed).
//! * All heavy work runs in a **single** worker thread consuming a queue, so only
//!   one whisper job is ever in flight (this box OOMs with two in parallel).
//! * A message that contains a URL is classified:
//!     - Twitch channel (`twitch.tv/<name>`, no `/videos/`) -> live mode.
//!     - Twitch VOD / clip / YouTube -> VOD mode.
//! * The UI is HTML-formatted with emoji, an editable live status message and
//!   inline-keyboard buttons (Stop / Status) wired to callback queries.
//! * `/stop` (or the Stop button) aborts the current live job via an atomic flag
//!   that `run_live` polls through `LiveJob.should_stop`.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

use crate::config;
use crate::livestream::live_pipeline::{run_live, LiveJob};
use crate::vod::pipeline::{run_job, Progress, ReelJob};
use crate::telegram;
use crate::utils::{ensure_dir, html_escape, log, timestamp};

const PARSE_HTML: &str = "HTML";

fn status_keyboard() -> String {
    r#"{"inline_keyboard":[[{"text":"🔄 Статус","callback_data":"status"}]]}"#.to_string()
}

fn stop_keyboard() -> String {
    r#"{"inline_keyboard":[[{"text":"⏹ Остановить","callback_data":"stop"}]]}"#.to_string()
}

fn empty_keyboard() -> &'static str {
    r#"{"inline_keyboard":[]}"#
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

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

/// A single editable "status" message in a chat, updated in place.
struct StatusMsg {
    chat: i64,
    id: i64,
    header: String,
    markup: Option<String>,
    last: Mutex<String>,
}

impl StatusMsg {
    fn new(chat: i64, id: i64, header: String, markup: Option<String>) -> Self {
        StatusMsg {
            chat,
            id,
            header,
            markup,
            last: Mutex::new(String::new()),
        }
    }

    fn render(&self, note: &str) -> String {
        if note.is_empty() {
            self.header.clone()
        } else {
            format!("{}\n\n{}", self.header, note)
        }
    }

    /// Edit the message, skipping identical updates (avoids API spam).
    fn update(&self, note: &str) {
        if self.id == 0 {
            return;
        }
        let html = self.render(note);
        let mut last = self.last.lock().unwrap();
        if *last == html {
            return;
        }
        if telegram::edit_message_text(&html, self.chat, self.id, Some(PARSE_HTML), self.markup.as_deref())
        {
            *last = html;
        }
    }

    /// Final update that also removes the inline keyboard.
    fn finalize(&self, note: &str) {
        if self.id == 0 {
            return;
        }
        let html = self.render(note);
        telegram::edit_message_text(&html, self.chat, self.id, Some(PARSE_HTML), Some(empty_keyboard()));
        *self.last.lock().unwrap() = html;
    }
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
        let caption = truncate_chars(&html_escape(&caption), 1024);
        telegram::send_chat_action(chat, "upload_video");
        telegram::send_video(video, &caption, Some(chat), 0.0);
    }

    fn run_vod(&self, task: &Task) {
        *self.current_desc.lock().unwrap() = format!("VOD {}", task.url);
        let header = format!("📼 <b>VOD</b>\n<code>{}</code>", html_escape(&task.url));
        let kb = status_keyboard();
        let msg_id = telegram::send_message_ex(
            &format!("{header}\n\n⏳ Беру в работу…"),
            Some(task.chat),
            Some(PARSE_HTML),
            Some(&kb),
            0.0,
        )
        .unwrap_or(0);
        let status = Arc::new(StatusMsg::new(task.chat, msg_id, header, Some(kb)));
        status.update(&format!(
            "⏳ Беру в работу, ищу {} лучших моментов…",
            self.auto
        ));

        let out_dir = ensure_dir(&config::output().join("bot").join(format!("{}-vod", timestamp())))
            .unwrap_or_else(|_| config::output());

        let progress: Progress = {
            let st = status.clone();
            let chat = task.chat;
            Arc::new(move |note: &str| {
                telegram::send_chat_action(chat, "typing");
                st.update(note);
            })
        };

        let job = ReelJob {
            source_url: task.url.clone(),
            segments: Vec::new(),
            auto_count: self.auto.max(0) as usize,
            out_dir,
            channel: task.url.clone(),
            subtitle_lang: self.lang.clone(),
            progress: Some(progress),
            ..Default::default()
        };
        let produced = match run_job(&job) {
            Ok(p) => p,
            Err(exc) => {
                status.finalize(&format!(
                    "❌ <b>Ошибка</b>\n<code>{}</code>",
                    html_escape(&exc.to_string())
                ));
                return;
            }
        };
        if produced.is_empty() {
            status.finalize("🤷 Моментов не нашёл.");
            return;
        }
        status.update(&format!(
            "📤 Готово: {} ролик(ов), отправляю…",
            produced.len()
        ));
        let mut sent = 0usize;
        for video in produced {
            if task.stop.load(Ordering::SeqCst) {
                status.finalize(&format!("⏹ Остановлено. Отправлено: {sent}."));
                return;
            }
            Self::send_sidecar(&video, task.chat);
            sent += 1;
        }
        status.finalize(&format!(
            "✅ Отправил {sent} ролик(ов). Приятного просмотра!"
        ));
    }

    fn run_live_task(&self, task: &Task) {
        *self.current_desc.lock().unwrap() = format!("live {}", task.url);
        let header = format!("🔴 <b>LIVE</b>\n<code>{}</code>", html_escape(&task.url));
        let kb = stop_keyboard();
        let msg_id = telegram::send_message_ex(
            &format!("{header}\n\n👀 Смотрю стрим, ловлю хайп…"),
            Some(task.chat),
            Some(PARSE_HTML),
            Some(&kb),
            0.0,
        )
        .unwrap_or(0);
        let status = Arc::new(StatusMsg::new(task.chat, msg_id, header, Some(kb)));

        let out_dir = ensure_dir(&config::output().join("bot").join(format!("{}-live", timestamp())))
            .unwrap_or_else(|_| config::output());
        let chat = task.chat;
        let count = Arc::new(AtomicUsize::new(0));
        let on_reel: Arc<dyn Fn(&Path) + Send + Sync> = {
            let st = status.clone();
            let count = count.clone();
            let stop = task.stop.clone();
            Arc::new(move |video: &Path| {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                st.update(&format!("🎬 Поймал момент #{n}! Отправляю…"));
                Shared::send_sidecar(video, chat);
                st.update(&format!("🎬 Момент #{n} отправлен. Продолжаю смотреть…"));
            })
        };
        let job = LiveJob {
            url: task.url.clone(),
            out_dir,
            max_reels: self.max_live.max(0) as usize,
            subtitle_lang: self.lang.clone(),
            should_stop: Some(task.stop.clone()),
            on_reel: Some(on_reel),
            ..Default::default()
        };
        match run_live(&job) {
            Ok(produced) => {
                if task.stop.load(Ordering::SeqCst) {
                    status.finalize(&format!(
                        "⏹ Стрим остановлен. Поймано роликов: {}.",
                        count.load(Ordering::SeqCst)
                    ));
                } else if produced.is_empty() {
                    status.finalize("🤷 Не поймал ни одного момента, попробуй ещё.");
                } else {
                    status.finalize(&format!(
                        "✅ Стрим закончился. Отправлено роликов: {}.",
                        produced.len()
                    ));
                }
            }
            Err(exc) => {
                status.finalize(&format!(
                    "❌ <b>Ошибка</b>\n<code>{}</code>",
                    html_escape(&exc.to_string())
                ));
            }
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

const HELP: &str = "👋 <b>Привет!</b> Пришли ссылку — сделаю вертикальные рилсы \
с русскими субтитрами.\n\n\
🔴 <b>Ссылка на Twitch-канал</b> — смотрю live и ловлю хайп-моменты\n\
📼 <b>VOD / клип / YouTube</b> — нарезаю лучшие моменты\n\n\
<b>Команды</b>\n\
/status — что сейчас в работе\n\
/stop — остановить текущий стрим\n\
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

    fn handle_callback(&mut self, cb: &Value) {
        let id = cb.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let data = cb.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let chat = cb
            .get("message")
            .and_then(|m| m.get("chat"))
            .and_then(|c| c.get("id"))
            .and_then(|v| v.as_i64());
        let Some(chat) = chat else { return };
        if !self.authorized(chat) {
            return;
        }
        match data.as_str() {
            "stop" => {
                let current = self.shared.current.lock().unwrap().clone();
                match current {
                    Some(t) if t.kind == "live" => {
                        t.stop.store(true, Ordering::SeqCst);
                        telegram::answer_callback_query(&id, "Останавливаю стрим…");
                    }
                    _ => {
                        telegram::answer_callback_query(&id, "Сейчас нечего останавливать");
                    }
                }
            }
            "status" => {
                let current = self.shared.current.lock().unwrap().clone();
                let txt = match current {
                    None => {
                        let pending = self.shared.qsize();
                        if pending > 0 {
                            format!("В очереди: {pending}")
                        } else {
                            "Свободен".to_string()
                        }
                    }
                    Some(_) => self.shared.current_desc.lock().unwrap().clone(),
                };
                telegram::answer_callback_query(&id, &txt);
            }
            _ => {
                telegram::answer_callback_query(&id, "");
            }
        }
    }

    fn handle(&mut self, update: &Value) {
        if let Some(cb) = update.get("callback_query") {
            if cb.is_object() {
                self.handle_callback(cb);
            }
            return;
        }
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
            telegram::send_message_ex(
                "📎 Пришли <b>ссылку</b> на стрим или VOD.",
                Some(chat),
                Some(PARSE_HTML),
                None,
                0.0,
            );
            return;
        }

        let low = text.to_lowercase();
        if low.starts_with("/start") || low.starts_with("/help") {
            telegram::send_message_ex(HELP, Some(chat), Some(PARSE_HTML), None, 0.0);
            return;
        }
        if low.starts_with("/status") {
            let current = self.shared.current.lock().unwrap().clone();
            let body = match current {
                None => {
                    let pending = self.shared.qsize();
                    if pending > 0 {
                        format!("💤 Сейчас ничего не делаю.\nВ очереди: {pending}.")
                    } else {
                        "💤 Сейчас ничего не делаю.".to_string()
                    }
                }
                Some(_) => {
                    let desc = self.shared.current_desc.lock().unwrap().clone();
                    format!("⚙️ В работе: <code>{}</code>", html_escape(&desc))
                }
            };
            telegram::send_message_ex(&body, Some(chat), Some(PARSE_HTML), None, 0.0);
            return;
        }
        if low.starts_with("/stop") {
            let current = self.shared.current.lock().unwrap().clone();
            match current {
                Some(task) if task.kind == "live" => {
                    task.stop.store(true, Ordering::SeqCst);
                    telegram::send_message_ex(
                        "⏹ Останавливаю стрим…",
                        Some(chat),
                        Some(PARSE_HTML),
                        None,
                        0.0,
                    );
                }
                _ => {
                    telegram::send_message_ex(
                        "Сейчас нечего останавливать.",
                        Some(chat),
                        Some(PARSE_HTML),
                        None,
                        0.0,
                    );
                }
            }
            return;
        }

        let url = match find_url(&text) {
            Some(u) => u,
            None => {
                telegram::send_message_ex(
                    &format!("🔗 Не вижу ссылку. Пришли URL стрима или VOD.\n\n{HELP}"),
                    Some(chat),
                    Some(PARSE_HTML),
                    None,
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
            format!("\n\nПеред тобой в очереди: {ahead}.")
        } else {
            String::new()
        };
        let label = if kind == "live" {
            "🔴 live-стрим"
        } else {
            "📼 VOD"
        };
        telegram::send_message_ex(
            &format!(
                "✅ Принял {label}\n<code>{}</code>{wait}\n\nНачну, как освобожусь.",
                html_escape(&url)
            ),
            Some(chat),
            Some(PARSE_HTML),
            Some(&status_keyboard()),
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
    if config::tg_token().is_none() {
        log(
            "bot: STREAMREELS_TG_TOKEN is not set — get a token from @BotFather and \
export STREAMREELS_TG_TOKEN=... before starting the bot",
        );
        return 2;
    }
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

    #[test]
    fn truncate_chars_limits_and_marks() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello world", 5), "hell…");
    }
}
