"""Face-tracking crop path computation (YuNet) with smoothing."""
from __future__ import annotations

import math
import subprocess
from dataclasses import dataclass
from pathlib import Path

import cv2
import numpy as np

from . import config
from .utils import log, retry, video_size

_ANALYZE_W = 640
_ANALYZE_FPS = 5.0
_EMA_ALPHA = 0.25


def ensure_yunet() -> Path:
    path = config.YUNET_PATH
    if path.exists() and path.stat().st_size > 100_000:
        return path
    path.parent.mkdir(parents=True, exist_ok=True)
    log("downloading YuNet face model...")

    def dl() -> None:
        subprocess.run(
            ["curl", "-L", "--fail", "--retry", "5", "--retry-all-errors",
             "-C", "-", "-o", str(path), config.YUNET_URL],
            check=True,
        )

    retry(dl, attempts=6, delay=3.0, what="yunet download")
    if path.stat().st_size < 100_000:
        raise RuntimeError("YuNet model download looks wrong (too small / LFS pointer)")
    return path


@dataclass
class CropPath:
    src_w: int
    src_h: int
    crop_w: int
    crop_h: int
    fps: int
    centers: list[tuple[float, float]]  # per output frame
    face_ratio: float

    def boxes(self):
        hw, hh = self.crop_w / 2.0, self.crop_h / 2.0
        for cx, cy in self.centers:
            x = int(round(min(max(cx - hw, 0), self.src_w - self.crop_w)))
            y = int(round(min(max(cy - hh, 0), self.src_h - self.crop_h)))
            yield x, y


def _iter_raw(proc: subprocess.Popen, w: int, h: int):
    frame_bytes = w * h * 3
    stream = proc.stdout
    assert stream is not None
    while True:
        buf = stream.read(frame_bytes)
        if len(buf) < frame_bytes:
            break
        yield np.frombuffer(buf, np.uint8).reshape(h, w, 3)


def _detect_centers(source: Path, start: float, dur: float, src_w: int, src_h: int):
    """Return (list of (cx,cy) in source coords, face_ratio)."""
    model = ensure_yunet()
    aw = _ANALYZE_W
    ah = max(2, int(round(src_h * aw / src_w)))
    detector = cv2.FaceDetectorYN.create(str(model), "", (aw, ah), 0.6, 0.3, 5000)

    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-ss", f"{start:.3f}", "-t", f"{dur:.3f}", "-i", str(source),
        "-vf", f"fps={_ANALYZE_FPS},scale={aw}:{ah}",
        "-pix_fmt", "bgr24", "-an", "-f", "rawvideo", "-",
    ]
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)

    sx, sy = src_w / aw, src_h / ah
    centers: list[tuple[float, float]] = []
    detected = 0
    total = 0
    try:
        for frame in _iter_raw(proc, aw, ah):
            total += 1
            _ret, faces = detector.detect(frame)
            if faces is not None and len(faces) > 0:
                # pick highest-confidence face
                idx = int(np.argmax(faces[:, -1]))
                fx, fy, fw, fh = faces[idx, :4]
                cx = (fx + fw / 2.0) * sx
                cy = (fy + fh / 2.0) * sy
                centers.append((float(cx), float(cy)))
                detected += 1
            else:
                centers.append((math.nan, math.nan))
    finally:
        proc.stdout and proc.stdout.close()
        proc.wait()

    ratio = detected / total if total else 0.0
    log(f"face detect: {detected}/{total} frames ({ratio * 100:.0f}%)")
    return centers, ratio


def _smooth(centers, fallback, alpha=_EMA_ALPHA):
    out: list[tuple[float, float]] = []
    prev = fallback
    for cx, cy in centers:
        if math.isnan(cx):
            cur = prev
        else:
            cur = (alpha * cx + (1 - alpha) * prev[0],
                   alpha * cy + (1 - alpha) * prev[1])
        out.append(cur)
        prev = cur
    return out


def _resample(centers, sample_fps, out_fps, n_frames):
    if not centers:
        return []
    res = []
    last = len(centers) - 1
    for i in range(n_frames):
        t = i / out_fps
        pos = t * sample_fps
        j = int(math.floor(pos))
        frac = pos - j
        j = min(max(j, 0), last)
        k = min(j + 1, last)
        cx = centers[j][0] * (1 - frac) + centers[k][0] * frac
        cy = centers[j][1] * (1 - frac) + centers[k][1] * frac
        res.append((cx, cy))
    return res


def build_crop_path(
    source: Path,
    start: float,
    dur: float,
    region_w: int,
    region_h: int,
    out_fps: int,
    zoom: float = 1.15,
) -> CropPath:
    src_w, src_h = video_size(source)
    aspect = region_w / region_h

    crop_h = src_h / max(zoom, 0.5)
    crop_w = crop_h * aspect
    if crop_w > src_w:
        crop_w = float(src_w)
        crop_h = crop_w / aspect
    if crop_h > src_h:
        crop_h = float(src_h)
        crop_w = crop_h * aspect
    cw = max(2, int(crop_w) // 2 * 2)
    ch = max(2, int(crop_h) // 2 * 2)

    raw_centers, ratio = _detect_centers(source, start, dur, src_w, src_h)
    fallback = (src_w / 2.0, src_h * 0.42)
    smoothed = _smooth(raw_centers, fallback)

    n_frames = max(1, int(round(dur * out_fps)))
    centers = _resample(smoothed, _ANALYZE_FPS, out_fps, n_frames)

    # clamp to valid crop window
    hw, hh = cw / 2.0, ch / 2.0
    clamped = [
        (min(max(cx, hw), src_w - hw), min(max(cy, hh), src_h - hh))
        for cx, cy in centers
    ]
    return CropPath(src_w, src_h, cw, ch, out_fps, clamped, ratio)
