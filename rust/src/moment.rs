//! Post-filter a detected live moment: keep only clips with real speech.
//!
//! A loud audio peak is necessary but not sufficient: it can be a music drop,
//! an instrumental advert, or a one-off scream followed by dead air.  After
//! extracting the clip we transcribe it once (the words are reused for
//! subtitles and the description, so whisper is not run twice) and judge
//! whether there is enough speech — especially *after* the peak — to make a
//! watchable reel.
//!
//! A second axis is transcript *quality*: a loud music/noise peak forces
//! whisper to emit words from noise, producing low-confidence, often
//! multi-language garbage that is not a watchable moment.  We gate on that too,
//! but conservatively (see [`judge`]): confidence alone does not separate good
//! from bad clips, so the quality gate only fires when *both* mean confidence
//! is low *and* a large fraction of words are very low confidence — otherwise
//! it would drop genuine hype reactions.

use regex::Regex;
use std::sync::OnceLock;

use crate::subtitles::Word;

fn cjk_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[\u3000-\u9fff\uac00-\ud7af\uff00-\uffef]")
            .expect("valid CJK regex")
    })
}

#[derive(Debug, Clone)]
pub struct Verdict {
    pub keep: bool,
    pub reason: String,
    // Diagnostic fields kept for parity with the Python Verdict; read in tests.
    #[allow(dead_code)]
    pub speech_ratio: f64,
    #[allow(dead_code)]
    pub words_after: usize,
    #[allow(dead_code)]
    pub words_total: usize,
    #[allow(dead_code)]
    pub speech_after_ratio: f64,
    pub mean_conf: f64,
    pub low_conf_frac: f64,
    pub garbage_frac: f64,
}

impl Verdict {
    #[allow(clippy::too_many_arguments)]
    fn base(keep: bool, reason: String, speech_ratio: f64, words_after: usize,
            words_total: usize, speech_after_ratio: f64,
            mean_conf: f64, low_conf_frac: f64, garbage_frac: f64) -> Self {
        Verdict {
            keep,
            reason,
            speech_ratio,
            words_after,
            words_total,
            speech_after_ratio,
            mean_conf,
            low_conf_frac,
            garbage_frac,
        }
    }
}

/// Return `(mean_confidence, frac_words_below_0.3, frac_non_lexical)`.
///
/// `confidence` is the per-word whisper probability (0..1).
/// `frac_non_lexical` counts words that are CJK/fullwidth or contain no
/// letters — a cheap signal for script-garbage on a Latin/Cyrillic stream.
pub fn transcript_quality(words: &[Word]) -> (f64, f64, f64) {
    let n = words.len();
    if n == 0 {
        return (0.0, 1.0, 1.0);
    }
    let confs: Vec<f64> = words.iter().map(|w| w.confidence as f64).collect();
    let low = confs.iter().filter(|&&c| c < 0.3).count() as f64 / n as f64;
    let cjk = cjk_regex();
    let mut garbage = 0usize;
    for w in words {
        let t = w.text.as_str();
        if cjk.is_match(t) || !t.chars().any(|c| c.is_alphabetic()) {
            garbage += 1;
        }
    }
    let mean = confs.iter().sum::<f64>() / n as f64;
    (mean, low, garbage as f64 / n as f64)
}

/// Deciding whether a clip is worth rendering.
///
/// * `words` — Word objects with times relative to the clip start (0..clip_dur).
/// * `clip_dur` — real clip length in seconds.
/// * `peak_offset` — seconds into the clip where the hype centre sits.
///
/// `min_mean_conf` / `max_low_conf_frac` form the transcript-quality gate: a
/// clip is dropped as "garbled transcript" only when mean confidence is below
/// `min_mean_conf` AND at least `max_low_conf_frac` of words are below 0.3 —
/// i.e. both must be bad.  A single bad condition is tolerated so a genuine
/// loud reaction (short, noisy but real speech) is not dropped.
/// `max_garbage_frac` is a hard drop if over that fraction of words are
/// non-lexical/CJK (clear script garbage), regardless of confidence.
#[allow(clippy::too_many_arguments)]
pub fn judge(
    words: &[Word],
    clip_dur: f64,
    peak_offset: f64,
    min_words_after: usize,
    min_speech_ratio: f64,
    min_speech_after_ratio: f64,
    min_mean_conf: f64,
    max_low_conf_frac: f64,
    max_garbage_frac: f64,
) -> Verdict {
    let clip_dur = clip_dur.max(0.1);
    let peak = peak_offset.clamp(0.0, clip_dur);

    let speech_dur = |ws: &[&Word]| -> f64 {
        ws.iter().map(|w| (w.end - w.start).max(0.0)).sum()
    };

    let total_words = words.len();
    let all_refs: Vec<&Word> = words.iter().collect();
    let total_speech = speech_dur(&all_refs);
    let after: Vec<&Word> = words.iter().filter(|w| w.start >= peak - 0.05).collect();
    let after_words = after.len();
    let after_speech = speech_dur(&after);

    let speech_ratio = (total_speech / clip_dur).min(1.0);
    let after_span = (clip_dur - peak).max(0.1);
    let speech_after_ratio = (after_speech / after_span).min(1.0);

    let (mean_conf, low_conf, garbage) = transcript_quality(words);

    if total_words == 0 {
        return Verdict::base(
            false,
            "no speech at all (music/silence?)".into(),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    if garbage > max_garbage_frac {
        return Verdict::base(
            false,
            format!(
                "garbled transcript (garbage {garbage:.2} > {max_garbage_frac:.2})"
            ),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    if mean_conf < min_mean_conf && low_conf > max_low_conf_frac {
        return Verdict::base(
            false,
            format!(
                "garbled transcript (conf {mean_conf:.2} < {min_mean_conf:.2} and {:.0}% words <0.3)",
                low_conf * 100.0
            ),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    if after_words < min_words_after {
        return Verdict::base(
            false,
            format!("only {after_words} word(s) after peak (need {min_words_after})"),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    if speech_ratio < min_speech_ratio {
        return Verdict::base(
            false,
            format!("speech ratio {speech_ratio:.2} < {min_speech_ratio:.2}"),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    if speech_after_ratio < min_speech_after_ratio {
        return Verdict::base(
            false,
            format!(
                "speech after peak {speech_after_ratio:.2} < {min_speech_after_ratio:.2}"
            ),
            speech_ratio,
            after_words,
            total_words,
            speech_after_ratio,
            mean_conf,
            low_conf,
            garbage,
        );
    }
    Verdict::base(
        true,
        format!("{after_words} words after peak, speech {speech_ratio:.2}"),
        speech_ratio,
        after_words,
        total_words,
        speech_after_ratio,
        mean_conf,
        low_conf,
        garbage,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: f64, end: f64, text: &str, conf: f32) -> Word {
        Word {
            start,
            end,
            text: text.to_string(),
            confidence: conf,
        }
    }

    #[test]
    fn empty_words_is_dropped() {
        let v = judge(&[], 10.0, 5.0, 3, 0.03, 0.0, 0.55, 0.35, 0.5);
        assert!(!v.keep);
        assert!(v.reason.contains("no speech"));
    }

    #[test]
    fn good_speech_is_kept() {
        let words = vec![
            w(1.0, 1.5, "привет", 0.9),
            w(6.0, 6.5, "это", 0.9),
            w(6.6, 7.0, "просто", 0.9),
            w(7.1, 7.5, "офигенно", 0.9),
        ];
        let v = judge(&words, 10.0, 5.0, 3, 0.03, 0.0, 0.55, 0.35, 0.5);
        assert!(v.keep, "reason: {}", v.reason);
        assert_eq!(v.words_after, 3);
    }

    #[test]
    fn too_few_words_after_peak_dropped() {
        let words = vec![w(1.0, 1.5, "привет", 0.9), w(6.0, 6.5, "ну", 0.9)];
        let v = judge(&words, 10.0, 5.0, 3, 0.03, 0.0, 0.55, 0.35, 0.5);
        assert!(!v.keep);
        assert!(v.reason.contains("after peak"));
    }

    #[test]
    fn garbage_script_dropped() {
        let words = vec![
            w(1.0, 1.5, "こんにちは", 0.9),
            w(2.0, 2.5, "世界", 0.9),
            w(6.0, 6.5, "你好", 0.9),
            w(7.0, 7.5, "谢谢", 0.9),
        ];
        let v = judge(&words, 10.0, 5.0, 1, 0.03, 0.0, 0.55, 0.35, 0.5);
        assert!(!v.keep);
        assert!(v.reason.contains("garbage"));
    }

    #[test]
    fn low_conf_plus_many_low_dropped() {
        let words = vec![
            w(1.0, 1.5, "abc", 0.1),
            w(2.0, 2.5, "def", 0.1),
            w(6.0, 6.5, "ghi", 0.1),
            w(7.0, 7.5, "jkl", 0.1),
        ];
        let v = judge(&words, 10.0, 5.0, 1, 0.03, 0.0, 0.55, 0.35, 0.5);
        assert!(!v.keep);
        assert!(v.reason.contains("conf"));
    }

    #[test]
    fn transcript_quality_counts_garbage() {
        let words = vec![w(0.0, 1.0, "привет", 0.8), w(1.0, 2.0, "123", 0.9)];
        let (mean, low, garbage) = transcript_quality(&words);
        assert!((mean - 0.85).abs() < 1e-6);
        assert!(low < 0.01);
        assert!((garbage - 0.5).abs() < 1e-6);
    }
}
