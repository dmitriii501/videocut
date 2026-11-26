//! Gameplay backgrounds for the bottom half of a reel.
//!
//! A preset maps to a YouTube search query. The chosen clip is downloaded once
//! via `yt-dlp` into the cache and reused for every job. To composite it we
//! pre-render a finite, band-sized strip with ffmpeg and reference that strip
//! from the reel filtergraph with `movie=` (see [`crate::vod::render`]).
//!
//! A looping `movie=...:loop` / `loop` filter proved unreliable through
//! ffmpeg-next (it either hangs or freezes after one pass), so instead we
//! materialise a strip that is long enough for the reel and trim it in-graph.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;
use crate::utils::{duration, ensure_dir, log, retry, run_out, short_hash};
use crate::vod::fetch;

/// A named gameplay background.
pub struct Preset {
    key: &'static str,
    label: &'static str,
    query: &'static str,
}

impl Preset {
    pub fn key(&self) -> &'static str {
        self.key
    }

    pub fn label(&self) -> &'static str {
        self.label
    }
}

const PRESETS: &[Preset] = &[
    Preset {
        key: "subway",
        label: "Subway Surfers",
        query: "subway surfers gameplay no commentary",
    },
    Preset {
        key: "minecraft",
        label: "Minecraft Parkour",
        query: "minecraft parkour gameplay no commentary",
    },
    Preset {
        key: "gta",
        label: "GTA 5 Driving",
        query: "gta 5 driving gameplay no commentary",
    },
    Preset {
        key: "asmr",
        label: "ASMR / Sand",
        query: "satisfying slime asmr",
    },
];

/// All built-in gameplay presets, in display order.
pub fn presets() -> &'static [Preset] {
    PRESETS
}

/// Resolve a spec into a cached clip on disk.
///
/// Accepted specs:
/// * `"none"` / empty  -> `Ok(None)` (webcam-only bottom band),
/// * an existing file  -> that file,
/// * a preset key      -> its downloaded clip,
/// * `"query:<text>"`  -> a clip from a custom YouTube search.
pub fn clip_for(spec: &str) -> Result<Option<PathBuf>> {
    let spec = spec.trim();
    if spec.is_empty() || spec == "none" || !config::gameplay_enabled() {
        return Ok(None);
    }
    if let Some(override_path) = config::gameplay_clip() {
        if override_path.is_file() {
            log(format!(
                "gameplay: using local clip {}",
                override_path.display()
            ));
            return Ok(Some(override_path));
        }
        bail!(
            "STREAMREELS_GAMEPLAY_CLIP points to a missing file: {}",
            override_path.display()
        );
    }
    if Path::new(spec).is_file() {
        return Ok(Some(PathBuf::from(spec)));
    }
    if let Some(query) = spec.strip_prefix("query:") {
        let key = format!("q-{}", short_hash(query));
        return Ok(Some(ensure_clip(&key, query)?));
    }
    match PRESETS.iter().find(|p| p.key == spec) {
        Some(preset) => Ok(Some(ensure_clip(preset.key, preset.query)?)),
        None => {
            let keys: Vec<&str> = PRESETS.iter().map(Preset::key).collect();
            bail!("unknown gameplay preset {spec:?} (use one of {keys:?} or a file path)")
        }
    }
}

fn gameplay_dir() -> PathBuf {
    config::cache().join("gameplay")
}

/// Return a cached clip whose filename starts with `key-` and is long enough.
fn cached_clip(key: &str) -> Option<PathBuf> {
    let dir = gameplay_dir();
    let prefix = format!("{key}-");
    let min = config::gameplay_min_seconds();
    let mut best: Option<(f64, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.starts_with(&prefix) || !name.ends_with(".mp4") {
            continue;
        }
        let dur = duration(&path);
        if dur < min {
            continue;
        }
        if best.as_ref().map(|(d, _)| dur > *d).unwrap_or(true) {
            best = Some((dur, path));
        }
    }
    best.map(|(_, p)| p)
}

fn ensure_clip(key: &str, query: &str) -> Result<PathBuf> {
    // Height is part of the name so a 720p clip is not scaled up and called 1080.
    let key = format!("{key}-h{}", config::max_source_height());
    if let Some(clip) = cached_clip(&key) {
        log(format!("gameplay: reusing cached clip {}", clip.display()));
        return Ok(clip);
    }
    log(format!("gameplay: downloading {query:?}"));
    let clip = download(&key, query)?;
    let dur = duration(&clip);
    if dur < config::gameplay_min_seconds() {
        bail!(
            "gameplay clip too short ({dur:.0}s) for {query:?}: {}",
            clip.display()
        );
    }
    log(format!(
        "gameplay: clip ready {} ({dur:.0}s)",
        clip.display()
    ));
    Ok(clip)
}

fn download(key: &str, query: &str) -> Result<PathBuf> {
    let dir = ensure_dir(&gameplay_dir())?;

    let attempt = || -> Result<PathBuf> {
        let mut cmd = Command::new(fetch::ytdlp_bin());
        cmd.arg("-S")
            .arg(format!(
                "res:{},ext:mp4:m4a",
                config::max_source_height()
            ))
            .arg("--format-sort-force")
            .args(fetch::common_args())
            .arg("--output")
            .arg(dir.join(format!("{key}-%(id)s.%(ext)s")))
            .arg("--print")
            .arg("after_move:filepath")
            .arg(format!("ytsearch1:{query}"));

        let out = run_out(&mut cmd)?;
        if let Some(line) = out
            .lines()
            .map(str::trim)
            .rfind(|l| l.ends_with(".mp4"))
        {
            return Ok(PathBuf::from(line));
        }
        // Fall back to the newest clip with our key prefix.
        let prefix = format!("{key}-");
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .map(|n| n.starts_with(&prefix) && n.ends_with(".mp4"))
                    .unwrap_or(false)
            })
            .collect();
        found.sort_by_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        found
            .pop()
            .ok_or_else(|| anyhow::anyhow!("yt-dlp produced no gameplay clip for {query:?}"))
    };

    retry(attempt, 3, 3.0, &format!("gameplay download {query}"))
}

/// Pre-render (once, cached) a strip sized to the bottom band (`width` x
/// `height`) and long enough for a `reel_dur`-second reel. Returns `None` if
/// ffmpeg fails, so the caller can fall back to the webcam-only band.
pub fn strip_for(width: u32, height: u32, clip: &Path, reel_dur: f64) -> Option<PathBuf> {
    // Round the strip length up in 30s buckets (min 60s) so a single cached
    // strip serves many reels of similar length.
    let strip_dur = ((reel_dur / 30.0).ceil() * 30.0).max(60.0) as u32;
    let dir = config::cache().join("gameplay");
    ensure_dir(&dir).ok()?;

    let key = format!(
        "{}-{}",
        clip.to_string_lossy(),
        clip.metadata().map(|m| m.len()).unwrap_or(0)
    );
    let path = dir.join(format!(
        "strip-{}-{}x{}-{}.mp4",
        &short_hash(&key)[..12],
        width,
        height,
        strip_dur
    ));
    if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Some(path);
    }

    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=increase:flags=lanczos,\
         crop={w}:{h},fps={fps},setsar=1,format=yuv420p",
        w = width,
        h = height,
        fps = config::fps(),
    );
    log(format!(
        "gameplay: rendering strip {} ({}x{}, {}s)",
        path.display(),
        width,
        height,
        strip_dur
    ));
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-stream_loop", "-1", "-i"])
        .arg(clip)
        .args(["-t", &strip_dur.to_string(), "-vf", &vf, "-an", "-c:v", "libx264", "-preset", "ultrafast", "-crf", "20"])
        .arg(&path)
        .status();
    match status {
        Ok(s) if s.success() => Some(path),
        Ok(s) => {
            log(format!("gameplay: ffmpeg strip failed with {s}"));
            None
        }
        Err(e) => {
            log(format!("gameplay: failed to run ffmpeg: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_are_unique_and_known() {
        let keys: Vec<&str> = PRESETS.iter().map(Preset::key).collect();
        for k in ["subway", "minecraft", "gta", "asmr"] {
            assert!(keys.contains(&k), "missing preset {k}");
        }
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "duplicate preset keys");
    }

    #[test]
    fn none_and_empty_disable_gameplay() {
        assert!(clip_for("").unwrap().is_none());
        assert!(clip_for("none").unwrap().is_none());
        assert!(clip_for("  none  ").unwrap().is_none());
    }

    #[test]
    fn unknown_preset_is_an_error() {
        assert!(clip_for("no-such-preset").is_err());
    }
}
