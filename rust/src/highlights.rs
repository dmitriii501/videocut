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
use crate::subtitles::Word;
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

/// Detect up to `count` non-overlapping highlight segments, best first.
pub fn detect_highlights(
    source: &Path,
    count: usize,
    min_dur: f64,
    max_dur: f64,
    words: Option<&[Word]>,
) -> Result<Vec<(f64, f64)>> {
    let total = duration(source);
    if total <= min_dur {
        return Ok(if total > 0.0 { vec![(0.0, total)] } else { vec![] });
    }

    let audio = media::decode_mono(source)?;
    let (times, rms) = energy_windows(&audio);
    let max_rms = rms.iter().cloned().fold(0.0f64, f64::max);
    if rms.is_empty() || max_rms == 0.0 {
        log("highlight: silent audio, falling back to even split");
        return Ok(even_split(total, count, min_dur, max_dur));
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

    // Greedy peak picking with non-overlap suppression.
    let mut order: Vec<usize> = (0..score.len()).collect();
    order.sort_by(|&a, &b| {
        score[b]
            .partial_cmp(&score[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut chosen: Vec<(f64, f64)> = Vec::new();
    let mut used: Vec<(f64, f64)> = Vec::new();
    let overlaps = |a0: f64, a1: f64, used: &[(f64, f64)]| {
        used.iter().any(|&(b0, b1)| !(a1 <= b0 || a0 >= b1))
    };

    for &i in &order {
        if chosen.len() >= count {
            break;
        }
        let peak_t = times[i];
        let lo = i.saturating_sub(6);
        let hi = (i + 6).min(score.len());
        let heat = if hi > lo {
            score[lo..hi].iter().sum::<f64>() / (hi - lo) as f64
        } else {
            0.0
        };
        let mut dur = min_dur + (max_dur - min_dur) * heat.min(1.0);
        let mut start = (peak_t - dur * 0.35).max(0.0).min(total - dur);
        if start + dur > total {
            dur = total - start;
        }
        if dur < min_dur * 0.6 {
            continue;
        }
        if overlaps(start, start + dur, &used) {
            continue;
        }
        used.push((start, start + dur));
        start = (start * 100.0).round() / 100.0;
        dur = (dur * 100.0).round() / 100.0;
        chosen.push((start, dur));
    }

    if chosen.is_empty() {
        return Ok(even_split(total, count, min_dur, max_dur));
    }
    chosen.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    log(format!(
        "highlight: picked {} segment(s) from {} windows",
        chosen.len(),
        times.len()
    ));
    for (s, d) in &chosen {
        log(format!("  {s:7.1}s +{d:4.1}s"));
    }
    Ok(chosen)
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
