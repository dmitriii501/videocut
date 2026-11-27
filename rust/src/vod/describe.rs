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

/// Matches URLs / bare domains so they never leak into titles or hashtags.
fn url_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:https?://\S+|www\.\S+|\S+\.(?:com|ru|net|org|tv|io|gg|me|be|ly|co)\b)",
        )
        .unwrap()
    })
}

/// URL / platform fragments that make meaningless hashtags when whisper
/// transcribes a spoken link ("точка ком", "твич", "ютуб" and so on).
const JUNK: &[&str] = &[
    "http", "https", "www", "twitch", "youtube", "youtu", "tiktok", "instagram", "telegram",
    "discord", "kick", "clip", "clips", "gmail", "subscribe", "follow",
];

fn junk_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| JUNK.iter().copied().collect())
}

/// Substrings that betray a whisper-hallucinated link glued into one token, e.g.
/// `httpwwwtwitchtvxqcclipRudeHomelyYogurtDatBoi...`. Exact-match `JUNK` misses
/// these because the whole URL becomes a single word with no separators.
const URL_MARKERS: &[&str] = &[
    "http", "www", "twitch", "youtube", "youtu", "tiktok", "instagram", "telegram", "discord",
    "clip", "gmail",
];

/// True for tokens that are really a mashed-up URL / whisper noise rather than a
/// real word: contains a platform/URL marker as a substring, is absurdly long for
/// a word, or carries heavy internal camel-case (hallucinated gibberish).
fn is_gibberish(w: &str) -> bool {
    let len = w.chars().count();
    if len > 24 {
        return true;
    }
    let lower = w.to_lowercase();
    if URL_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    let uppers = w.chars().filter(|c| c.is_uppercase()).count();
    len >= 10 && uppers >= 3
}

/// Greeting / channel-filler words that make a useless title on their own.
const GREETING: &[&str] = &[
    "здравствуйте",
    "здравствуй",
    "привет",
    "приветствую",
    "приветик",
    "хай",
    "хеллоу",
    "hello",
    "hi",
    "hey",
    "добрый",
    "доброе",
    "доброго",
    "день",
    "вечер",
    "утро",
    "всем",
    "ребята",
    "друзья",
    "народ",
    "чат",
    "welcome",
];

fn greeting_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| GREETING.iter().copied().collect())
}

/// Sentence made up only of greetings and stopwords ("Здравствуйте!", "Всем
/// привет") — never a good title or the opening of a description.
fn is_filler(s: &str) -> bool {
    let lower = s.to_lowercase();
    let stop = stop_set();
    let greet = greeting_set();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return true;
    }
    words
        .iter()
        .all(|w| greet.contains(*w) || stop.contains(*w))
}

/// Weak sentence openers (prepositions / conjunctions) that mark a spoken
/// fragment like "в микрофон" rather than a self-contained thought.
const WEAK_OPENERS: &[&str] = &[
    "в", "во", "на", "с", "со", "к", "у", "о", "об", "из", "от", "до", "по", "за", "для",
    "при", "про", "над", "под", "без", "через", "между", "а", "и", "но", "ну", "ах", "ох",
    "эх", "вот",
];

/// A short fragment that starts with a preposition/conjunction and reads as
/// broken speech rather than a complete sentence.
fn is_fragment(s: &str) -> bool {
    if s.chars().count() >= 20 {
        return false;
    }
    let lower = s.to_lowercase();
    let first = lower
        .split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty());
    match first {
        Some(w) => WEAK_OPENERS.contains(&w),
        None => true,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Script {
    Cyrillic,
    Latin,
}

fn script_of(ch: char) -> Option<Script> {
    if ('\u{0400}'..='\u{04FF}').contains(&ch) {
        Some(Script::Cyrillic)
    } else if ch.is_ascii_alphabetic() {
        Some(Script::Latin)
    } else {
        None
    }
}

/// Dominant writing system of the transcript (used to avoid mixing ru+en tags).
fn dominant_script(text: &str) -> Option<Script> {
    let (mut cyr, mut lat) = (0usize, 0usize);
    for ch in text.chars() {
        match script_of(ch) {
            Some(Script::Cyrillic) => cyr += 1,
            Some(Script::Latin) => lat += 1,
            None => {}
        }
    }
    if cyr == 0 && lat == 0 {
        None
    } else if cyr >= lat {
        Some(Script::Cyrillic)
    } else {
        Some(Script::Latin)
    }
}

fn has_script(word: &str, want: Script) -> bool {
    word.chars().any(|c| script_of(c) == Some(want))
}

fn strip_urls(text: &str) -> String {
    url_re().replace_all(text, " ").to_string()
}

/// A candidate is "meaningful" if it has enough letters and is not mostly
/// digits/punctuation (filters whisper number-salad and URL gibberish).
fn meaningful(s: &str) -> bool {
    let letters = s.chars().filter(|c| c.is_alphabetic()).count();
    let total = s.chars().filter(|c| !c.is_whitespace()).count();
    letters >= 6 && total > 0 && letters * 100 / total >= 50
}

/// Trim `cand` to `limit` chars on a word boundary, appending an ellipsis.
fn truncate_ellipsis(cand: &str, limit: usize) -> String {
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
    let junk = junk_set();
    let text = strip_urls(text);
    // Dominant script must ignore hallucinated URL gibberish: one long Latin
    // token ("httpwwwtwitch...") must not flip an otherwise-Cyrillic clip.
    let clean_for_script: String = text
        .split_whitespace()
        .filter(|w| !is_gibberish(w))
        .collect::<Vec<_>>()
        .join(" ");
    let want = dominant_script(&clean_for_script);
    for m in word_re().find_iter(&text.to_lowercase()) {
        let w = m.as_str();
        if stop.contains(w)
            || junk.contains(w)
            || greeting_set().contains(w)
            || is_gibberish(w)
            || seen.iter().any(|s| s == w)
            || !w.chars().any(|c| c.is_alphabetic())
        {
            continue;
        }
        // Keep the dominant language only; skip stray foreign-script tokens.
        if let Some(want) = want {
            if !has_script(w, want) {
                continue;
            }
        }
        seen.push(w.to_string());
        if seen.len() >= limit {
            break;
        }
    }
    seen.into_iter().map(|w| format!("#{w}")).collect()
}

/// First meaningful sentence, trimmed to `limit` chars on a word boundary.
fn short_title(text: &str, limit: usize) -> String {
    let text = strip_urls(text);
    let want = dominant_script(&text);
    let sents = sentences(&text);
    let pick = |min: usize, strict: bool| -> Option<String> {
        sents
            .iter()
            .map(|s| s.trim_matches(|c| matches!(c, ' ' | '.' | ',' | '!' | '…' | '-')))
            .find(|s| {
                s.chars().count() >= min
                    && meaningful(s)
                    && !is_filler(s)
                    && (!strict || !is_fragment(s))
                    && want.map(|w| has_script(s, w)).unwrap_or(true)
            })
            .map(|s| s.to_string())
    };
    // Prefer a substantive sentence (not a greeting/fragment); relax the rules
    // only if nothing qualifies so we still produce *something* usable.
    let cand = pick(12, true)
        .or_else(|| pick(12, false))
        .or_else(|| pick(6, false))
        .unwrap_or_else(|| text.trim().to_string());
    if cand.is_empty() {
        return String::new();
    }
    truncate_ellipsis(&cand, limit)
}

fn heuristic(transcript: &str, channel: &str) -> Description {
    let text = clean(&strip_urls(transcript));
    let mut title = short_title(&text, config::describe_title_max());
    if title.is_empty() {
        title = "Стрим-момент".to_string();
    }
    let desc = if text.is_empty() {
        "Момент со стрима.".to_string()
    } else {
        let raw: String = text.chars().take(config::describe_desc_max()).collect();
        // Drop greeting/filler openers, then cut at the last complete sentence so
        // the description neither starts with "Здравствуйте!" nor ends mid-word.
        let mut sents = sentences(&raw);
        while sents.len() > 1 && (is_filler(&sents[0]) || is_fragment(&sents[0])) {
            sents.remove(0);
        }
        match sents {
            mut s if s.len() > 1 => {
                s.pop();
                s.join(" ")
            }
            _ => raw,
        }
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
    fn hashtags_drop_urls_and_platform_junk() {
        let tags = hashtags("залетаем на стрим https www twitch tv xqc катка фидер разносим", 6);
        assert!(!tags.iter().any(|t| t == "#https" || t == "#www" || t == "#twitch" || t == "#xqc"));
        assert!(tags.iter().any(|t| t == "#катка"), "got {tags:?}");
    }

    #[test]
    fn hashtags_keep_dominant_script() {
        // Cyrillic-dominant text: stray latin words must not become tags.
        let tags = hashtags("сегодня отличная катка на стриме super gameplay", 6);
        assert!(tags.iter().any(|t| t == "#сегодня"));
        assert!(!tags.iter().any(|t| t == "#super" || t == "#gameplay"));
    }

    #[test]
    fn short_title_skips_url_and_number_salad() {
        let t = short_title("https://twitch.tv/xqc 12345 67890 1234567 Это реально смешной момент стрима", 70);
        assert!(!t.contains("http"), "got {t:?}");
        assert!(t.to_lowercase().contains("смешной"), "got {t:?}");
    }

    #[test]
    fn heuristic_strips_urls_from_description() {
        let d = heuristic("Залетаем на https://twitch.tv/foo сегодня разносим лобби в игре!", "streamer");
        assert!(!d.description.contains("http"), "got {:?}", d.description);
        assert!(!d.title.contains("http"), "got {:?}", d.title);
    }

    #[test]
    fn hashtags_drop_glued_url_gibberish() {
        // A real whisper hallucination: the whole URL becomes one token.
        let tags = hashtags(
            "микрофон очень взволнована httpwwwtwitchtvxqcclipRudeHomelyYogurtDatBoiSkoigoy катка разносим",
            8,
        );
        assert!(
            !tags.iter().any(|t| t.to_lowercase().contains("http") || t.to_lowercase().contains("twitch")),
            "got {tags:?}"
        );
        assert!(tags.iter().any(|t| t == "#катка"), "got {tags:?}");
    }

    #[test]
    fn short_title_skips_greeting_opener() {
        let t = short_title("Здравствуйте! Так что я очень взволнована. Приятно, что вы здесь!", 70);
        assert!(!t.to_lowercase().contains("здравствуйте"), "got {t:?}");
        assert!(t.to_lowercase().contains("взволнована") || t.to_lowercase().contains("приятно"), "got {t:?}");
    }

    #[test]
    fn short_title_skips_weak_fragment_opener() {
        let t = short_title("в микрофон. Сегодня разносим лобби в финале турнира", 70);
        assert!(!t.to_lowercase().starts_with("в микрофон"), "got {t:?}");
        assert!(t.to_lowercase().contains("разносим"), "got {t:?}");
    }

    #[test]
    fn description_drops_greeting_opening() {
        let d = heuristic("Здравствуйте! Так что я очень взволнована. Приятно, что вы здесь!", "ch");
        assert!(!d.description.to_lowercase().starts_with("здравствуйте"), "got {:?}", d.description);
    }

    #[test]
    fn p1_real_transcript_regression() {
        // Exact transcript from the user's reported reel.
        let t = "в микрофон. Здравствуйте! Так что я очень взволнована. Приятно, что вы здесь! Ах, конечно, кто?";
        let d = heuristic(t, "xqc");
        assert!(!d.title.to_lowercase().contains("здравствуйте"), "title={:?}", d.title);
        assert!(!d.description.to_lowercase().starts_with("в микрофон"), "desc={:?}", d.description);
        assert!(!d.description.to_lowercase().starts_with("здравствуйте"), "desc={:?}", d.description);
        let tags = d.hashtags.join(" ");
        assert!(!tags.to_lowercase().contains("http"), "tags={:?}", tags);
        assert!(!tags.to_lowercase().contains("twitchtv"), "tags={:?}", tags);
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


