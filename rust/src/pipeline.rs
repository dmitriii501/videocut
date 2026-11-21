//! End-to-end pipeline orchestration: source -> segments -> render -> sidecar.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::config;
use crate::describe;
use crate::fetch;
use crate::highlights;
use crate::render;
use crate::subtitles::{self, Word};
use crate::telegram;
use crate::utils::{duration, ensure_dir, fmt_ts, log, short_hash, slugify};
use crate::webcam;

/// Callback invoked with human-readable progress notes as a job advances.
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// A single reel-production job.
#[derive(Clone)]
pub struct ReelJob {
    pub source_url: String,
    pub segments: Vec<(f64, f64)>,
    pub out_dir: PathBuf,
    pub no_subs: bool,
    pub local_source: Option<PathBuf>,
    /// >0 => ignore `segments`, auto-detect this many highlights.
    pub auto_count: usize,
    /// Manual webcam panel `x0,y0,x1,y1` override.
    pub webcam_spec: Option<String>,
    /// Streamer/channel name, used for descriptions.
    pub channel: String,
    /// Upload each reel to Telegram on completion.
    pub send_telegram: bool,
    /// Precomputed transcript (clip-relative), skips whisper.
    pub words: Option<Vec<Word>>,
    /// Translate subtitles to this language (e.g. "ru").
    pub subtitle_lang: Option<String>,
    /// Optional progress callback (e.g. to update a Telegram status message).
    pub progress: Option<Progress>,
}

impl std::fmt::Debug for ReelJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReelJob")
            .field("source_url", &self.source_url)
            .field("segments", &self.segments)
            .field("out_dir", &self.out_dir)
            .field("auto_count", &self.auto_count)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

impl ReelJob {
    fn note(&self, msg: &str) {
        if let Some(cb) = &self.progress {
            cb(msg);
        }
    }
}

impl Default for ReelJob {
    fn default() -> Self {
        ReelJob {
            source_url: String::new(),
            segments: Vec::new(),
            out_dir: config::output(),
            no_subs: false,
            local_source: None,
            auto_count: 0,
            webcam_spec: None,
            channel: String::new(),
            send_telegram: false,
            words: None,
            subtitle_lang: None,
            progress: None,
        }
    }
}

fn workdir(url: &str) -> Result<PathBuf> {
    ensure_dir(&config::cache().join("work").join(short_hash(url)))
}

/// Run the job, returning the produced reel paths.
pub fn run_job(job: &ReelJob) -> Result<Vec<PathBuf>> {
    ensure_dir(&job.out_dir)?;

    let source: PathBuf = if let Some(local) = &job.local_source {
        if !local.exists() {
            bail!("local source not found: {}", local.display());
        }
        log(format!("using local source: {}", local.display()));
        job.note("📥 Открываю локальный файл…");
        local.clone()
    } else {
        job.note("📥 Скачиваю видео…");
        fetch::fetch_source(&job.source_url, &workdir(&job.source_url)?)?
    };

    let total = duration(&source);
    log(format!("source duration: {}", fmt_ts(total)));

    let stem = slugify(
        source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("reel"),
        60,
    );
    let lay = render::layout();
    let mut produced: Vec<PathBuf> = Vec::new();

    // Webcam panel is resolved per segment: the overlay can move over a long
    // stream, but is static within a clip.
    let manual_panel = match &job.webcam_spec {
        Some(spec) => {
            let p = webcam::parse_manual(spec)?;
            log(format!("webcam panel (manual): {:?}", p.as_tuple()));
            Some(p)
        }
        None => None,
    };

    // Full-source word transcript (absolute times).  In auto mode it drives
    // highlight detection and is reused for every segment's subtitles; in
    // manual mode we still transcribe the whole source once so segments can be
    // sliced.
    let all_words: Option<Vec<Word>> = if let Some(words) = &job.words {
        Some(words.clone())
    } else if !job.no_subs || job.auto_count > 0 {
        job.note("🎧 Распознаю речь (whisper)…");
        Some(
            subtitles::transcribe(&source, config::whisper_lang().as_deref())
                .context("transcribe source")?,
        )
    } else {
        None
    };

    let segments: Vec<(f64, f64)> = if job.auto_count > 0 {
        job.note("🔍 Ищу лучшие моменты…");
        let segs = highlights::detect_highlights(
            &source,
            job.auto_count,
            15.0,
            45.0,
            all_words.as_deref(),
        )?;
        if segs.is_empty() {
            bail!("auto-detect found no highlights; use -s instead");
        }
        log(format!("auto mode: {} highlight(s)", segs.len()));
        segs
    } else {
        job.segments.clone()
    };

    let total_segments = segments.len();
    for (idx, &(start, mut dur)) in segments.iter().enumerate() {
        let idx = idx + 1;
        if start >= total {
            log(format!("segment {idx} starts after source end, skipped"));
            continue;
        }
        dur = dur.min(total - start);
        log(format!(
            "--- segment {idx}/{total_segments}: {} +{dur:.1}s",
            fmt_ts(start)
        ));
        job.note(&format!("✂️ Ролик {idx}/{total_segments}: монтаж…"));

        let work = workdir(&job.source_url)?;
        let ass_path = work.join(format!("seg{idx}.ass"));
        let mut ass_for_render: Option<PathBuf> = None;
        let seg_words = slice_words(all_words.as_deref().unwrap_or(&[]), start, dur);
        let mut sub_words_for_desc: Vec<Word> = seg_words.clone();

        if !job.no_subs {
            if !seg_words.is_empty() {
                let sub_words = if let Some(target) = &job.subtitle_lang {
                    let src = config::whisper_lang().unwrap_or_else(|| "en".to_string());
                    job.note(&format!("🌐 Перевожу субтитры {src}→{target}…"));
                    let translated =
                        crate::translate::translate_words(&seg_words, target, &src, None);
                    log(format!("translated subtitles {src}->{target}"));
                    translated
                } else {
                    seg_words.clone()
                };
                sub_words_for_desc = sub_words.clone();
                subtitles::build_ass(
                    &sub_words,
                    &ass_path,
                    start,
                    dur,
                    0,
                    lay.top_h as i32,
                    lay.width,
                    lay.height,
                )?;
                ass_for_render = Some(ass_path);
            } else {
                log("no words transcribed; rendering without subtitles");
            }
        }

        let name = format!("{stem}-{idx:02}.mp4");
        let out = job.out_dir.join(name);
        let panel = match &manual_panel {
            Some(p) => p.clone(),
            None => webcam::detect_panel_window(&source, start, dur),
        };
        job.note(&format!("🎬 Рендерю ролик {idx}/{total_segments}…"));
        render::render_reel(&source, start, dur, ass_for_render.as_deref(), &out, Some(panel))?;
        produced.push(out.clone());

        // Description / title / hashtags from the reel's own transcript.
        // If subtitles were translated, describe the translated text so the
        // title/description match what viewers actually read.
        let seg_text = sub_words_for_desc
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let channel = if job.channel.is_empty() {
            stem.clone()
        } else {
            job.channel.clone()
        };
        let info = describe::describe(&seg_text, &channel);
        let text_path = out.with_extension("txt");
        std::fs::write(&text_path, describe::as_text_file(&info))?;
        log(format!(
            "description -> {}: {}",
            text_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?"),
            info.title
        ));

        if job.send_telegram {
            let caption = describe::as_caption(&info, config::describe_caption_max());
            telegram::send_video(&out, &caption, None, 0.0);
        }
    }
    Ok(produced)
}

/// Return words overlapping `[start, start+dur]` (absolute times kept).
pub fn slice_words(words: &[Word], start: f64, dur: f64) -> Vec<Word> {
    let end = start + dur;
    words
        .iter()
        .filter(|w| w.end > start && w.start < end)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_words_keeps_overlaps() {
        let words = vec![
            Word { start: 0.0, end: 1.0, text: "a".into(), confidence: 1.0 },
            Word { start: 5.0, end: 6.0, text: "b".into(), confidence: 1.0 },
            Word { start: 20.0, end: 21.0, text: "c".into(), confidence: 1.0 },
        ];
        let got = slice_words(&words, 4.0, 4.0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "b");
    }
}
