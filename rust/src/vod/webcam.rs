//! Auto-detect the streamer's face using TypeSafe ("jev").
//!
//! TypeSafe's System One endpoint (POST /v1/systemone) localizes an object from
//! a natural-language instruction and returns a normalized bbox. We ask it for
//! the **face** of the person on screen (or the avatar/character face for a
//! VTuber), expand that tight box into a head-and-shoulders framing, then sample
//! a few frames and aggregate the boxes into one static rect, cached per source
//! window.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::config;
use crate::utils::{duration, ensure_dir, log, retry, short_hash};

/// Fallback panel (relative coords) if detection is unavailable/unreliable.
pub const DEFAULT_PANEL: (f64, f64, f64, f64) = (0.70, 0.00, 1.00, 1.00);

const QUESTION_INSTRUCTIONS: &str = "the face of the streamer — the head/face of the person \
    shown on screen. If the streamer is a VTuber, use the face of their avatar/character. \
    Return a tight box around the face itself, not the whole webcam panel or its background";

/// Webcam panel rect in relative source coords (0..1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    #[serde(default = "default_source")]
    pub source: String,
}

fn default_source() -> String {
    "default".to_string()
}

impl Panel {
    pub fn new(t: (f64, f64, f64, f64)) -> Self {
        Panel {
            x0: t.0,
            y0: t.1,
            x1: t.2,
            y1: t.3,
            source: "default".to_string(),
        }
    }

    pub fn with_source(t: (f64, f64, f64, f64), source: &str) -> Self {
        Panel {
            x0: t.0,
            y0: t.1,
            x1: t.2,
            y1: t.3,
            source: source.to_string(),
        }
    }

    pub fn as_tuple(&self) -> (f64, f64, f64, f64) {
        (self.x0, self.y0, self.x1, self.y1)
    }
}

/// Parse `--webcam x0,y0,x1,y1` (relative 0..1) into a Panel.
pub fn parse_manual(spec: &str) -> Result<Panel> {
    let normalized = spec.replace(':', ",");
    let parts: Vec<&str> = normalized.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(anyhow!("--webcam expects x0,y0,x1,y1 in 0..1"));
    }
    let v: Vec<f64> = parts
        .iter()
        .map(|p| p.parse::<f64>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow!("--webcam expects numbers"))?;
    let (mut x0, mut x1) = (v[0].max(0.0), v[2].min(1.0));
    let (mut y0, mut y1) = (v[1].max(0.0), v[3].min(1.0));
    if x0 > x1 {
        std::mem::swap(&mut x0, &mut x1);
    }
    if y0 > y1 {
        std::mem::swap(&mut y0, &mut y1);
    }
    if x1 - x0 < 0.02 || y1 - y0 < 0.02 {
        return Err(anyhow!("--webcam box too small"));
    }
    Ok(Panel::with_source((x0, y0, x1, y1), "manual"))
}

/// Grab a single frame as JPEG bytes via the ffmpeg binary (simple + reliable
/// for a one-shot still).
fn grab_frame_jpeg(source: &Path, t: f64) -> Result<Vec<u8>> {
    let tmp = std::env::temp_dir().join(format!(
        "sr-frame-{}-{}.jpg",
        std::process::id(),
        (t * 1000.0) as i64
    ));
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-ss")
        .arg(format!("{t:.3}"))
        .arg("-i")
        .arg(source)
        .args(["-frames:v", "1", "-q:v", "3", "-y"])
        .arg(&tmp)
        .status()
        .context("spawn ffmpeg for frame grab")?;
    if !status.success() {
        return Err(anyhow!("ffmpeg frame grab failed at {t:.1}s"));
    }
    let data = std::fs::read(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    if data.is_empty() {
        return Err(anyhow!("empty frame grab"));
    }
    Ok(data)
}

fn parse_box(resp: &serde_json::Value) -> Option<(f64, f64, f64, f64)> {
    let answers = resp.get("answers")?.as_object()?;
    for (_name, ans) in answers {
        let box_obj = ans
            .get("box")
            .and_then(|b| b.as_object())
            .or_else(|| {
                if ans.get("top").is_some()
                    && ans.get("bottom").is_some()
                    && ans.get("left").is_some()
                    && ans.get("right").is_some()
                {
                    ans.as_object()
                } else {
                    None
                }
            })?;
        let top = box_obj.get("top")?.as_f64()?;
        let bottom = box_obj.get("bottom")?.as_f64()?;
        let left = box_obj.get("left")?.as_f64()?;
        let right = box_obj.get("right")?.as_f64()?;
        let (x0, x1) = (left.min(right), left.max(right));
        let (y0, y1) = (top.min(bottom), top.max(bottom));
        let x0 = x0.clamp(0.0, 1.0);
        let x1 = x1.clamp(0.0, 1.0);
        let y0 = y0.clamp(0.0, 1.0);
        let y1 = y1.clamp(0.0, 1.0);
        if x1 - x0 > 0.02 && y1 - y0 > 0.02 {
            return Some((x0, y0, x1, y1));
        }
    }
    None
}

fn ask_box(api_key: &str, img: &[u8]) -> Result<Option<(f64, f64, f64, f64)>> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(img);
    let payload = serde_json::json!({
        "model": config::typesafe_model(),
        "state": "frame",
        "assets": {
            "frame": {"type": "image", "format": "jpg", "data_base64": encoded}
        },
        "questions": {
            "panel": {"type": "box", "instructions": QUESTION_INSTRUCTIONS}
        }
    });
    let url = format!("{}/v1/systemone", config::typesafe_url().trim_end_matches('/'));
    let resp = crate::http::post_json(
        &url,
        &[
            ("Authorization", &format!("Bearer {api_key}")),
            ("TypeSafe-Danger-Zone-Images", "true"),
        ],
        &payload,
        60,
    )?;
    Ok(parse_box(&resp))
}

/// Expand a tight face box into a head-and-shoulders framing, since the render
/// crops the whole "panel" band and a bare face would be cropped too tightly.
fn expand_face_box(b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (x0, y0, x1, y1) = b;
    let fw = (x1 - x0).max(0.01);
    let fh = (y1 - y0).max(0.01);
    let cx = (x0 + x1) / 2.0;

    // Wider than the face and taller (down to the shoulders).
    let pw = fw * 2.6;
    let ph = fh * 3.2;
    let px0 = cx - pw / 2.0;
    let px1 = cx + pw / 2.0;
    let py0 = y0 - fh * 0.45;
    let py1 = py0 + ph;

    (
        px0.clamp(0.0, 1.0),
        py0.clamp(0.0, 1.0),
        px1.clamp(0.0, 1.0),
        py1.clamp(0.0, 1.0),
    )
}

fn plausible(b: (f64, f64, f64, f64)) -> bool {
    let (x0, y0, x1, y1) = b;
    let (w, h) = (x1 - x0, y1 - y0);
    if w < 0.02 || h < 0.02 {
        return false;
    }
    if w > 0.98 || h > 0.98 {
        return false;
    }
    if w * h > 0.92 {
        return false;
    }
    true
}

/// Pick the largest spatially-agreeing group of boxes, then median them.
fn median_box(boxes: &[(f64, f64, f64, f64)]) -> (f64, f64, f64, f64) {
    let close = |a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)| -> bool {
        (0..4).all(|i| {
            let av = [a.0, a.1, a.2, a.3][i];
            let bv = [b.0, b.1, b.2, b.3][i];
            (av - bv).abs() <= 0.12
        })
    };
    let mut best: Vec<(f64, f64, f64, f64)> = Vec::new();
    for &b in boxes {
        let group: Vec<_> = boxes.iter().copied().filter(|&o| close(b, o)).collect();
        if group.len() > best.len() {
            best = group;
        }
    }
    let chosen = if best.is_empty() { boxes.to_vec() } else { best };

    let mut xs0: Vec<f64> = chosen.iter().map(|b| b.0).collect();
    let mut ys0: Vec<f64> = chosen.iter().map(|b| b.1).collect();
    let mut xs1: Vec<f64> = chosen.iter().map(|b| b.2).collect();
    let mut ys1: Vec<f64> = chosen.iter().map(|b| b.3).collect();
    xs0.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys0.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs1.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ys1.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let m = chosen.len() / 2;
    (xs0[m], ys0[m], xs1[m], ys1[m])
}

fn window_cache_path(source: &Path, start: f64) -> PathBuf {
    let size = source.metadata().map(|m| m.len()).unwrap_or(0);
    let mtime = source
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = short_hash(&format!(
        "{}:{size}:{mtime}:s{}",
        source.display(),
        start.round() as i64
    ));
    config::panels().join(format!("{key}.json"))
}

fn load_cached(path: &Path) -> Option<Panel> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn store_cached(path: &Path, panel: &Panel) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(panel) {
        let _ = std::fs::write(path, text);
    }
}

/// Detect the streamer face panel for a specific clip window `[start, start+dur]`.
pub fn detect_panel_window(source: &Path, start: f64, dur: f64) -> Panel {
    let _ = ensure_dir(&config::panels());
    let cache = window_cache_path(source, start);
    if let Some(p) = load_cached(&cache) {
        log(format!("face panel (cached window @ {start:.0}s): {:?}", p.as_tuple()));
        return p;
    }

    let key = match config::typesafe_api_key() {
        Some(k) => k,
        None => return Panel::new(DEFAULT_PANEL),
    };

    let mut boxes: Vec<(f64, f64, f64, f64)> = Vec::new();
    for frac in [0.15, 0.35, 0.5, 0.65, 0.85] {
        let t = start + dur * frac;
        let img = match grab_frame_jpeg(source, t) {
            Ok(i) => i,
            Err(e) => {
                log(format!("webcam window sample @ {t:.1}s failed: {e}"));
                continue;
            }
        };
        let box_opt = retry(|| ask_box(&key, &img), 4, 2.0, &format!("typesafe box @ {t:.1}s"));
        match box_opt {
            Ok(Some(b)) if plausible(b) => {
                let face = expand_face_box(b);
                log(format!(
                    "face @ {t:.1}s -> face x[{:.2},{:.2}] y[{:.2},{:.2}] => panel x[{:.2},{:.2}] y[{:.2},{:.2}]",
                    b.0, b.2, b.1, b.3, face.0, face.2, face.1, face.3
                ));
                boxes.push(face);
            }
            Ok(Some(b)) => log(format!(
                "face @ {t:.1}s rejected (implausible): x[{:.2},{:.2}] y[{:.2},{:.2}]",
                b.0, b.2, b.1, b.3
            )),
            Ok(None) => {}
            Err(e) => log(format!("webcam window sample @ {t:.1}s failed: {e}")),
        }
    }

    if boxes.is_empty() {
        log("face window detect: no plausible box; falling back to whole-source panel");
        return detect_panel(source);
    }

    let panel = Panel::with_source(median_box(&boxes), "face");
    store_cached(&cache, &panel);
    log(format!("face panel window @ {start:.0}s: {:?}", panel.as_tuple()));
    panel
}

/// Detect the face panel from a single already-grabbed JPEG frame.
///
/// Returns `None` when no TypeSafe key is configured or the model finds no
/// plausible face. Used by the studio preview so the operator sees the same
/// box the render would pick before starting a job.
pub fn detect_from_jpeg(img: &[u8]) -> Option<Panel> {
    let key = config::typesafe_api_key()?;
    let b = match retry(|| ask_box(&key, img), 3, 1.5, "typesafe preview box") {
        Ok(Some(b)) if plausible(b) => b,
        _ => return None,
    };
    let panel = Panel::with_source(expand_face_box(b), "face");
    log(format!("preview face panel: {:?}", panel.as_tuple()));
    Some(panel)
}

/// Detect the streamer face panel for a whole source, cached.
pub fn detect_panel(source: &Path) -> Panel {
    let _ = ensure_dir(&config::panels());
    let key_hash = short_hash(&format!(
        "{}:{}:{}",
        source.display(),
        source.metadata().map(|m| m.len()).unwrap_or(0),
        source
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ));
    let cache = config::panels().join(format!("{key_hash}.json"));
    if let Some(p) = load_cached(&cache) {
        log(format!("face panel (cached): {:?}", p.as_tuple()));
        return p;
    }

    let key = match config::typesafe_api_key() {
        Some(k) => k,
        None => {
            log("face auto-detect: no TypeSafe API key; using default panel");
            return Panel::new(DEFAULT_PANEL);
        }
    };

    let n = config::typesafe_samples().max(1);
    let total = duration(source);
    let mut boxes: Vec<(f64, f64, f64, f64)> = Vec::new();
    for i in 0..n {
        let t = if total > 0.0 {
            total * (i + 1) as f64 / (n + 1) as f64
        } else {
            0.0
        };
        let img = match grab_frame_jpeg(source, t) {
            Ok(i) => i,
            Err(e) => {
                log(format!("webcam detect sample @ {t:.1}s failed: {e}"));
                continue;
            }
        };
        match retry(|| ask_box(&key, &img), 4, 2.0, &format!("typesafe box @ {t:.1}s")) {
            Ok(Some(b)) if plausible(b) => boxes.push(expand_face_box(b)),
            _ => {}
        }
    }

    if boxes.is_empty() {
        log("face auto-detect: all samples failed; using default panel");
        return Panel::new(DEFAULT_PANEL);
    }

    let panel = Panel::with_source(median_box(&boxes), "face");
    if (panel.x1 - panel.x0) < 0.10 || (panel.y1 - panel.y0) < 0.10 {
        log(format!(
            "face auto-detect: implausible box {:?}; using default",
            panel.as_tuple()
        ));
        return Panel::new(DEFAULT_PANEL);
    }
    store_cached(&cache, &panel);
    log(format!("face panel detected: {:?}", panel.as_tuple()));
    panel
}
