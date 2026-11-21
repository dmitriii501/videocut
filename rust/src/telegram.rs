//! Minimal Telegram Bot API client (sendVideo) over reqwest.
//!
//! Network here is flaky (TLS resets), so every call is wrapped in `retry`.
//! Configure with env `STREAMREELS_TG_TOKEN` / `STREAMREELS_TG_CHAT_ID`, or pass
//! explicitly. If no chat_id is configured we try to discover it from
//! getUpdates (the user must have sent the bot at least one message).

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::config;
use crate::utils::{log, retry};

fn token() -> Result<String> {
    config::tg_token().ok_or_else(|| {
        anyhow!(
            "telegram: set STREAMREELS_TG_TOKEN (bot token from @BotFather); \
it is intentionally not stored in the repo"
        )
    })
}

fn api(method: &str) -> Result<String> {
    Ok(format!("https://api.telegram.org/bot{}/{}", token()?, method))
}

fn client(timeout_secs: u64) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(20))
        .user_agent("streamreels/0.1")
        .build()
        .map_err(|e| anyhow!("build http client: {e}"))
}

fn check(data: Value) -> Result<Value> {
    if data["ok"].as_bool() == Some(true) {
        Ok(data)
    } else {
        Err(anyhow!("telegram error: {data}"))
    }
}

/// Long-poll getUpdates. Returns the raw update dicts (possibly empty).
pub fn get_updates(offset: Option<i64>, timeout: u64) -> Result<Vec<Value>> {
    let url = api("getUpdates")?;
    let call = || -> Result<Vec<Value>> {
        let cl = client(timeout + 20)?;
        let mut form = vec![("timeout", timeout.to_string())];
        if let Some(off) = offset {
            form.push(("offset", off.to_string()));
        }
        let resp = cl.post(&url).form(&form).send()?;
        let data: Value = resp.json()?;
        let data = check(data)?;
        Ok(data["result"].as_array().cloned().unwrap_or_default())
    };
    retry(call, 5, 1.5, "tg getUpdates")
}

/// Send a plain text message. Returns true on success.
pub fn send_message(text: &str, chat: Option<i64>, _timeout: f64) -> bool {
    let cid = chat.or_else(chat_id);
    let Some(cid) = cid else {
        log("telegram: no chat_id (send the bot a message first, or set STREAMREELS_TG_CHAT_ID)");
        return false;
    };
    let call = || -> Result<()> {
        let url = api("sendMessage")?;
        let cl = client(60)?;
        let form = vec![
            ("chat_id", cid.to_string()),
            ("text", text.to_string()),
            ("disable_web_page_preview", "true".to_string()),
        ];
        let resp = cl.post(&url).form(&form).send()?;
        let data: Value = resp.json()?;
        check(data)?;
        Ok(())
    };
    match retry(call, 3, 2.0, "telegram sendMessage") {
        Ok(()) => true,
        Err(exc) => {
            log(format!("telegram: sendMessage failed: {exc}"));
            false
        }
    }
}

/// Return the chat_id of the most recent message sent to the bot.
pub fn discover_chat_id() -> Option<i64> {
    let call = || -> Result<Option<i64>> {
        let url = api("getUpdates")?;
        let cl = client(60)?;
        let resp = cl.post(&url).form(&[("timeout", "0".to_string())]).send()?;
        let data: Value = resp.json()?;
        let data = check(data)?;
        let result = data["result"].as_array().cloned().unwrap_or_default();
        let last = result
            .iter()
            .rev()
            .find(|u| u.get("message").is_some())
            .and_then(|u| u["message"]["chat"]["id"].as_i64());
        Ok(last)
    };
    match retry(call, 4, 2.0, "tg getUpdates") {
        Ok(id) => id,
        Err(exc) => {
            log(format!("telegram: getUpdates failed: {exc}"));
            None
        }
    }
}

/// Configured chat_id, falling back to discovery from getUpdates.
pub fn chat_id() -> Option<i64> {
    if let Some(id) = config::tg_chat_id() {
        return Some(id);
    }
    discover_chat_id()
}

/// Upload a video with a caption. Returns true on success.
pub fn send_video(video: &Path, caption: &str, chat: Option<i64>, _timeout: f64) -> bool {
    let cid = chat.or_else(chat_id);
    let Some(cid) = cid else {
        log("telegram: no chat_id (send the bot a message first, or set STREAMREELS_TG_CHAT_ID)");
        return false;
    };
    let call = || -> Result<()> {
        let url = api("sendVideo")?;
        let cl = client(180)?;
        let part = reqwest::blocking::multipart::Part::file(video)?;
        let form = reqwest::blocking::multipart::Form::new()
            .text("chat_id", cid.to_string())
            .part("video", part)
            .text("caption", caption.to_string())
            .text("supports_streaming", "true");
        let resp = cl.post(&url).multipart(form).send()?;
        let data: Value = resp.json()?;
        check(data)?;
        Ok(())
    };
    match retry(call, 4, 3.0, "telegram sendVideo") {
        Ok(()) => {
            let name = video
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            log(format!("telegram: sent {name} -> chat {cid}"));
            true
        }
        Err(exc) => {
            log(format!("telegram: send failed: {exc}"));
            false
        }
    }
}
