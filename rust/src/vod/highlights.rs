//! Automatic highlight detection.
//!
//! Two cheap, CPU-only signals are combined:
//! * **Audio energy** - short bursts of loudness well above the rolling median.
//! * **Speech cues** - if a word-level transcript is available we boost windows
//!   that contain laughter / hype keywords (RU + EN).
//!
//! Output: up to `count` non-overlapping `(start, duration)` segments, best first.

use anyhow::Result;
use regex::Regex;
use std::path::Path;

use crate::media;
use crate::text::subtitles::Word;
use crate::utils::{duration, log};

/// Words that usually surround funny / hype moments (case-insensitive).
fn laugh_regex() -> Regex {
    Regex::new(
        r"(?i)(?:ха+х|аха+х|хе+х|хи+х|хаха|ахах|лол|рофл|ору|ржу|смешн|смех|пиздец|пздц|жесть|красав|ауф|нихуя|охуе|офиге|lol|lmao|haha|hehe|omg|wtf|no way|insane|crazy)",
    )
    .expect("valid laugh regex")
}

const WIN_SEC: f64 = 0.5;
const HOP_SEC: f64 = 0.25;

/// Short-time RMS energy over `audio` (f32 mono @ SAMPLE_RATE), lightly
/// smoothed over ~2 s.
fn energy_windows(audio: &[f32]) -> (Vec<f64>, Vec<f64>) {
    let sr = media::SAMPLE_RATE as f64;
    let w = (WIN_SEC * sr) as usize;
    let h = (HOP_SEC * sr) as usize;
    if audio.len() < w || w == 0 || h == 0 {
        return (vec![0.0], vec![0.0]);
    }
    let n = 1 + (audio.len() - w) / h;
    let mut rms = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * h;
        let frame = &audio[start..start + w];
        let mut acc = 0.0f64;
        for &s in frame {
            acc += (s as f64) * (s as f64);
        }
        rms.push((acc / w as f64).sqrt());
    }

    // Smooth over ~2 s.
    let k = (2.0 / HOP_SEC).round().max(1.0) as usize;
    let mut smoothed = vec![0.0f64; rms.len()];
    let half = k / 2;
    for (i, slot) in smoothed.iter_mut().enumerate() {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(rms.len());
        let sum: f64 = rms[lo..hi].iter().sum();
        *slot = sum / (hi - lo) as f64;
    }

    let times: Vec<f64> = (0..n).map(|i| i as f64 * HOP_SEC).collect();
    (times, smoothed)
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// Linear-interpolated percentile (p in 0..=100).
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (p / 100.0) * (v.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        v[lo]
    } else {
        let frac = rank - lo as f64;
        v[lo] * (1.0 - frac) + v[hi] * frac
    }
}

/// A detected highlight plus a normalised 0..1 "hype" score (how strongly the
/// moment stands out against the rest of the source).
#[derive(Debug, Clone, Copy)]
pub struct ScoredSegment {
    pub start: f64,
    pub dur: f64,
    pub hype: f64,
}

/// Per-window hype signal built from audio energy + speech cues.
struct Signal {
    total: f64,
    /// Window start times (seconds).
    times: Vec<f64>,
    /// Combined score per window.
    score: Vec<f64>,
    /// True when audio was silent / too short to analyse.
    silent: bool,
}

/// Build the combined energy + cue score array over the whole source once.
fn analyze(source: &Path, words: Option<&[Word]>) -> Result<Signal> {
    let total = duration(source);
    let audio = media::decode_mono(source)?;
    Ok(analyze_audio(&audio, total, words))
}

/// Same as [`analyze`] but over an already-decoded mono buffer, so callers that
/// also need the samples (e.g. to transcribe only the chosen windows) decode
/// once instead of twice.
fn analyze_audio(audio: &[f32], total: f64, words: Option<&[Word]>) -> Signal {
    let (times, rms) = energy_windows(audio);
    let max_rms = rms.iter().cloned().fold(0.0f64, f64::max);
    if rms.is_empty() || max_rms == 0.0 {
        return Signal {
            total,
            times,
            score: Vec::new(),
            silent: true,
        };
    }

    let med = median(&rms);
    let hi = percentile(&rms, 95.0).max(med).max(1.0);
    let denom = (hi - med).max(1e-6);
    let energy: Vec<f64> = rms
        .iter()
        .map(|&r| ((r - med) / denom).clamp(0.0, 1.5))
        .collect();

    let mut cue_arr = vec![0.0f64; energy.len()];
    if let Some(words) = words {
        let re = laugh_regex();
        for w in words {
            if re.is_match(&w.text) {
                let i = (w.start / HOP_SEC).round() as isize;
                let lo = (i - 4).max(0) as usize;
                let hi_i = ((i + 4) as usize).min(cue_arr.len());
                if lo < hi_i {
                    for c in &mut cue_arr[lo..hi_i] {
                        *c += 1.0;
                    }
                }
            }
        }
        let cmax = cue_arr.iter().cloned().fold(0.0f64, f64::max);
        if cmax > 0.0 {
            for c in &mut cue_arr {
                *c /= cmax;
            }
        }
    }

    let score: Vec<f64> = energy
        .iter()
        .zip(&cue_arr)
        .map(|(&e, &c)| 0.75 * e + 0.6 * c)
        .collect();

    Signal {
        total,
        times,
        score,
        silent: false,
    }
}

/// Mean combined score over `[start, start+dur]` mapped to 0..1 relative to the
/// source-wide distribution, so values are comparable across jobs.
fn window_hype(sig: &Signal, start: f64, dur: f64) -> f64 {
    if sig.silent || sig.score.is_empty() {
        return 0.5;
    }
    let n = sig.score.len();
    let lo = ((start / HOP_SEC).floor() as usize).min(n - 1);
    let hi = (((start + dur) / HOP_SEC).ceil() as usize).clamp(lo + 1, n);
    let mean = sig.score[lo..hi].iter().sum::<f64>() / (hi - lo) as f64;
    let base = percentile(&sig.score, 20.0);
    let top = percentile(&sig.score, 95.0).max(base + 1e-6);
    ((mean - base) / (top - base)).clamp(0.05, 0.99)
}

/// Detect up to `count` non-overlapping highlights, each with a hype score.
pub fn detect_highlights_scored(
    source: &Path,
    count: usize,
    min_dur: f64,
    max_dur: f64,
    words: Option<&[Word]>,
) -> Result<Vec<ScoredSegment>> {
    let sig = analyze(source, words)?;
    Ok(detect_from_signal(sig, count, min_dur, max_dur))
}

/// Like [`detect_highlights_scored`] but over an already-decoded mono buffer,
/// so the pipeline can decode the audio once and reuse it.
pub fn detect_highlights_from_audio(
    audio: &[f32],
    total: f64,
    count: usize,
    min_dur: f64,
    max_dur: f64,
    words: Option<&[Word]>,
) -> Vec<ScoredSegment> {
    let sig = analyze_audio(audio, total, words);
    detect_from_signal(sig, count, min_dur, max_dur)
}

fn detect_from_signal(
    sig: Signal,
    count: usize,
    min_dur: f64,
    max_dur: f64,
) -> Vec<ScoredSegment> {
    let total = sig.total;
    if total <= min_dur {
        return if total > 0.0 {
            vec![ScoredSegment {
                start: 0.0,
                dur: total,
                hype: 0.5,
            }]
        } else {
            vec![]
        };
    }

    if sig.silent {
        log("highlight: silent audio, falling back to even split");
        return even_split(total, count, min_dur, max_dur)
            .into_iter()
            .map(|(start, dur)| ScoredSegment {
                start,
                dur,
                hype: 0.5,
            })
            .collect();
    }

    let score = &sig.score;
    let times = &sig.times;

    // Windows ranked by descending score.
    let mut order: Vec<usize> = (0..score.len()).collect();
    order.sort_by(|&a, &b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut chosen: Vec<ScoredSegment> = Vec::new();
    let mut used: Vec<(f64, f64)> = Vec::new();

    // Greedy peak picking: best-scoring non-overlapping windows first.
    for &i in &order {
        if chosen.len() >= count {
            break;
        }
        if let Some((start, dur)) = place_window(i, score, times, total, min_dur, max_dur, &used) {
            used.push((start, start + dur));
            let hype = window_hype(&sig, start, dur);
            chosen.push(ScoredSegment { start, dur, hype });
        }
    }

    // Gap-fill: if the greedy pass under-fills (overlap suppression can leave
    // the tail of a VOD unused), pack the leftover time so a long-enough source
    // still yields the requested `count` clips.
    if chosen.len() < count {
        for (start, dur) in fill_gaps(total, count - chosen.len(), min_dur, max_dur, &used) {
            used.push((start, start + dur));
            let hype = window_hype(&sig, start, dur);
            chosen.push(ScoredSegment { start, dur, hype });
        }
    }

    if chosen.is_empty() {
        return even_split(total, count, min_dur, max_dur)
            .into_iter()
            .map(|(start, dur)| ScoredSegment {
                start,
                dur,
                hype: 0.5,
            })
            .collect();
    }
    chosen.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap_or(std::cmp::Ordering::Equal));
    log(format!(
        "highlight: picked {} segment(s) from {} windows",
        chosen.len(),
        times.len()
    ));
    for s in &chosen {
        log(format!("  {:7.1}s +{:4.1}s  hype={:.0}%", s.start, s.dur, s.hype * 100.0));
    }
    chosen
}

/// Turn a peak window index into a rounded `(start, dur)` that fits the source
/// and does not overlap anything in `used`. Returns `None` if it cannot be
/// placed (too short a clip, or it collides with an existing one).
fn place_window(
    i: usize,
    score: &[f64],
    times: &[f64],
    total: f64,
    min_dur: f64,
    max_dur: f64,
    used: &[(f64, f64)],
) -> Option<(f64, f64)> {
    let peak_t = times[i];
    let lo = i.saturating_sub(6);
    let hi = (i + 6).min(score.len());
    let heat = if hi > lo {
        score[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
    } else {
        0.0
    };
    let mut dur = min_dur + (max_dur - min_dur) * heat.min(1.0);
    let start = (peak_t - dur * 0.35).max(0.0).min((total - dur).max(0.0));
    if start + dur > total {
        dur = total - start;
    }
    if dur < min_dur * 0.6 {
        return None;
    }
    if used.iter().any(|&(b0, b1)| !(start + dur <= b0 || start >= b1)) {
        return None;
    }
    Some((
        (start * 100.0).round() / 100.0,
        (dur * 100.0).round() / 100.0,
    ))
}

/// Pack `need` extra non-overlapping clips into the gaps left by `used`,
/// scanning free space from the start of the source. Returns fewer than `need`
/// only when there is genuinely not enough free time.
fn fill_gaps(
    total: f64,
    need: usize,
    min_dur: f64,
    max_dur: f64,
    used: &[(f64, f64)],
) -> Vec<(f64, f64)> {
    if need == 0 {
        return Vec::new();
    }
    // Sort occupied spans and collect the free gaps between them.
    let mut spans: Vec<(f64, f64)> = used.to_vec();
    spans.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut cursor = 0.0f64;
    let mut gaps: Vec<(f64, f64)> = Vec::new(); // (gap_start, gap_end)
    for &(s, e) in &spans {
        if s > cursor {
            gaps.push((cursor, s));
        }
        cursor = cursor.max(e);
    }
    if total > cursor {
        gaps.push((cursor, total));
    }

    let mut out = Vec::new();
    for (g0, g1) in gaps {
        if out.len() >= need {
            break;
        }
        let free = g1 - g0;
        if free < min_dur {
            continue;
        }
        // Spread the remaining clips across this gap, shrinking toward min_dur
        // when space is tight but never exceeding max_dur.
        let want = need - out.len();
        let dur = (free / want as f64).clamp(min_dur, max_dur).min(free);
        let mut pos = g0;
        while pos + dur <= g1 + 1e-6 && out.len() < need {
            out.push((
                (pos * 100.0).round() / 100.0,
                (dur * 100.0).round() / 100.0,
            ));
            pos += dur;
        }
    }
    out
}

/// Score arbitrary (already chosen) segments, e.g. manual `-s` clips, so the
/// UI can show a real hype value instead of a constant.
pub fn score_segments(
    source: &Path,
    segments: &[(f64, f64)],
    words: Option<&[Word]>,
) -> Result<Vec<f64>> {
    if segments.is_empty() {
        return Ok(Vec::new());
    }
    let sig = analyze(source, words)?;
    Ok(segments
        .iter()
        .map(|&(start, dur)| window_hype(&sig, start, dur))
        .collect())
}

/// Score already-chosen segments from a decoded audio buffer (no re-decode).
pub fn score_audio(
    audio: &[f32],
    total: f64,
    segments: &[(f64, f64)],
    words: Option<&[Word]>,
) -> Vec<f64> {
    if segments.is_empty() {
        return Vec::new();
    }
    let sig = analyze_audio(audio, total, words);
    segments
        .iter()
        .map(|&(start, dur)| window_hype(&sig, start, dur))
        .collect()
}

fn even_split(total: f64, count: usize, min_dur: f64, max_dur: f64) -> Vec<(f64, f64)> {
    let count = count.max(1);
    let dur = max_dur.min(min_dur.max(total / count as f64));
    let step = total / count as f64;
    let mut out = Vec::with_capacity(count);
    for k in 0..count {
        let start = k as f64 * step;
        let d = dur.min(total - start);
        out.push((
            (start * 100.0).round() / 100.0,
            (d * 100.0).round() / 100.0,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(score: Vec<f64>) -> Signal {
        let times = (0..score.len()).map(|i| i as f64 * HOP_SEC).collect();
        Signal {
            total: score.len() as f64 * HOP_SEC,
            times,
            score,
            silent: false,
        }
    }

    #[test]
    fn window_hype_is_bounded() {
        let s = sig((0..40).map(|i| i as f64 / 40.0).collect());
        for &(start, dur) in &[(0.0, 2.0), (5.0, 3.0), (8.0, 1.0)] {
            let h = window_hype(&s, start, dur);
            assert!((0.05..=0.99).contains(&h), "hype {h} out of range");
        }
    }

    #[test]
    fn window_hype_ranks_hot_above_cold() {
        // Quiet baseline with one loud burst in the middle.
        let mut score = vec![0.1; 40];
        for v in &mut score[18..24] {
            *v = 1.5;
        }
        let s = sig(score);
        let cold = window_hype(&s, 0.0, 2.0);
        let hot = window_hype(&s, 4.6, 1.5);
        assert!(hot > cold, "hot {hot} should beat cold {cold}");
    }

    #[test]
    fn fill_gaps_reaches_requested_count() {
        // 120s source, one 30s clip already placed mid-way, need 2 more.
        let used = vec![(45.0, 30.0)];
        let got = fill_gaps(120.0, 2, 15.0, 45.0, &used);
        assert_eq!(got.len(), 2, "got {got:?}");
        // Nothing overlaps the occupied span or exceeds the source.
        for &(s, d) in &got {
            assert!(s + d <= 120.0 + 1e-6);
            assert!(s + d <= 45.0 + 1e-6 || s >= 75.0 - 1e-6, "overlaps used: {s}+{d}");
        }
    }

    #[test]
    fn fill_gaps_respects_available_space() {
        // Only ~20s free at the tail: cannot fit two 15s clips.
        let used = vec![(0.0, 80.0)];
        let got = fill_gaps(100.0, 2, 15.0, 45.0, &used);
        assert_eq!(got.len(), 1, "got {got:?}");
    }

    #[test]
    fn silent_signal_scores_neutral() {
        let s = Signal {
            total: 10.0,
            times: vec![],
            score: vec![],
            silent: true,
        };
        assert_eq!(window_hype(&s, 0.0, 5.0), 0.5);
    }
}
