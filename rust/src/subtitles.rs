//! Transcription (whisper-rs / whisper.cpp) + ASS subtitle generation with
//! per-word highlight.
//!
//! The model file (ggml) is resolved from `STREAMREELS_WHISPER_MODEL_PATH`, or
//! looked up under `cache/models/<WHISPER_MODEL>.bin`.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::config;
use crate::media;
use crate::utils::log;

#[derive(Debug, Clone)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub confidence: f32,
}

// Phantom phrases Whisper hallucinates on silence / laughter / music. Matched
// case-insensitively against the whole segment; such segments are dropped.
const HALLUCINATIONS: &[&str] = &[
    "субтитры сделал",
    "субтитры делал",
    "субтитры создал",
    "субтитры подготовил",
    "корректор",
    "редактор субтитров",
    "продолжение следует",
    "спасибо за просмотр",
    "подписывайтесь",
    "dimatorzok",
    "dima torzok",
    "amara.org",
    "thanks for watching",
    "subscribe",
    "субтитры от",
];

fn is_hallucination(text: &str) -> bool {
    let low = text.to_lowercase();
    HALLUCINATIONS.iter().any(|h| low.contains(h))
}

/// Whisper.cpp renders special tokens (timestamp markers, language tags, SOT/
/// EOT, etc.) as bracketed `[_...]` strings, e.g. `[_TT_285]`, `[_SOT_]`,
/// `[_LANG_en]`. They must never reach subtitle or description text.
fn is_special_token(text: &str) -> bool {
    let t = text.trim();
    t.starts_with("[_") && t.ends_with(']')
}

/// Remove any embedded `[_...]` special-token markers from a word buffer.
fn strip_special(text: &str) -> String {
    if !text.contains("[_") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' && i + 1 < bytes.len() && bytes[i + 1] == b'_' {
            if let Some(rel) = text[i..].find(']') {
                i += rel + 1;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Resolve the ggml model path.
pub fn model_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("STREAMREELS_WHISPER_MODEL_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        bail!("STREAMREELS_WHISPER_MODEL_PATH does not exist: {}", p.display());
    }
    let models = config::cache().join("models");
    let name = config::whisper_model();
    for cand in [
        models.join(format!("{name}.bin")),
        models.join(format!("ggml-{name}.bin")),
    ] {
        if cand.exists() {
            return Ok(cand);
        }
    }
    bail!(
        "whisper model not found. Set STREAMREELS_WHISPER_MODEL_PATH or place \
         ggml-{name}.bin in {}",
        models.display()
    )
}

/// Word-level transcription of `media_path` (absolute times in seconds).
pub fn transcribe(media_path: &Path, language: Option<&str>) -> Result<Vec<Word>> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    let audio = media::decode_mono(media_path)
        .with_context(|| format!("decode audio {}", media_path.display()))?;
    let model = model_path()?;
    log(format!(
        "loading whisper model {} ({} samples @ {} Hz)",
        model.display(),
        audio.len(),
        media::SAMPLE_RATE
    ));

    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
        .map_err(|e| anyhow!("load whisper model: {e}"))?;
    let mut state = ctx.create_state().map_err(|e| anyhow!("whisper state: {e}"))?;

    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_n_threads(config::whisper_threads());
    let lang = language.map(str::to_string).or_else(config::whisper_lang);
    params.set_language(lang.as_deref());
    params.set_translate(false);
    // Disable cross-segment conditioning: it makes Whisper loop/repeat and
    // invent text on long silence or laughter.
    params.set_no_context(true);
    params.set_token_timestamps(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);

    state
        .full(params, &audio)
        .map_err(|e| anyhow!("whisper run: {e}"))?;

    let n_segments = state.full_n_segments();
    let mut words: Vec<Word> = Vec::new();
    let mut dropped = 0;

    for seg in state.as_iter() {
        let seg_words = segment_to_words(&seg);
        if seg_words.is_empty() {
            continue;
        }
        let seg_text = seg_words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if is_hallucination(&seg_text) {
            dropped += 1;
            continue;
        }
        words.extend(seg_words);
    }

    if dropped > 0 {
        log(format!("dropped {dropped} hallucinated segment(s)"));
    }
    log(format!("transcribed {} words ({n_segments} segments)", words.len()));
    Ok(words)
}

/// whisper.cpp token timestamps are in 10 ms units (centiseconds).
const TS_UNIT: f64 = 100.0;

fn segment_to_words(seg: &whisper_rs::WhisperSegment) -> Vec<Word> {
    let n = seg.n_tokens();
    let mut words: Vec<Word> = Vec::new();
    // Accumulator for the current word being built from tokens.
    let mut cur: Option<(f64, f64, String, f32, u32)> = None; // start,end,text,prob_sum,count

    let flush = |cur: &mut Option<(f64, f64, String, f32, u32)>, words: &mut Vec<Word>| {
        if let Some((start, end, text, psum, cnt)) = cur.take() {
            let text = strip_special(&text);
            let text = text.trim().to_string();
            if !text.is_empty() {
                words.push(Word {
                    start,
                    end,
                    text,
                    confidence: if cnt > 0 { psum / cnt as f32 } else { 1.0 },
                });
            }
        }
    };

    for i in 0..n {
        let token = match seg.get_token(i) {
            Some(t) => t,
            None => continue,
        };
        let text = token.to_str_lossy().map(|c| c.into_owned()).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        // Special tokens (timestamps, language/SOT/EOT markers) are not spoken
        // text: treat them as word boundaries and drop them.
        if is_special_token(&text) {
            flush(&mut cur, &mut words);
            continue;
        }
        let data = token.token_data();
        let t0 = data.t0 as f64 / TS_UNIT;
        let t1 = data.t1 as f64 / TS_UNIT;
        let prob = token.token_probability();

        let starts_word = text.starts_with(' ');
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }

        if starts_word {
            flush(&mut cur, &mut words);
            cur = Some((t0, t1, trimmed.to_string(), prob, 1));
        } else {
            match cur.as_mut() {
                Some((_, end, buf, psum, cnt)) => {
                    buf.push_str(trimmed);
                    *end = t1;
                    *psum += prob;
                    *cnt += 1;
                }
                None => {
                    cur = Some((t0, t1, trimmed.to_string(), prob, 1));
                }
            }
        }
    }
    flush(&mut cur, &mut words);
    words
}

// ----------------------------- ASS rendering -----------------------------

fn ass_time(t: f64) -> String {
    let t = t.max(0.0);
    let h = (t / 3600.0) as u64;
    let m = ((t % 3600.0) / 60.0) as u64;
    let s = t % 60.0;
    format!("{h}:{m:02}:{s:05.2}")
}

fn ass_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('{', "(")
        .replace('}', ")")
}

fn group_words(words: &[Word], per_caption: usize) -> Vec<Vec<Word>> {
    let mut groups: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    for w in words {
        if let Some(last) = cur.last() {
            if w.start - last.end > 0.6 {
                groups.push(std::mem::take(&mut cur));
            }
        }
        cur.push(w.clone());
        if cur.len() >= per_caption {
            groups.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}

#[allow(clippy::too_many_arguments)]
fn style_header(width: u32, height: u32, font: &str, size: u32, margin_v: i32) -> String {
    format!(
        "[Script Info]\n\
         ScriptType: v4.00+\n\
         PlayResX: {width}\n\
         PlayResY: {height}\n\
         WrapStyle: 2\n\
         ScaledBorderAndShadow: yes\n\n\
         [V4+ Styles]\n\
         Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n\
         Style: Default,{font},{size},&H00FFFFFF,&H0000FFFF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,3,1,2,40,40,{margin_v},1\n\n\
         [Events]\n\
         Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n"
    )
}

/// Write an ASS file where each caption is centered inside the top (stream)
/// region. Times are shifted by `-offset` (source -> clip-local). Words outside
/// the clip are dropped; the active word is highlighted.
#[allow(clippy::too_many_arguments)]
pub fn build_ass(
    words: &[Word],
    out_path: &Path,
    offset: f64,
    clip_dur: f64,
    region_top: i32,
    region_height: i32,
    width: u32,
    height: u32,
) -> Result<PathBuf> {
    let font_size = (region_height as f64 * 0.075).max(20.0) as u32;
    let margin_v = (height as f64 * 0.02) as i32
        + (height as i32 - region_top - region_height).max(0)
        + (region_height as f64 * 0.06) as i32;
    let header = style_header(width, height, &config::subtitle_font(), font_size, margin_v);

    let groups = group_words(words, config::subtitle_words_per_caption());
    let hl = r"{\c&H00FFFF&}"; // yellow (BGR)
    let reset = r"{\c&HFFFFFF&}"; // white

    let mut lines: Vec<String> = Vec::new();
    for grp in &groups {
        let grp: Vec<&Word> = grp
            .iter()
            .filter(|w| offset <= w.start && w.start < offset + clip_dur)
            .collect();
        if grp.is_empty() {
            continue;
        }
        for (i, w) in grp.iter().enumerate() {
            let seg_start = (w.start - offset).max(0.0);
            let mut seg_end = (w.end - offset).min(clip_dur);
            if seg_end <= seg_start {
                seg_end = (seg_start + 0.12).min(clip_dur);
            }
            let parts: Vec<String> = grp
                .iter()
                .enumerate()
                .map(|(j, other)| {
                    let txt = ass_escape(&other.text);
                    if j == i {
                        format!("{hl}{txt}{reset}")
                    } else {
                        txt
                    }
                })
                .collect();
            let body = parts.join(" ");
            lines.push(format!(
                "Dialogue: 0,{},{},Default,,0,0,0,,{}",
                ass_time(seg_start),
                ass_time(seg_end),
                body
            ));
        }
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut content = header;
    content.push_str(&lines.join("\n"));
    content.push('\n');
    std::fs::write(out_path, content)
        .with_context(|| format!("write ASS {}", out_path.display()))?;
    log(format!(
        "wrote {} subtitle events -> {}",
        lines.len(),
        out_path.display()
    ));
    Ok(out_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_special_tokens() {
        assert!(is_special_token("[_TT_285]"));
        assert!(is_special_token(" [_SOT_] "));
        assert!(is_special_token("[_LANG_en]"));
        assert!(!is_special_token("hello"));
        assert!(!is_special_token("[bracketed]"));
    }

    #[test]
    fn strips_embedded_special_tokens() {
        assert_eq!(strip_special("dog.[_TT_460]"), "dog.");
        assert_eq!(
            strip_special("correctly.[_TT_285] The quick"),
            "correctly. The quick"
        );
        assert_eq!(strip_special("plain text"), "plain text");
        assert_eq!(strip_special("[_SOT_][_EOT_]"), "");
    }
}
