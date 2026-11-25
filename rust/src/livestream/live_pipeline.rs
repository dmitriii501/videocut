//! Live orchestration: watch a Twitch stream, score hype, cut reels.
//!
//! Flow:
//!   1. Start a [`RingBuffer`] recording the live stream into short segments.
//!   2. Start a [`ChatMonitor`] on the channel's IRC.
//!   3. Every tick each signal reports a continuous *evidence* ratio to its own
//!      trigger level (1.0 == exactly at the trigger).  They are fused into one
//!      score `audio_weight*ev_audio + chat_weight*ev_chat` and a reel is cut
//!      when the score crosses `score_threshold`.
//!   4. The clip window is `[center - pre_roll, center + post_roll]`; the audio
//!      peak supplies a content-accurate centre, chat falls back to the current
//!      content time.
//!   5. Extract that window from the ring buffer and render it as a reel via the
//!      normal pipeline (transcription + subtitles + webcam layout).
//!   6. Repeat until stopped or `max_reels` is reached.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use crate::livestream::chat::ChatMonitor;
use crate::config;
use crate::livestream::live::{self, RingBuffer};
use crate::text::moment;
use crate::vod::pipeline::{run_job, ReelJob};
use crate::livestream::review::ReviewLog;
use crate::text::subtitles;
use crate::utils::{ensure_dir, log, slugify};

#[derive(Clone)]
pub struct LiveJob {
    pub url: String,
    pub out_dir: PathBuf,
    pub pre_roll: f64,
    pub post_roll: f64,
    pub buffer_secs: f64,
    pub segment_secs: f64,
    pub chat_threshold: f64,
    pub min_rate: f64,
    pub cooldown: f64,
    pub max_reels: usize,
    pub use_audio: bool,
    pub audio_adaptive: bool,
    pub audio_k: f64,
    pub audio_ratio: f64,
    pub audio_floor: f64,
    pub audio_baseline_secs: f64,
    pub audio_weight: f64,
    pub chat_weight: f64,
    pub score_threshold: f64,
    pub moment_filter: bool,
    pub min_words_after: usize,
    pub min_speech_ratio: f64,
    pub min_mean_conf: f64,
    pub max_low_conf_frac: f64,
    pub no_subs: bool,
    pub subtitle_lang: Option<String>,
    /// Inject the animated mid-roll promo banner into each reel.
    pub banner: bool,
    pub webcam_spec: Option<String>,
    pub send_telegram: bool,
    pub review_dir: Option<PathBuf>,
    pub should_stop: Option<Arc<AtomicBool>>,
    /// Called with each finished reel path (e.g. to stream it to Telegram).
    pub on_reel: Option<OnReel>,
}

/// Callback invoked with each finished reel path (e.g. to stream it to Telegram).
pub type OnReel = Arc<dyn Fn(&std::path::Path) + Send + Sync>;

impl std::fmt::Debug for LiveJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveJob")
            .field("url", &self.url)
            .field("out_dir", &self.out_dir)
            .field("max_reels", &self.max_reels)
            .field("on_reel", &self.on_reel.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for LiveJob {
    fn default() -> Self {
        LiveJob {
            url: String::new(),
            out_dir: config::output(),
            pre_roll: 12.0,
            post_roll: 28.0,
            buffer_secs: 120.0,
            segment_secs: 4.0,
            chat_threshold: 2.5,
            min_rate: 1.5,
            cooldown: 30.0,
            max_reels: 0,
            use_audio: true,
            audio_adaptive: true,
            audio_k: 3.0,
            audio_ratio: 1.6,
            audio_floor: 0.012,
            audio_baseline_secs: 60.0,
            audio_weight: 1.0,
            chat_weight: 0.4,
            score_threshold: 0.7,
            moment_filter: true,
            min_words_after: 3,
            min_speech_ratio: 0.03,
            min_mean_conf: 0.55,
            max_low_conf_frac: 0.35,
            no_subs: false,
            subtitle_lang: None,
            banner: config::banner_enabled(),
            webcam_spec: None,
            send_telegram: false,
            review_dir: None,
            should_stop: None,
            on_reel: None,
        }
    }
}

fn should_stop(job: &LiveJob) -> bool {
    job.should_stop
        .as_ref()
        .map(|f| f.load(Ordering::SeqCst))
        .unwrap_or(false)
}

fn now_stamp() -> String {
    // YYYYMMDD-HHMMSS from the system clock via `date`-free formatting.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

/// Format epoch seconds as `YYYYMMDD-HHMMSS` (UTC, good enough for dir names).
fn format_utc(epoch: u64) -> String {
    let days = epoch / 86400;
    let rem = epoch % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mo <= 2 { y + 1 } else { y };
    format!("{year:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

/// Run the live loop, returning the produced reel paths.
pub fn run_live(job: &LiveJob) -> Result<Vec<PathBuf>> {
    ensure_dir(&job.out_dir)?;
    let channel = live::channel_from_url(&job.url);
    // Unique per-run workdir.
    let run_id = format!("{}-{}", now_stamp(), std::process::id());
    let work = ensure_dir(&config::cache().join("live").join(slugify(&channel, 60)).join(run_id))?;

    let mut buf = RingBuffer::new(
        &job.url,
        &work.join("ring"),
        job.segment_secs,
        job.buffer_secs,
    )?;
    let mut mon = ChatMonitor::new(
        &channel,
        job.chat_threshold,
        job.min_rate,
        5.0,
        60.0,
    );

    let review_log = match &job.review_dir {
        Some(dir) => {
            let rl = ReviewLog::new(dir)?;
            log(format!(
                "live: review mode -> {} (label hit/miss in index.csv)",
                rl.location()
            ));
            Some(rl)
        }
        None => None,
    };

    let mut produced: Vec<PathBuf> = Vec::new();
    let result = (|| -> Result<()> {
        buf.start()?;
        mon.start();
        let warmup = job.buffer_secs.max(30.0);
        log(format!("live: warming up {warmup:.0}s (buffer + chat baseline)..."));
        sleep(Duration::from_secs_f64(warmup));

        let mut idx = 0usize;
        let mut last_center = -1e9f64;
        log("live: watching audio + chat for hype (until the stream ends)...");
        'outer: while !(job.max_reels > 0 && produced.len() >= job.max_reels) {
            if should_stop(job) {
                log("live: stop requested; finishing");
                break;
            }
            struct Hit {
                center: f64,
                reason: String,
                a_stats: (f64, f64, f64),
                a_ev: f64,
                c_ev: f64,
                c_rate: f64,
                c_base: f64,
                score: f64,
            }
            #[allow(unused_assignments)]
            let mut hit: Option<Hit> = None;

            loop {
                if should_stop(job) {
                    log("live: stop requested; exiting inner loop");
                    break 'outer;
                }
                if !buf.is_alive() {
                    log("live: recorder stopped (stream ended); finishing");
                    break 'outer;
                }
                let (a_center, a_ev, a_stats_now) = if job.use_audio {
                    buf.audio_evidence(
                        6.0,
                        job.audio_baseline_secs,
                        job.audio_ratio,
                        job.audio_floor,
                        job.audio_adaptive,
                        job.audio_k,
                    )
                } else {
                    (None, 0.0, (0.0, 0.0, 0.0))
                };
                let (c_ev, c_rate, c_base) = mon.evidence();
                let score = job.audio_weight * a_ev + job.chat_weight * c_ev;
                if score >= job.score_threshold {
                    let cand = match a_center {
                        Some(c) if a_ev >= c_ev => c,
                        _ => buf.content_time(),
                    };
                    if cand - last_center >= job.cooldown {
                        let (peak, med, sigma) = a_stats_now;
                        let mut parts: Vec<String> = Vec::new();
                        if job.use_audio && a_ev > 0.0 {
                            if job.audio_adaptive {
                                let thr = med + job.audio_k * sigma;
                                parts.push(format!(
                                    "audio {a_ev:.2} (e={peak:.3} vs base {med:.3}+{:.1}*{sigma:.3} thr {thr:.3})",
                                    job.audio_k
                                ));
                            } else {
                                parts.push(format!(
                                    "audio {a_ev:.2} (e={peak:.3} vs base {med:.3})"
                                ));
                            }
                        }
                        if c_ev > 0.0 {
                            parts.push(format!(
                                "chat {c_ev:.2} ({c_rate:.1}/s vs {c_base:.1}/s)"
                            ));
                        }
                        hit = Some(Hit {
                            center: cand,
                            reason: format!("score {score:.2} = {}", parts.join(" + ")),
                            a_stats: a_stats_now,
                            a_ev,
                            c_ev,
                            c_rate,
                            c_base,
                            score,
                        });
                        break;
                    }
                }
                sleep(Duration::from_millis(500));
            }

            let Hit {
                center,
                reason,
                a_stats,
                a_ev: a_ev_final,
                c_ev: c_ev_final,
                c_rate: c_rate_final,
                c_base: c_base_final,
                score: score_final,
            } = match hit {
                Some(h) => h,
                None => break,
            };
            last_center = center;
            idx += 1;
            log(format!("live: HYPE #{idx} ({reason}; content {center:.1}s)"));

            let c0 = (center - job.pre_roll).max(0.0);
            let c1 = center + job.post_roll;
            let clip = work.join(format!("clip_{idx:03}.mp4"));
            let out_clip = match buf.extract_clip(c0, c1, &clip) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    log("live: buffer did not cover the clip window; skipping");
                    continue;
                }
                Err(exc) => {
                    log(format!("live: clip extraction failed: {exc}"));
                    continue;
                }
            };

            let clip_dur = (c1 - c0).max(1.0);
            let mut clip_words: Option<Vec<subtitles::Word>> = None;
            if job.moment_filter || !job.no_subs {
                match subtitles::transcribe(&out_clip, config::whisper_lang().as_deref()) {
                    Ok(w) => clip_words = Some(w),
                    Err(exc) => {
                        log(format!("live: clip transcription failed: {exc}"));
                        clip_words = None;
                    }
                }
            }
            let verdict = if job.moment_filter {
                Some(moment::judge(
                    clip_words.as_deref().unwrap_or(&[]),
                    clip_dur,
                    center - c0,
                    job.min_words_after,
                    job.min_speech_ratio,
                    0.0,
                    job.min_mean_conf,
                    job.max_low_conf_frac,
                    0.5,
                ))
            } else {
                None
            };
            if let Some(v) = &verdict {
                if !v.keep {
                    log(format!("live: skip HYPE #{idx}: {}", v.reason));
                }
            }

            if let Some(rl) = &review_log {
                let (peak, med, sigma) = a_stats;
                let mut row: HashMap<String, String> = HashMap::new();
                row.insert("idx".into(), idx.to_string());
                row.insert("time".into(), now_stamp());
                row.insert("channel".into(), channel.clone());
                row.insert("content_center".into(), format!("{center:.1}"));
                row.insert("score".into(), format!("{score_final:.2}"));
                row.insert("audio_ev".into(), format!("{a_ev_final:.2}"));
                row.insert("chat_ev".into(), format!("{c_ev_final:.2}"));
                row.insert("audio_peak".into(), format!("{peak:.4}"));
                row.insert("audio_base".into(), format!("{med:.4}"));
                row.insert("audio_sigma".into(), format!("{sigma:.4}"));
                row.insert("chat_rate".into(), format!("{c_rate_final:.2}"));
                row.insert("chat_base".into(), format!("{c_base_final:.2}"));
                match &verdict {
                    None => {
                        row.insert("filter_keep".into(), String::new());
                        row.insert("filter_reason".into(), String::new());
                        row.insert("mean_conf".into(), String::new());
                        row.insert("low_conf_frac".into(), String::new());
                        row.insert("garbage_frac".into(), String::new());
                    }
                    Some(v) => {
                        row.insert("filter_keep".into(), if v.keep { "1" } else { "0" }.into());
                        row.insert("filter_reason".into(), v.reason.clone());
                        row.insert("mean_conf".into(), format!("{:.2}", v.mean_conf));
                        row.insert("low_conf_frac".into(), format!("{:.2}", v.low_conf_frac));
                        row.insert("garbage_frac".into(), format!("{:.2}", v.garbage_frac));
                    }
                }
                rl.record(&out_clip, &row);
            }

            if let Some(v) = &verdict {
                if !v.keep {
                    let _ = std::fs::remove_file(&out_clip);
                    continue;
                }
                log(format!("live: moment ok ({})", v.reason));
            }

            let reel_out = job.out_dir.clone();
            let mut reel_job = ReelJob {
                source_url: format!("live:{channel}"),
                segments: vec![(0.0, clip_dur)],
                out_dir: reel_out.clone(),
                no_subs: job.no_subs,
                local_source: Some(out_clip.clone()),
                webcam_spec: job.webcam_spec.clone(),
                channel: channel.clone(),
                send_telegram: job.send_telegram,
                words: clip_words,
                subtitle_lang: job.subtitle_lang.clone(),
                banner: job.banner,
                ..Default::default()
            };
            reel_job.auto_count = 0;

            let outs = match run_job(&reel_job) {
                Ok(o) => o,
                Err(exc) => {
                    log(format!("live: reel render failed: {exc}"));
                    continue;
                }
            };
            for o in outs {
                let final_path = reel_out.join(format!("{}-live-{idx:03}.mp4", slugify(&channel, 60)));
                let final_path = match std::fs::rename(&o, &final_path) {
                    Ok(()) => final_path,
                    Err(_) => o.clone(),
                };
                // Move the sidecar alongside the reel.
                let sidecar = o.with_extension("txt");
                let final_sidecar = final_path.with_extension("txt");
                if sidecar.exists() && sidecar != final_sidecar {
                    let _ = std::fs::rename(&sidecar, &final_sidecar);
                }
                produced.push(final_path.clone());
                log(format!("live: produced {}", final_path.display()));
                if let Some(cb) = &job.on_reel {
                    cb(&final_path);
                }
            }
            let _ = std::fs::remove_file(&out_clip);
        }
        Ok(())
    })();

    mon.stop();
    buf.stop();
    result?;
    Ok(produced)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_utc_epoch_zero() {
        assert_eq!(format_utc(0), "19700101-000000");
    }

    #[test]
    fn format_utc_known_date() {
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(format_utc(1609459200), "20210101-000000");
    }

}
