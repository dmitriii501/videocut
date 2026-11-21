//! Auto-generate a title / description / hashtags for a reel.
//!
//! Without an LLM key this is a heuristic over the transcript: the first clean
//! sentence becomes the title, the full (trimmed) transcript the description,
//! and a few salient words become hashtags. If an OpenAI-compatible endpoint is
//! configured (`STREAMREELS_LLM_API_KEY` + optional `STREAMREELS_LLM_URL` /
//! `STREAMREELS_LLM_MODEL`) we ask the model instead and fall back on any error.

use std::collections::HashSet;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::http;
use crate::utils::{log, retry};

/// Generated description bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Description {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hashtags: Vec<String>,
}

/// Filler / non-content words to skip when guessing hashtags (ru + en).
const STOP: &[&str] = &[
    "и", "в", "во", "не", "что", "он", "на", "я", "с", "со", "как", "а", "то", "все", "она",
    "так", "его", "но", "да", "ты", "к", "у", "же", "вы", "за", "бы", "по", "только", "ее",
    "мне", "было", "вот", "от", "меня", "еще", "нет", "о", "из", "ему", "теперь", "когда",
    "даже", "ну", "вдруг", "ли", "если", "уже", "или", "ни", "быть", "был", "него", "до",
    "вас", "нибудь", "опять", "уж", "вам", "ведь", "там", "потом", "себя", "ничего", "ей",
    "может", "они", "тут", "где", "есть", "надо", "ней", "для", "мы", "тебя", "их", "чем",
    "была", "сам", "чтоб", "без", "будто", "чего", "раз", "тоже", "себе", "под", "будет",
    "ж", "тогда", "кто", "этот", "того", "потому", "этого", "какой", "совсем", "ним",
    "здесь", "этом", "один", "почти", "мой", "тем", "чтобы", "нее", "сейчас", "были", "куда",
    "зачем", "всех", "никогда", "можно", "при", "наконец", "два", "об", "другой", "хоть",
    "после", "над", "больше", "тот", "через", "эти", "нас", "про", "всего", "них", "какая",
    "много", "разве", "три", "эту", "моя", "впрочем", "хорошо", "свою", "этой", "перед",
    "иногда", "лучше", "чуть", "том", "нельзя", "такой", "им", "более", "всегда", "конечно",
    "всю", "между", "the", "a", "an", "and", "or", "but", "to", "of", "in", "on", "is", "it",
    "this", "that", "for", "with", "as", "at", "be", "are", "was", "were", "so", "you", "i",
    "we", "they", "he", "she", "just", "like", "get",
];

fn stop_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| STOP.iter().copied().collect())
}

fn word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-zА-Яа-яЁё0-9]{4,}").unwrap())
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-zА-Яа-яЁё0-9]+").unwrap())
}

fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// Split on sentence terminators followed by whitespace (no lookbehind needed).
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        cur.push(ch);
        let is_term = matches!(ch, '.' | '!' | '?' | '…');
        let next_ws = chars.get(i + 1).map(|c| c.is_whitespace()).unwrap_or(false);
        if is_term && next_ws {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            cur.clear();
            // skip the run of whitespace
            while i + 1 < chars.len() && chars[i + 1].is_whitespace() {
                i += 1;
            }
        }
        i += 1;
    }
    let s = cur.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

fn hashtags(text: &str, limit: usize) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let stop = stop_set();
    for m in word_re().find_iter(&text.to_lowercase()) {
        let w = m.as_str();
        if stop.contains(w) || seen.iter().any(|s| s == w) {
            continue;
        }
        seen.push(w.to_string());
        if seen.len() >= limit {
            break;
        }
    }
    seen.into_iter().map(|w| format!("#{w}")).collect()
}

/// First sentence, trimmed to `limit` chars on a word boundary.
fn short_title(text: &str, limit: usize) -> String {
    let sents = sentences(text);
    let cand = sents
        .iter()
        .find(|s| s.chars().count() >= 12)
        .cloned()
        .unwrap_or_else(|| text.to_string());
    let cand = cand.trim_matches(|c| matches!(c, ' ' | '.' | ',' | '!' | '…' | '-'));
    if cand.chars().count() <= limit {
        return cand.to_string();
    }
    let head: String = cand.chars().take(limit).collect();
    let cut = match head.rsplit_once(' ') {
        Some((left, _)) => left,
        None => head.as_str(),
    };
    format!(
        "{}…",
        cut.trim_end_matches([' ', '.', ',', '!', '…', '-'])
    )
}

fn heuristic(transcript: &str, channel: &str) -> Description {
    let text = clean(transcript);
    let mut title = short_title(&text, config::describe_title_max());
    if title.is_empty() {
        title = "Стрим-момент".to_string();
    }
    let desc = if text.is_empty() {
        "Момент со стрима.".to_string()
    } else {
        text.chars().take(config::describe_desc_max()).collect()
    };

    // Hashtags: skip words already in the title to avoid repetition.
    let title_words: HashSet<String> = token_re()
        .find_iter(&title)
        .map(|m| m.as_str().to_lowercase())
        .collect();
    let body = text
        .split_whitespace()
        .filter(|w| {
            let stripped = w.trim_matches(|c| matches!(c, '.' | ',' | '!' | '?' | '…'));
            !title_words.contains(&stripped.to_lowercase())
        })
        .collect::<Vec<_>>()
        .join(" ");

    let mut tags = hashtags(&body, 5);
    tags.extend(config::default_hashtags());
    if !channel.is_empty() {
        let ch: String = channel
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !ch.is_empty() {
            tags.push(format!("#{ch}"));
        }
    }

    // dedupe, keep order (case-insensitive)
    let mut out_tags: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for t in tags {
        if seen.insert(t.to_lowercase()) {
            out_tags.push(t);
        }
    }

    Description {
        title,
        description: desc,
        hashtags: out_tags,
    }
}

fn extract_json(content: &str) -> Option<String> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(content[start..=end].to_string())
}

fn llm(transcript: &str, channel: &str) -> Option<Description> {
    let key = config::llm_api_key()?;
    let base = config::llm_url();
    let model = config::llm_model();

    let body_text: String = transcript.chars().take(3000).collect();
    let prompt = format!(
        "Ты — редактор коротких вертикальных видео (Reels/Shorts/TikTok). \
По транскрипту момента со стрима придумай цепляющий заголовок (до 80 \
символов), описание (до 300 символов) и 3-6 хештегов. \
Ответь строго JSON: {{\"title\":...,\"description\":...,\"hashtags\":[...]}}.\n\n\
Канал: {channel}\nТранскрипт:\n{body_text}"
    );

    let payload = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": prompt }],
        "temperature": config::llm_temperature(),
    });

    let url = format!("{base}/chat/completions");
    let auth = format!("Bearer {key}");
    let headers = [("Authorization", auth.as_str())];

    let call = || -> Result<Description> {
        let data = http::post_json(&url, &headers, &payload, 60)?;
        let content = data["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("llm response missing content"))?;
        let json_str = extract_json(content).unwrap_or_else(|| content.to_string());
        let desc: Description = serde_json::from_str(&json_str)?;
        if desc.title.is_empty() {
            anyhow::bail!("llm response missing title");
        }
        Ok(desc)
    };

    match retry(call, 3, 2.0, "llm describe") {
        Ok(d) => Some(d),
        Err(exc) => {
            log(format!("describe: LLM failed ({exc}); using heuristic"));
            None
        }
    }
}

/// Return title/description/hashtags for the given transcript.
pub fn describe(transcript: &str, channel: &str) -> Description {
    if let Some(res) = llm(transcript, channel) {
        res
    } else {
        heuristic(transcript, channel)
    }
}

/// Render description info into a Telegram caption (<= max_len).
pub fn as_caption(info: &Description, max_len: usize) -> String {
    let title = info.title.trim();
    let desc = info.description.trim();
    let tags = info.hashtags.join(" ");
    let caption = format!("{title}\n\n{desc}\n\n{tags}")
        .trim()
        .to_string();
    if caption.chars().count() > max_len {
        let head: String = caption.chars().take(max_len.saturating_sub(1)).collect();
        format!("{}…", head.trim_end())
    } else {
        caption
    }
}

/// Render description info as a plain markdown-ish text file body.
pub fn as_text_file(info: &Description) -> String {
    format!(
        "# {}\n\n{}\n\n{}\n",
        info.title,
        info.description,
        info.hashtags.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_collapses_whitespace() {
        assert_eq!(clean("  hello   world \n\t!"), "hello world !");
    }

    #[test]
    fn sentences_split_on_terminators() {
        let s = sentences("Привет мир. Как дела? Хорошо!…");
        assert_eq!(s, vec!["Привет мир.", "Как дела?", "Хорошо!…"]);
    }

    #[test]
    fn short_title_trimmed_on_word_boundary() {
        let t = short_title("Это очень длинное предложение которое точно превышает лимит символов", 20);
        assert!(t.chars().count() <= 21, "got {t:?}");
        assert!(t.ends_with('…'));
    }

    #[test]
    fn hashtags_skip_stopwords_and_dupes() {
        let tags = hashtags("стрим стрим играем в игру the and", 5);
        assert_eq!(tags, vec!["#стрим", "#играем", "#игру"]);
    }

    #[test]
    fn heuristic_produces_all_fields() {
        let d = heuristic("Привет всем. Сегодня играем в новую игру!", "streamer_1");
        assert!(!d.title.is_empty());
        assert!(!d.description.is_empty());
        assert!(d.hashtags.iter().any(|t| t == "#streamer_1"));
        assert!(d.hashtags.iter().any(|t| t == "#стрим"));
    }

    #[test]
    fn caption_truncates() {
        let d = Description {
            title: "T".into(),
            description: "D".repeat(2000),
            hashtags: vec!["#a".into()],
        };
        let c = as_caption(&d, 100);
        assert!(c.chars().count() <= 100);
        assert!(c.ends_with('…'));
    }
}
