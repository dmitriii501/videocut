//! Live Twitch capture: resolve the HLS stream and keep a rolling ring buffer.
//!
//! The live stream cannot be seeked backwards, so we continuously record it
//! into short independent segments (ffmpeg `segment` muxer).  A monitor thread
//! tracks each finished segment together with an approximate cumulative
//! content time, so we can later cut a clip that starts a few seconds *before*
//! an event we only noticed after the fact (pre-roll).

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::utils::{ensure_dir, log, run, run_out};

/// Quality selector for the live stream (720p60 is plenty for a 720x1280 reel).
const LIVE_FORMAT: &str = "best[height<=720]/best";

/// Upper bound on continuous evidence fed into the fused hype score.
const MAX_EVIDENCE: f64 = 4.0;

fn ytdlp_bin() -> String {
    std::env::var("STREAMREELS_YTDLP").unwrap_or_else(|_| "yt-dlp".to_string())
}

/// ffprobe duration without spewing stderr on a still-writing segment.
fn quiet_duration(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Mean RMS loudness of a segment's audio (0..~1), for hype detection.
///
/// Cheap: decodes a few-second segment at low sample rate.  Returns 0.0 on any
/// failure (missing audio track, still-writing file, etc.).
fn seg_energy(path: &Path, sr: u32) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", &sr.to_string(), "-f", "f32le", "-"])
        .stderr(Stdio::null())
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => return 0.0,
    };
    let raw = out.stdout;
    if raw.is_empty() {
        return 0.0;
    }
    let n = raw.len() / 4;
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0.0f64;
    for i in 0..n {
        let b = &raw[i * 4..i * 4 + 4];
        let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64;
        sum += v * v;
    }
    (sum / n as f64).sqrt()
}

/// Extract the Twitch channel/login from a URL or a bare channel name.
pub fn channel_from_url(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if let Some(tail) = u.split("twitch.tv/").nth(1) {
        return tail
            .split('/')
            .next()
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_lowercase();
    }
    u.to_lowercase()
}

/// Resolve a Twitch channel/page URL to a direct HLS (.m3u8) URL.
pub fn resolve_live_url(url: &str) -> Result<String> {
    log(format!("resolving live stream: {url}"));
    let mut cmd = Command::new(ytdlp_bin());
    cmd.args(["-g", "-f", LIVE_FORMAT, "--no-warnings"]).arg(url);
    let out = run_out(&mut cmd)?;
    let m3u8 = out
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string());
    match m3u8 {
        Some(m) => {
            let head: String = m.chars().take(80).collect();
            log(format!("live HLS: {head}..."));
            Ok(m)
        }
        None => bail!("yt-dlp returned no stream URL (channel offline?)"),
    }
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub index: usize,
    pub path: PathBuf,
    /// Cumulative content time (sec) where this seg starts.
    pub content_start: f64,
    /// Cumulative content time where it ends.
    pub content_end: f64,
    /// Mean RMS loudness (0..1-ish) over the segment.
    pub energy: f64,
}

struct Inner {
    segments: Vec<Segment>,
    seen: HashSet<String>,
    content_cursor: f64,
    last_progress_wall: Option<Instant>,
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
}

impl Shared {
    fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Continuously record a live HLS stream into short segments.
pub struct RingBuffer {
    url: String,
    workdir: PathBuf,
    segment_secs: f64,
    keep_secs: f64,
    shared: Arc<Shared>,
    proc: Option<Child>,
    monitor: Option<JoinHandle<()>>,
    rec_log: Option<File>,
    stall_secs: f64,
}

impl RingBuffer {
    pub fn new(url: &str, workdir: &Path, segment_secs: f64, keep_secs: f64) -> Result<Self> {
        Ok(RingBuffer {
            url: url.to_string(),
            workdir: ensure_dir(workdir)?,
            segment_secs,
            keep_secs,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    segments: Vec::new(),
                    seen: HashSet::new(),
                    content_cursor: 0.0,
                    last_progress_wall: None,
                }),
                stop: AtomicBool::new(false),
            }),
            proc: None,
            monitor: None,
            rec_log: None,
            stall_secs: (segment_secs * 8.0).max(45.0),
        })
    }

    // -- lifecycle ---------------------------------------------------------
    pub fn start(&mut self) -> Result<()> {
        let m3u8 = resolve_live_url(&self.url)?;
        ensure_dir(&self.workdir)?;
        if let Ok(entries) = std::fs::read_dir(&self.workdir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n.starts_with("seg_") && n.ends_with(".mp4"))
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "warning"])
            .arg("-i")
            .arg(&m3u8)
            .args(["-c", "copy", "-f", "segment"])
            .arg("-segment_time")
            .arg(format!("{:.3}", self.segment_secs))
            .args(["-reset_timestamps", "1", "-segment_format", "mp4"])
            .arg(self.workdir.join("seg_%05d.mp4"))
            .stdout(Stdio::null());

        log("ring buffer: starting ffmpeg recorder");
        // Keep the recorder's stderr in a file: when ffmpeg dies we need to
        // know *why*.
        let rec_log_path = self
            .workdir
            .parent()
            .map(|p| p.join("recorder.log"))
            .unwrap_or_else(|| self.workdir.join("recorder.log"));
        let rec_log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&rec_log_path)
            .with_context(|| format!("open {}", rec_log_path.display()))?;
        cmd.stderr(Stdio::from(rec_log.try_clone()?));

        // Own session/process-group so stop() can reliably kill ffmpeg even if
        // it spawned helpers.  PDEATHSIG makes the kernel kill ffmpeg the
        // instant this process dies (even on SIGKILL / timeout), so a killed
        // run can never leave an orphan recorder behind.
        unsafe {
            cmd.pre_exec(|| {
                // New session -> new process group (pgid == pid).
                let _ = nix::unistd::setsid();
                let _ = nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL);
                Ok(())
            });
        }

        let child = cmd.spawn().context("spawn ffmpeg recorder")?;
        self.rec_log = Some(rec_log);
        self.proc = Some(child);

        self.shared
            .inner
            .lock()
            .unwrap()
            .last_progress_wall = Some(Instant::now());

        let shared = self.shared.clone();
        let workdir = self.workdir.clone();
        let keep = self.keep_secs;
        let monitor = thread::spawn(move || {
            while !shared.is_stopped() {
                scan(&shared, &workdir);
                prune(&shared, keep);
                thread::sleep(Duration::from_millis(500));
            }
        });
        self.monitor = Some(monitor);
        Ok(())
    }

    pub fn is_alive(&mut self) -> bool {
        match &mut self.proc {
            Some(child) => {
                if let Ok(Some(_)) = child.try_wait() {
                    return false;
                }
            }
            None => return false,
        }
        let inner = self.shared.inner.lock().unwrap();
        if let Some(last) = inner.last_progress_wall {
            if last.elapsed().as_secs_f64() > self.stall_secs {
                return false;
            }
        }
        true
    }

    /// Total finalized content time recorded so far (seconds).
    pub fn content_time(&self) -> f64 {
        self.shared.inner.lock().unwrap().content_cursor
    }

    pub fn snapshot(&self) -> Vec<Segment> {
        self.shared.inner.lock().unwrap().segments.clone()
    }

    // -- signal model ------------------------------------------------------
    fn noise_model(
        &self,
        segs: &[Segment],
        newest: f64,
        lookback_secs: f64,
        baseline_secs: f64,
        floor: f64,
    ) -> Option<(f64, f64, f64)> {
        let base: Vec<f64> = segs
            .iter()
            .filter(|s| {
                newest - baseline_secs <= s.content_end && s.content_end <= newest - lookback_secs
            })
            .map(|s| s.energy)
            .collect();
        if base.is_empty() {
            return None;
        }
        let med = median(&base);
        let mad = median(&base.iter().map(|e| (e - med).abs()).collect::<Vec<_>>());
        let mut sigma = 1.4826 * mad;
        // Heavy-tail guard.
        sigma = sigma.min(med * 0.6);
        sigma = sigma.max(med * 0.15).max(0.002);
        let gate = floor.min(med * 0.6).max(0.002);
        Some((med, sigma, gate))
    }

    fn audio_peak(
        &self,
        lookback_secs: f64,
        baseline_secs: f64,
        ratio: f64,
        floor: f64,
        adaptive: bool,
        k: f64,
    ) -> Option<(Segment, f64, f64, f64, f64)> {
        let segs = self.snapshot();
        if segs.len() < 4 {
            return None;
        }
        let newest = segs.last()?.content_end;
        let recent: Vec<Segment> = segs
            .iter()
            .filter(|s| s.content_end > newest - lookback_secs)
            .cloned()
            .collect();
        let (base_med, sigma, gate) =
            self.noise_model(&segs, newest, lookback_secs, baseline_secs, floor)?;
        if recent.is_empty() {
            return None;
        }
        let peak = recent
            .iter()
            .max_by(|a, b| a.energy.partial_cmp(&b.energy).unwrap_or(std::cmp::Ordering::Equal))?
            .clone();
        let threshold = if adaptive {
            (base_med + k * sigma).max(base_med * ratio).max(gate)
        } else {
            base_med.max(floor) * ratio
        };
        Some((peak, threshold, gate, base_med, sigma))
    }

    /// Continuous loudness evidence normalised to the trigger threshold.
    ///
    /// Returns `(content_center, evidence, (peak, median, sigma))` where
    /// `evidence` measures the *excess over the baseline median*, scaled so
    /// that 0.0 == background, 1.0 == exactly at the trigger, >1 above it.
    pub fn audio_evidence(
        &self,
        lookback_secs: f64,
        baseline_secs: f64,
        ratio: f64,
        floor: f64,
        adaptive: bool,
        k: f64,
    ) -> (Option<f64>, f64, (f64, f64, f64)) {
        let r = self.audio_peak(lookback_secs, baseline_secs, ratio, floor, adaptive, k);
        let (peak, threshold, gate, med, sigma) = match r {
            Some(v) => v,
            None => return (None, 0.0, (0.0, 0.0, 0.0)),
        };
        let stats = (peak.energy, med, sigma);
        if peak.energy <= gate {
            return (None, 0.0, stats);
        }
        let center = (peak.content_start + peak.content_end) / 2.0;
        let span = threshold - med;
        let evidence = if span <= 0.0 {
            if threshold > 0.0 {
                peak.energy / threshold
            } else {
                0.0
            }
        } else {
            (peak.energy - med) / span
        };
        (Some(center), evidence.clamp(0.0, MAX_EVIDENCE), stats)
    }

    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(child) = &mut self.proc {
            // Kill the whole process group (setsid made pgid == pid).
            let pid = nix::unistd::Pid::from_raw(child.id() as i32);
            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGTERM);
            let mut waited = false;
            for _ in 0..50 {
                if let Ok(Some(_)) = child.try_wait() {
                    waited = true;
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if !waited {
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(handle) = self.monitor.take() {
            let _ = handle.join();
        }
        self.rec_log = None;
        log("ring buffer: stopped");
    }

    // -- clip extraction ---------------------------------------------------
    /// Block until `target` seconds of content have been finalized.
    pub fn wait_for_content(&self, target: f64, timeout: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        while Instant::now() < deadline {
            if self.content_time() >= target {
                return true;
            }
            thread::sleep(Duration::from_millis(300));
        }
        false
    }

    /// Cut the `[c0, c1]` window (content seconds) from the buffer.
    pub fn extract_clip(&self, c0: f64, c1: f64, out_path: &Path) -> Result<Option<PathBuf>> {
        let timeout = ((c1 - self.content_time()) + 60.0).max(60.0);
        if !self.wait_for_content(c1, timeout) {
            log(format!("clip: timed out waiting for content {c1:.1}s"));
            return Ok(None);
        }
        let segs = self.snapshot();
        if segs.is_empty() {
            return Ok(None);
        }
        let mut covering: Vec<Segment> = segs
            .into_iter()
            .filter(|s| s.content_end > c0 && s.content_start < c1)
            .collect();
        if covering.is_empty() {
            log(format!(
                "clip: no buffered segments cover content [{c0:.1},{c1:.1}]"
            ));
            return Ok(None);
        }
        covering.sort_by_key(|s| s.index);

        if let Some(parent) = out_path.parent() {
            ensure_dir(parent)?;
        }
        let concat_list = out_path.with_extension("concat.txt");
        let list_body: String = covering
            .iter()
            .map(|s| format!("file '{}'\n", s.path.to_string_lossy()))
            .collect();
        std::fs::write(&concat_list, list_body)?;

        let joined = out_path.with_extension("joined.mp4");
        let result = (|| -> Result<()> {
            run(Command::new("ffmpeg").args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                &concat_list.to_string_lossy(),
                "-c",
                "copy",
                "-y",
                &joined.to_string_lossy(),
            ]))?;
            let offset = (c0 - covering[0].content_start).max(0.0);
            let dur = (c1 - c0).max(0.5);
            run(Command::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-ss"])
                .arg(format!("{offset:.3}"))
                .arg("-t")
                .arg(format!("{dur:.3}"))
                .arg("-i")
                .arg(&joined)
                .args(["-c", "copy", "-y"])
                .arg(out_path))?;
            Ok(())
        })();

        let _ = std::fs::remove_file(&concat_list);
        let _ = std::fs::remove_file(&joined);
        result?;
        Ok(Some(out_path.to_path_buf()))
    }
}

/// Scan the workdir for newly finalized segments and add them.
fn scan(shared: &Arc<Shared>, workdir: &Path) {
    let mut files: Vec<PathBuf> = match std::fs::read_dir(workdir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n.starts_with("seg_") && n.ends_with(".mp4"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return,
    };
    if files.len() < 2 {
        return;
    }
    files.sort();
    // The newest file is still being written, so we never touch it.
    let finalized = &files[..files.len() - 1];
    for f in finalized {
        let name = match f.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        {
            let inner = shared.inner.lock().unwrap();
            if inner.seen.contains(&name) {
                continue;
            }
        }
        let dur = quiet_duration(f);
        if dur <= 0.0 {
            continue;
        }
        let idx: usize = name
            .replace("seg_", "")
            .replace(".mp4", "")
            .parse()
            .unwrap_or(0);
        let energy = seg_energy(f, 8000);
        let mut inner = shared.inner.lock().unwrap();
        if inner.seen.contains(&name) {
            continue;
        }
        let start = inner.content_cursor;
        let end = start + dur;
        let seg = Segment {
            index: idx,
            path: f.clone(),
            content_start: start,
            content_end: end,
            energy,
        };
        inner.content_cursor += dur;
        inner.seen.insert(name);
        inner.segments.push(seg.clone());
        inner.last_progress_wall = Some(Instant::now());
        log(format!(
            "buffer: +seg {idx:05} ({dur:.1}s, e={energy:.3}) [content {:.1}-{:.1}]",
            seg.content_start, seg.content_end
        ));
    }
}

/// Drop segments older than `keep_secs` of content.
fn prune(shared: &Arc<Shared>, keep_secs: f64) {
    let mut inner = shared.inner.lock().unwrap();
    if inner.segments.is_empty() {
        return;
    }
    let newest = inner.segments.last().unwrap().content_end;
    let cutoff = newest - keep_secs;
    let mut keep: Vec<Segment> = Vec::new();
    for seg in inner.segments.drain(..) {
        if seg.content_end < cutoff {
            let _ = std::fs::remove_file(&seg.path);
        } else {
            keep.push(seg);
        }
    }
    inner.segments = keep;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_from_url_parses() {
        assert_eq!(channel_from_url("https://twitch.tv/SomeOne/"), "someone");
        assert_eq!(channel_from_url("https://www.twitch.tv/xqc?foo=1"), "xqc");
        assert_eq!(channel_from_url("barechannel"), "barechannel");
    }

    #[test]
    fn median_handles_even_and_odd() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }
}
