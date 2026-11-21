"""Compose the final vertical reel: stream screen on top, static webcam below.

Layout (720x1280, 9:16):
  TOP    (50%) = full stream frame, letterboxed on a blurred fill (16:9 fits).
  BOTTOM (50%) = streamer webcam panel (auto-detected via TypeSafe or given),
                 cropped to the panel aspect and scaled up.  Fully STATIC: no
                 face tracking, no panning.

Gameplay background is no longer used.
"""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from . import config
from .utils import ensure_dir, has_audio, log, run, video_size
from .webcam import DEFAULT_PANEL, Panel

# Fallback webcam panel in relative source coords (used when none supplied).
WC_X0, WC_Y0, WC_X1, WC_Y1 = DEFAULT_PANEL


@dataclass
class Layout:
    width: int
    height: int
    top_h: int
    bot_h: int


def layout() -> Layout:
    w = config.REEL_W // 2 * 2
    h = config.REEL_H // 2 * 2
    top_h = h // 2 // 2 * 2
    bot_h = h - top_h
    return Layout(w, h, top_h, bot_h)


def _webcam_crop(src_w: int, src_h: int, panel: Panel,
                 panel_w: int, panel_h: int):
    """Static webcam crop rect (x, y, w, h) in source pixels, centered on the
    detected panel and shaped to the output band aspect."""
    px0 = panel.x0 * src_w
    py0 = panel.y0 * src_h
    px1 = panel.x1 * src_w
    py1 = panel.y1 * src_h
    panel_px_w = max(2.0, px1 - px0)

    aspect = panel_w / panel_h
    crop_w = panel_px_w
    crop_h = crop_w / aspect
    if crop_h > src_h:
        crop_h = float(src_h)
        crop_w = crop_h * aspect

    cx = (px0 + px1) / 2.0
    cy = (py0 + py1) / 2.0
    x = int(round(min(max(cx - crop_w / 2, 0), src_w - crop_w)))
    y = int(round(min(max(cy - crop_h / 2, 0), src_h - crop_h)))
    return max(0, x), max(0, y), int(crop_w) // 2 * 2, int(crop_h) // 2 * 2


def build_filter(lay: Layout, src_w: int, src_h: int, fps: int,
                 ass_path: Path | None, panel: Panel | None = None) -> str:
    w, h = lay.width, lay.height
    panel = panel or Panel(*DEFAULT_PANEL)
    wx, wy, ww, wh = _webcam_crop(src_w, src_h, panel, w, lay.bot_h)

    # TOP: split into blur background (cover) + fitted foreground (contain).
    top = (
        "[0:v]split=3[sbg][sfg][swc];"
        f"[sbg]scale={w}:{lay.top_h}:force_original_aspect_ratio=increase:flags=lanczos,"
        f"crop={w}:{lay.top_h},boxblur=20:2[bg];"
        f"[sfg]scale={w}:{lay.top_h}:force_original_aspect_ratio=decrease:flags=lanczos[fg];"
        f"[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1,fps={fps}[vtop];"
    )
    # BOTTOM: static webcam crop, scaled to the panel.
    bottom = (
        f"[swc]crop={ww}:{wh}:{wx}:{wy},"
        f"scale={w}:{lay.bot_h}:flags=lanczos,setsar=1,fps={fps}[vbot];"
    )
    stack = "[vtop][vbot]vstack=inputs=2[vs];"

    if ass_path is not None and Path(ass_path).exists() and Path(ass_path).stat().st_size > 0:
        tail = f"[vs]ass='{ass_path}'[vout]"
    else:
        tail = "[vs]null[vout]"
    return top + bottom + stack + tail


def render_reel(
    *,
    source: Path,
    start: float,
    dur: float,
    ass_path: Path | None,
    out_path: Path,
    crf: int = 20,
    preset: str = "veryfast",
    panel: Panel | None = None,
) -> Path:
    ensure_dir(out_path.parent)
    lay = layout()
    fps = config.FPS
    src_w, src_h = video_size(source)

    filter_complex = build_filter(lay, src_w, src_h, fps, ass_path, panel)
    audio_in = has_audio(source)

    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-stats",
        "-ss", f"{start:.3f}", "-t", f"{dur:.3f}", "-i", str(source),
        "-filter_complex", filter_complex,
        "-map", "[vout]",
    ]
    if audio_in:
        cmd += ["-map", "0:a", "-c:a", "aac", "-b:a", "128k"]
    cmd += [
        "-c:v", "libx264", "-preset", preset, "-crf", str(crf),
        "-pix_fmt", "yuv420p", "-movflags", "+faststart",
        "-t", f"{dur:.3f}",
        "-y", str(out_path),
    ]
    log(f"rendering reel -> {out_path}")
    run(cmd)
    log(f"done: {out_path}")
    return out_path
