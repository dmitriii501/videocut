//! Translate short subtitle phrases into another language.
//!
//! Uses the free MyMemory API (no key, ~5k chars/day for anonymous callers).
//! Results are cached on disk so repeated phrases cost nothing. Any failure
//! (quota, network, empty response) falls back to the original text so a reel
//! still renders -- subtitles are just left untranslated instead of breaking
//! the run.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde_json::Value;

use crate::config;
use crate::http;
use crate::text::subtitles::Word;
use crate::utils::{log, retry};

/// MyMemory wants a plain ISO code; these are the ones we expect to use.
const SUPPORTED: &[&str] = &["ru", "en", "uk", "es", "de", "fr", "pt", "it", "pl", "tr"];

type CacheKey = (String, String, String);

fn cache() -> &'static Mutex<HashMap<CacheKey, String>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn loaded() -> &'static Mutex<HashSet<(String, String)>> {
    static LOADED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    LOADED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn cache_file(source: &str, target: &str) -> PathBuf {
    config::translate_cache().join(format!("{source}-{target}.json"))
}

fn load_cache(source: &str, target: &str) {
    let key = (source.to_string(), target.to_string());
    {
        let mut l = loaded().lock().unwrap();
        if !l.insert(key.clone()) {
            return; // already loaded
        }
    }
    let path = cache_file(source, target);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(Value::Object(map)) = serde_json::from_str(&text) else {
        return;
    };
    let mut c = cache().lock().unwrap();
    for (k, v) in &map {
        if let Some(v) = v.as_str() {
            c.insert((source.to_string(), target.to_string(), k.clone()), v.to_string());
        }
    }
    log(format!(
        "translate: loaded {} cached phrase(s) ({}->{})",
        map.len(),
        source,
        target
    ));
}

fn save_cache(source: &str, target: &str) {
    let path = cache_file(source, target);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let map: serde_json::Map<String, Value> = {
        let c = cache().lock().unwrap();
        c.iter()
            .filter(|((s, t, _), _)| s == source && t == target)
            .map(|((_, _, k), v)| (k.clone(), Value::String(v.clone())))
            .collect()
    };
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(map)) {
        let _ = std::fs::write(&path, text);
    }
}

/// Split into <=limit-char chunks on word boundaries (MyMemory caps length).
fn split(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cl = cur.chars().count();
        let wl = word.chars().count();
        if !cur.is_empty() && cl + 1 + wl > limit {
            chunks.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else if cur.is_empty() {
            cur = word.to_string();
        } else {
            cur.push(' ');
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Minimal HTML entity decoding (&amp; &lt; &gt; &quot; &#NNN; &#xHH;).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut ent = String::new();
        let mut closed = false;
        while let Some(&c) = chars.peek() {
            if c == ';' {
                chars.next();
                closed = true;
                break;
            }
            if ent.len() > 10 || !(c.is_ascii_alphanumeric() || c == '#') {
                break;
            }
            ent.push(c);
            chars.next();
        }
        if !closed {
            out.push('&');
            out.push_str(&ent);
            continue;
        }
        let decoded = match ent.as_str() {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => ent.strip_prefix('#').and_then(|num| {
                let code = if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    num.parse::<u32>().ok()
                };
                code.and_then(char::from_u32)
            }),
        };
        match decoded {
            Some(c) => out.push(c),
            None => {
                out.push('&');
                out.push_str(&ent);
                out.push(';');
            }
        }
    }
    out
}

fn mymemory(text: &str, source: &str, target: &str) -> Result<String> {
    let q: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let url = format!("https://api.mymemory.translated.net/get?q={q}&langpair={source}|{target}");
    let data: Value = http::get_json(&url, &[], 30)?;
    if data["quotaFinished"].as_bool().unwrap_or(false) {
        anyhow::bail!("MyMemory daily quota finished");
    }
    let status = &data["responseStatus"];
    let translated = data["responseData"]["translatedText"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let ok_status = status.is_null() || status.as_i64() == Some(200) || status.as_str() == Some("200");
    if !ok_status && translated.is_empty() {
        anyhow::bail!("MyMemory status {status}: {}", data["responseDetails"]);
    }
    if translated.is_empty() {
        anyhow::bail!("empty translation");
    }
    let upper = translated.to_uppercase();
    if ["MYMEMORY WARNING", "PLEASE SELECT", "QUERY LENGTH LIMIT"]
        .iter()
        .any(|p| upper.starts_with(p))
    {
        let head: String = translated.chars().take(80).collect();
        anyhow::bail!("{head}");
    }
    Ok(unescape(&translated))
}

/// Cheap script check: avoid translating ru->ru or en->ru on English text.
fn looks_like(text: &str, lang: &str) -> bool {
    let has_cyr = text.chars().any(|c| ('\u{0400}'..='\u{04ff}').contains(&c));
    let has_lat = text.chars().any(|c| c.is_ascii_alphabetic());
    if lang == "ru" || lang == "uk" {
        has_cyr && !has_lat
    } else if lang == "en" {
        has_lat && !has_cyr
    } else {
        false
    }
}

/// Translate a Word list caption-group by caption-group.
///
/// Words are grouped the same way the subtitles builder groups them (pause >
/// 0.6s or `per_caption` words). Each group's phrase is translated as a whole
/// (far better than word-by-word) and the translated tokens are spread evenly
/// across the group's time span, so per-word highlighting still works.
pub fn translate_words(
    words: &[Word],
    target: &str,
    source: &str,
    per_caption: Option<usize>,
) -> Vec<Word> {
    let per_caption = per_caption.unwrap_or_else(config::subtitle_words_per_caption);

    let mut groups: Vec<Vec<&Word>> = Vec::new();
    let mut cur: Vec<&Word> = Vec::new();
    for w in words {
        if let Some(last) = cur.last() {
            if w.start - last.end > 0.6 {
                groups.push(std::mem::take(&mut cur));
            }
        }
        cur.push(w);
        if cur.len() >= per_caption {
            groups.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }

    let mut out = Vec::new();
    for grp in groups {
        let phrase = grp
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .trim()
            .to_string();
        let translated = translate_text(&phrase, target, source);
        let tokens: Vec<&str> = translated.split_whitespace().collect();
        if tokens.is_empty() {
            out.extend(grp.iter().map(|w| (*w).clone()));
            continue;
        }
        let g_start = grp[0].start;
        let g_end = grp[grp.len() - 1].end.max(g_start + 0.05);
        let span = g_end - g_start;
        let step = span / tokens.len() as f64;
        let conf = grp.iter().map(|w| w.confidence as f64).sum::<f64>() / grp.len() as f64;
        for (i, tok) in tokens.iter().enumerate() {
            out.push(Word {
                start: g_start + i as f64 * step,
                end: g_start + (i + 1) as f64 * step,
                text: tok.to_string(),
                confidence: conf as f32,
            });
        }
    }
    out
}

/// Translate `text` from `source` to `target` (falls back to input).
pub fn translate_text(text: &str, target: &str, source: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if !SUPPORTED.contains(&target) {
        return text.to_string();
    }
    if target == source || looks_like(text, target) {
        return text.to_string();
    }
    load_cache(source, target);
    let key = (source.to_string(), target.to_string(), text.to_string());
    if let Some(v) = cache().lock().unwrap().get(&key) {
        return v.clone();
    }

    let mut parts = Vec::new();
    for chunk in split(text, 450) {
        let ck = (source.to_string(), target.to_string(), chunk.clone());
        if let Some(v) = cache().lock().unwrap().get(&ck) {
            parts.push(v.clone());
            continue;
        }
        let res = match retry(|| mymemory(&chunk, source, target), 3, 2.0, "translate") {
            Ok(r) => r,
            Err(exc) => {
                log(format!("translate: failed ({exc}); keeping original phrase"));
                return text.to_string();
            }
        };
        cache().lock().unwrap().insert(ck, res.clone());
        parts.push(res);
    }
    let result = parts.join(" ");
    cache().lock().unwrap().insert(key, result.clone());
    save_cache(source, target);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_limit_and_words() {
        let chunks = split("один два три четыре пять шесть", 10);
        assert!(chunks.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(chunks.join(" "), "один два три четыре пять шесть");
    }

    #[test]
    fn looks_like_detects_scripts() {
        assert!(looks_like("привет мир", "ru"));
        assert!(!looks_like("hello world", "ru"));
        assert!(looks_like("hello", "en"));
        assert!(!looks_like("привет", "en"));
    }

    #[test]
    fn unescape_handles_entities() {
        assert_eq!(unescape("a &amp; b &lt;c&gt; &#33; &#x41;"), "a & b <c> ! A");
    }

    #[test]
    fn same_language_returns_input_without_network() {
        assert_eq!(translate_text("привет", "ru", "en"), "привет");
        assert_eq!(translate_text("hello", "en", "ru"), "hello");
    }

    #[test]
    fn translate_words_spreads_tokens() {
        // Use a target/source where looks_like short-circuits (no network):
        // ru target on cyrillic text returns input, tokens preserved per group.
        let words = vec![
            Word { start: 0.0, end: 0.3, text: "привет".into(), confidence: 1.0 },
            Word { start: 0.3, end: 0.6, text: "мир".into(), confidence: 1.0 },
        ];
        let out = translate_words(&words, "ru", "en", None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "привет");
        assert!((out[0].start - 0.0).abs() < 1e-9);
        assert!((out[1].end - 0.6).abs() < 1e-9);
    }
}
