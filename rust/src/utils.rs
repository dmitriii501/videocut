//! Small helpers: logging, subprocess, ffmpeg media probing, retries, hashing.
//!
//! Media probing goes through `ffmpeg-next` (native FFI), so we do not depend on
//! an external `ffprobe` binary or its flag quirks.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

pub fn log(msg: impl AsRef<str>) {
    println!("[streamreels] {}", msg.as_ref());
}

/// Local-time-ish `YYYYMMDD-HHMMSS` stamp for run/output directory names.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm), UTC.
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

/// Run a command, echoing it, and fail on a non-zero exit.
pub fn run(cmd: &mut Command) -> Result<()> {
    log(format!("run: {:?}", cmd));
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {:?}", cmd))?;
    if !status.success() {
        bail!("command failed ({status}): {:?}", cmd);
    }
    Ok(())
}

/// Run a command capturing stdout as a UTF-8 string.
pub fn run_out(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to spawn {:?}", cmd))?;
    if !out.status.success() {
        bail!(
            "command failed ({}): {:?}\nstderr: {}",
            out.status,
            cmd,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Retry a fallible operation with linear backoff. Broad on purpose: the network
/// here is flaky (TLS resets), so any error is worth another attempt.
pub fn retry<F, T>(mut f: F, attempts: u32, delay: f64, what: &str) -> Result<T>
where
    F: FnMut() -> Result<T>,
{
    let mut last: Option<anyhow::Error> = None;
    for i in 0..attempts {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                log(format!("retry {}/{} {}: {}", i + 1, attempts, what, e));
                last = Some(e);
                if i + 1 < attempts {
                    let secs = delay * (i + 1) as f64;
                    sleep(Duration::from_secs_f64(secs));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("retry exhausted: {what}")))
}

pub fn ensure_dir(path: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("mkdir -p {}", path.display()))?;
    Ok(path.to_path_buf())
}

pub fn slugify(text: &str, maxlen: usize) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    let s = if trimmed.is_empty() { "x" } else { trimmed };
    s.chars().take(maxlen).collect()
}

/// Short stable hex digest (first 12 chars of SHA-1) for cache keys.
pub fn short_hash(text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    // A full crypto hash is overkill for cache keys; DefaultHasher is fast and
    // stable within a build. Two rounds to widen the digest.
    let mut h1 = DefaultHasher::new();
    text.hash(&mut h1);
    let mut h2 = DefaultHasher::new();
    0x9e3779b97f4a7c15u64.hash(&mut h2);
    text.hash(&mut h2);
    format!("{:016x}{:016x}", h1.finish(), h2.finish())
        .chars()
        .take(12)
        .collect()
}

// --- Media probing (native ffmpeg-next) -------------------------------------

/// Basic stream/format facts about a media file.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub has_video: bool,
    pub has_audio: bool,
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    use ffmpeg_next as ffmpeg;
    ffmpeg::init().ok();
    let ctx = ffmpeg::format::input(&path)
        .with_context(|| format!("open media {}", path.display()))?;

    let mut info = MediaInfo {
        duration: ctx.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE),
        ..Default::default()
    };

    for stream in ctx.streams() {
        let params = stream.parameters();
        match params.medium() {
            ffmpeg::media::Type::Video if !info.has_video => {
                let codec = ffmpeg::codec::context::Context::from_parameters(params)?;
                if let Ok(v) = codec.decoder().video() {
                    info.width = v.width();
                    info.height = v.height();
                    info.has_video = true;
                }
            }
            ffmpeg::media::Type::Audio => info.has_audio = true,
            _ => {}
        }
    }
    Ok(info)
}

pub fn duration(path: &Path) -> f64 {
    probe(path).map(|m| m.duration).unwrap_or(0.0)
}

pub fn video_size(path: &Path) -> Result<(u32, u32)> {
    let m = probe(path)?;
    if !m.has_video {
        bail!("no video stream in {}", path.display());
    }
    Ok((m.width, m.height))
}

pub fn has_audio(path: &Path) -> bool {
    probe(path).map(|m| m.has_audio).unwrap_or(false)
}

// --- Timecode parsing --------------------------------------------------------

/// Accept `90`, `1:30`, `01:02:03.5` -> seconds.
pub fn parse_timecode(value: &str) -> Result<f64> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty timecode");
    }
    let nums: Vec<f64> = value
        .split(':')
        .map(|p| p.trim().parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow!("bad timecode: {value:?}"))?;
    match nums.as_slice() {
        [s] => Ok(*s),
        [m, s] => Ok(m * 60.0 + s),
        [h, m, s] => Ok(h * 3600.0 + m * 60.0 + s),
        _ => bail!("bad timecode: {value:?}"),
    }
}

/// Parse `start-end` or `start+duration` -> (start, duration).
pub fn parse_segment(text: &str) -> Result<(f64, f64)> {
    let text = text.trim();
    if text.is_empty() {
        bail!("empty segment");
    }
    let (start, dur) = if let Some((a, b)) = text.split_once('-') {
        let start = parse_timecode(a)?;
        let end = parse_timecode(b)?;
        (start, end - start)
    } else if let Some((a, b)) = text.split_once('+') {
        (parse_timecode(a)?, parse_timecode(b)?)
    } else {
        bail!("segment must be 'start-end' or 'start+dur': {text:?}");
    };
    if dur <= 0.0 {
        bail!("non-positive duration in segment: {text:?}");
    }
    Ok((start, dur))
}

pub fn fmt_ts(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    let h = (seconds / 3600.0) as u64;
    let m = ((seconds % 3600.0) / 60.0) as u64;
    let s = seconds % 60.0;
    format!("{h:02}:{m:02}:{s:05.2}")
}

/// Verify external tools we shell out to are present.
pub fn check_tool(name: &str) -> Result<()> {
    let ok = Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        bail!("{name} not found in PATH");
    }
    Ok(())
}
