"""Render a single preview frame for the 'screen on top, webcam below' layout.

Usage:
    python -m streamreels.preview SOURCE --time 30 [--out frame.png]

This is a fast iteration tool: it grabs one frame with ffmpeg, detects the
streamer webcam face with YuNet, and composites the candidate layout so we can
eyeball it before committing to a full render.
"""
from __future__ import annotations

import argparse
import subprocess
import tempfile
from pathlib import Path

import cv2
import numpy as np

from .analyze import ensure_yunet
from .utils import video_size

# Webcam overlay panel in relative source coords (right strip).  Determined by
# temporal-motion profiling of the reference stream; can be overridden.
WC_X0 = 0.70
WC_X1 = 1.00
WC_Y0 = 0.00
WC_Y1 = 1.00


def grab_frame(source: Path, t: float) -> np.ndarray:
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
        out = tmp.name
    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-ss", f"{t:.3f}", "-i", str(source), "-frames:v", "1",
        "-y", out,
    ]
    subprocess.run(cmd, check=True)
    img = cv2.imread(out)
    Path(out).unlink(missing_ok=True)
    if img is None:
        raise RuntimeError("failed to grab frame")
    return img


def detect_face(frame: np.ndarray, x0: int, x1: int):
    """Detect the streamer face inside the webcam strip. Returns (fx,fy,fw,fh) in
    full-frame coords or None."""
    model = ensure_yunet()
    strip = frame[:, x0:x1]
    h, w = strip.shape[:2]
    det = cv2.FaceDetectorYN.create(str(model), "", (w, h), 0.5, 0.3, 5000)
    _ret, faces = det.detect(strip)
    if faces is None or len(faces) == 0:
        return None
    idx = int(np.argmax(faces[:, -1]))
    fx, fy, fw, fh = faces[idx, :4]
    return (fx + x0, fy, fw, fh)


def _fit_contain(img: np.ndarray, w: int, h: int) -> np.ndarray:
    ih, iw = img.shape[:2]
    s = min(w / iw, h / ih)
    nw, nh = max(1, int(iw * s)), max(1, int(ih * s))
    resized = cv2.resize(img, (nw, nh), interpolation=cv2.INTER_LANCZOS4)
    canvas = np.zeros((h, w, 3), np.uint8)
    ox, oy = (w - nw) // 2, (h - nh) // 2
    canvas[oy:oy + nh, ox:ox + nw] = resized
    return canvas


def _blur_fill(img: np.ndarray, w: int, h: int) -> np.ndarray:
    bg = _fit_cover(img, w, h)
    bg = cv2.GaussianBlur(bg, (0, 0), sigmaX=24)
    ih, iw = img.shape[:2]
    s = min(w / iw, h / ih)
    nw, nh = max(1, int(iw * s)), max(1, int(ih * s))
    fg = cv2.resize(img, (nw, nh), interpolation=cv2.INTER_LANCZOS4)
    ox, oy = (w - nw) // 2, (h - nh) // 2
    bg[oy:oy + nh, ox:ox + nw] = fg
    return bg


def _fit_cover(img: np.ndarray, w: int, h: int) -> np.ndarray:
    ih, iw = img.shape[:2]
    s = max(w / iw, h / ih)
    nw, nh = max(w, int(np.ceil(iw * s))), max(h, int(np.ceil(ih * s)))
    resized = cv2.resize(img, (nw, nh), interpolation=cv2.INTER_LANCZOS4)
    ox, oy = max(0, (nw - w) // 2), max(0, (nh - h) // 2)
    return resized[oy:oy + h, ox:ox + w]


def webcam_panel(frame: np.ndarray, face, panel_w: int, panel_h: int,
                 face_fill: float = 2.4, y_bias: float = 0.42) -> np.ndarray:
    """Crop the webcam region centered on the face and scale to the panel.

    The webcam strip is narrow in the source, so we prefer to use its FULL width
    (minimal upscaling) and only slide vertically to keep the face framed.
    """
    fh_img, fw_img = frame.shape[:2]
    sx0, sx1 = int(WC_X0 * fw_img), int(WC_X1 * fw_img)
    sy0, sy1 = int(WC_Y0 * fh_img), int(WC_Y1 * fh_img)

    aspect = panel_w / panel_h
    strip_w = sx1 - sx0
    # Prefer the whole strip width -> tallest crop that fits the panel aspect.
    crop_w = float(strip_w)
    crop_h = crop_w / aspect
    if crop_h > (sy1 - sy0):
        crop_h = float(sy1 - sy0)
        crop_w = crop_h * aspect

    if face is not None:
        fx, fy, fw, fh = face
        cx = fx + fw / 2.0
        cy = fy + fh * y_bias
    else:
        cx = (sx0 + sx1) / 2.0
        cy = (sy0 + sy1) * 0.42

    x = int(round(min(max(cx - crop_w / 2, sx0), sx1 - crop_w)))
    y = int(round(min(max(cy - crop_h / 2, sy0), sy1 - crop_h)))
    x = max(0, x); y = max(0, y)
    crop = frame[y:y + int(crop_h), x:x + int(crop_w)]
    return cv2.resize(crop, (panel_w, panel_h), interpolation=cv2.INTER_LANCZOS4)


def compose(frame: np.ndarray, top_h: int, out_w: int, out_h: int,
            face, top_mode: str = "blur") -> np.ndarray:
    bot_h = out_h - top_h
    if top_mode == "blur":
        top = _blur_fill(frame, out_w, top_h)
    elif top_mode == "contain":
        top = _fit_contain(frame, out_w, top_h)
    else:  # cover (crop sides)
        top = _fit_cover(frame, out_w, top_h)
    bottom = webcam_panel(frame, face, out_w, bot_h)
    return np.vstack([top, bottom])


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="streamreels.preview")
    ap.add_argument("source", type=Path)
    ap.add_argument("--time", type=float, default=30.0)
    ap.add_argument("--out", type=Path, default=Path("/tmp/preview.png"))
    ap.add_argument("--top-h", type=int, default=None,
                    help="height of the top (screen) band; default = 16:9 fit")
    args = ap.parse_args(argv)

    src_w, src_h = video_size(args.source)
    frame = grab_frame(args.source, args.time)
    x0, x1 = int(WC_X0 * src_w), int(WC_X1 * src_w)
    face = detect_face(frame, x0, x1)
    print(f"source {src_w}x{src_h}; webcam strip x={x0}..{x1}; face={face}")

    out_w, out_h = 720, 1280
    top_h = args.top_h
    if top_h is None:
        top_h = int(round(out_w * 9 / 16))  # full 16:9 screen, no bars
        top_h += top_h % 2

    variants = {
        "screen_full": (top_h, "contain"),
        "screen_blur": (out_h - int(round(out_w * 9 / 16)), "blur"),
        "half": (out_h // 2, "blur"),
    }
    for name, (th, mode) in variants.items():
        canvas = compose(frame, th, out_w, out_h, face, top_mode=mode)
        path = args.out.with_name(f"{args.out.stem}_{name}{args.out.suffix}")
        cv2.imwrite(str(path), canvas)
        print(f"wrote {path}  (top_h={th}, bottom_h={out_h - th}, mode={mode})")

    # Debug: original frame with webcam strip + face box drawn.
    dbg = frame.copy()
    cv2.line(dbg, (x0, 0), (x0, src_h), (0, 0, 255), 2)
    if face is not None:
        fx, fy, fw, fh = [int(v) for v in face]
        cv2.rectangle(dbg, (fx, fy), (fx + fw, fy + fh), (0, 255, 0), 2)
    cv2.imwrite(str(args.out.with_name(f"{args.out.stem}_debug{args.out.suffix}")), dbg)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
