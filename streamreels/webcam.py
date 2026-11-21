"""Auto-detect the streamer webcam overlay panel using TypeSafe ("jev").

TypeSafe's System One endpoint (POST /v1/systemone) can localize an object in an
image from a natural-language instruction and return a normalized bbox.  We send
a few sampled stream frames, ask for the "streamer webcam video overlay panel",
and aggregate the boxes into one static panel rect.

Result is cached per source (by content hash) in cache/panels/<hash>.json so we
only pay for the API once per stream.

Docs: https://api.typesafe.ai/docs  (Swagger). Auth: `Authorization: Bearer <key>`.
Images require header `TypeSafe-Danger-Zone-Images: true`.
"""
from __future__ import annotations

import base64
import json
import subprocess
import tempfile
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from . import config
from .utils import duration, ensure_dir, log, retry, short_hash

# Fallback panel (relative coords) if detection is unavailable/unreliable.
DEFAULT_PANEL = (0.70, 0.00, 1.00, 1.00)

_QUESTION = "streamer webcam video overlay panel"
_INSTRUCTIONS = (
    "the webcam/facecam video overlay panel of the streamer, usually a "
    "rectangle in a corner of the screen showing a person"
)


@dataclass
class Panel:
    """Webcam panel rect in relative source coords (0..1)."""
    x0: float
    y0: float
    x1: float
    y1: float
    source: str = "default"  # 'typesafe' | 'default' | 'manual'

    def as_tuple(self):
        return (self.x0, self.y0, self.x1, self.y1)

    def to_json(self) -> dict:
        return {"x0": self.x0, "y0": self.y0, "x1": self.x1, "y1": self.y1,
                "source": self.source}


def _grab_frame_b64(source: Path, t: float, fmt: str = "jpg") -> str:
    """Grab a single frame at time t and return base64 (no data: prefix)."""
    with tempfile.NamedTemporaryFile(suffix=f".{fmt}", delete=False) as tmp:
        out = tmp.name
    try:
        subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error",
             "-ss", f"{t:.3f}", "-i", str(source), "-frames:v", "1",
             "-q:v", "3", "-y", out],
            check=True,
        )
        data = Path(out).read_bytes()
    finally:
        Path(out).unlink(missing_ok=True)
    if not data:
        raise RuntimeError("empty frame grab")
    return base64.b64encode(data).decode("ascii")


def _parse_box(resp: dict) -> tuple[float, float, float, float] | None:
    """Extract (x0,y0,x1,y1) relative coords from a SystemOne response."""
    answers = resp.get("answers") or {}
    for _name, ans in answers.items():
        # Answer may be wrapped: {"type": "box", "box": {...}} or the box directly.
        box = None
        if isinstance(ans, dict):
            box = ans.get("box") or (ans if {"top", "bottom", "left", "right"}
                                     <= set(ans) else None)
        if not isinstance(box, dict):
            continue
        try:
            top = float(box["top"])
            bottom = float(box["bottom"])
            left = float(box["left"])
            right = float(box["right"])
        except (KeyError, TypeError, ValueError):
            continue
        x0, x1 = sorted((left, right))
        y0, y1 = sorted((top, bottom))
        x0, x1 = max(0.0, min(1.0, x0)), max(0.0, min(1.0, x1))
        y0, y1 = max(0.0, min(1.0, y0)), max(0.0, min(1.0, y1))
        if x1 - x0 > 0.02 and y1 - y0 > 0.02:
            return x0, y0, x1, y1
    return None


def _ask_box(api_key: str, img_b64: str, fmt: str) -> tuple[float, float, float, float] | None:
    payload = {
        "model": config.TYPESAFE_MODEL,
        "state": "frame",
        "assets": {
            "frame": {"type": "image", "format": fmt, "data_base64": img_b64},
        },
        "questions": {
            "panel": {"type": "box", "instructions": _INSTRUCTIONS},
        },
    }
    body = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        f"{config.TYPESAFE_URL.rstrip('/')}/v1/systemone",
        data=body,
        method="POST",
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "TypeSafe-Danger-Zone-Images": "true",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            resp = json.loads(r.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", "replace")[:500]
        raise RuntimeError(f"TypeSafe HTTP {exc.code}: {detail}") from exc
    return _parse_box(resp)


def _plausible(box: tuple[float, float, float, float]) -> bool:
    """Reject whole-frame / content-face boxes; keep corner-panel-sized ones."""
    x0, y0, x1, y1 = box
    w, h = x1 - x0, y1 - y0
    if w < 0.04 or h < 0.04:
        return False
    if w > 0.70 or h > 0.90:  # nearly full frame -> model caught the content
        return False
    if w * h > 0.55:
        return False
    return True


def _median_box(boxes: list[tuple[float, float, float, float]]):
    """Pick the largest spatially-agreeing group of boxes, then median them.

    Guards against a single stray detection dragging the result to the wrong
    corner (common when the watched content shows a big face).
    """
    def close(a, b, tol=0.12):
        return all(abs(a[i] - b[i]) <= tol for i in range(4))

    best_group: list[tuple[float, float, float, float]] = []
    for b in boxes:
        group = [o for o in boxes if close(b, o)]
        if len(group) > len(best_group):
            best_group = group
    chosen = best_group or boxes

    xs0 = sorted(b[0] for b in chosen)
    ys0 = sorted(b[1] for b in chosen)
    xs1 = sorted(b[2] for b in chosen)
    ys1 = sorted(b[3] for b in chosen)
    m = len(chosen) // 2
    return xs0[m], ys0[m], xs1[m], ys1[m]


def _window_cache_path(source: Path, start: float) -> Path:
    key = short_hash(
        f"{source.resolve()}:{source.stat().st_size}:{source.stat().st_mtime}:"
        f"s{int(round(start))}"
    )
    return config.PANELS / f"{key}.json"


def detect_panel_window(source: Path, start: float, dur: float,
                        api_key: str | None = None,
                        use_cache: bool = True) -> Panel:
    """Detect the webcam panel for a specific clip window [start, start+dur].

    The webcam overlay can move around the screen over a long stream (e.g. left
    for a while, then right).  Within a short reel it is static, so we sample a
    few frames inside the window and take the median box, rejecting whole-frame
    / content-face outliers.  Falls back to the whole-source panel, then default.
    """
    ensure_dir(config.PANELS)
    cache = _window_cache_path(source, start)
    if use_cache and cache.exists():
        try:
            d = json.loads(cache.read_text())
            log(f"webcam panel (cached window @ {start:.0f}s): {d}")
            return Panel(d["x0"], d["y0"], d["x1"], d["y1"], d.get("source", "cached"))
        except Exception:  # noqa: BLE001
            pass

    key = api_key or config.TYPESAFE_API_KEY
    if not key:
        return Panel(*DEFAULT_PANEL, source="default")

    boxes: list[tuple[float, float, float, float]] = []
    for frac in (0.15, 0.35, 0.5, 0.65, 0.85):
        t = start + dur * frac
        try:
            img = _grab_frame_b64(source, t, "jpg")
            box = retry(lambda: _ask_box(key, img, "jpg"), attempts=4, delay=2.0,
                        what=f"typesafe box @ {t:.1f}s")
        except Exception as exc:  # noqa: BLE001
            log(f"webcam window sample @ {t:.1f}s failed: {exc}")
            continue
        if box and _plausible(box):
            log(f"webcam box @ {t:.1f}s -> x[{box[0]:.2f},{box[2]:.2f}] "
                f"y[{box[1]:.2f},{box[3]:.2f}]")
            boxes.append(box)
        elif box:
            log(f"webcam box @ {t:.1f}s rejected (implausible): "
                f"x[{box[0]:.2f},{box[2]:.2f}] y[{box[1]:.2f},{box[3]:.2f}]")

    if not boxes:
        log("webcam window detect: no plausible box; falling back to whole-source panel")
        return detect_panel(source, api_key=key, use_cache=use_cache)

    panel = Panel(*_median_box(boxes), source="typesafe")
    try:
        cache.write_text(json.dumps(panel.to_json(), indent=2))
    except Exception:  # noqa: BLE001
        pass
    log(f"webcam panel window @ {start:.0f}s: {panel.as_tuple()}")
    return panel


def _sample_times(total: float, n: int) -> list[float]:
    if total <= 0:
        return [0.0]
    # Spread across the stream, avoiding the very start/end.
    return [total * (i + 1) / (n + 1) for i in range(n)]


def _cache_path(source: Path) -> Path:
    key = short_hash(f"{source.resolve()}:{source.stat().st_size}:{source.stat().st_mtime}")
    return config.PANELS / f"{key}.json"


def detect_panel(source: Path, api_key: str | None = None,
                 samples: int | None = None, use_cache: bool = True) -> Panel:
    """Detect the webcam panel for `source`, caching the result.

    Returns a Panel. Falls back to DEFAULT_PANEL if no API key is available or
    detection fails for every sample.
    """
    ensure_dir(config.PANELS)
    cache = _cache_path(source)
    if use_cache and cache.exists():
        try:
            d = json.loads(cache.read_text())
            log(f"webcam panel (cached): {d}")
            return Panel(d["x0"], d["y0"], d["x1"], d["y1"], d.get("source", "cached"))
        except Exception:  # noqa: BLE001
            pass

    key = api_key or config.TYPESAFE_API_KEY
    if not key:
        log("webcam auto-detect: no TypeSafe API key (set TYPESAFE_API_KEY); "
            "using default panel")
        return Panel(*DEFAULT_PANEL, source="default")

    n = samples or config.TYPESAFE_SAMPLES
    total = duration(source)
    boxes: list[tuple[float, float, float, float]] = []
    for t in _sample_times(total, n):
        try:
            img = _grab_frame_b64(source, t, "jpg")
            box = retry(lambda: _ask_box(key, img, "jpg"), attempts=4, delay=2.0,
                        what=f"typesafe box @ {t:.1f}s")
            if box and _plausible(box):
                log(f"webcam box @ {t:.1f}s -> "
                    f"x[{box[0]:.2f},{box[2]:.2f}] y[{box[1]:.2f},{box[3]:.2f}]")
                boxes.append(box)
        except Exception as exc:  # noqa: BLE001
            log(f"webcam detect sample @ {t:.1f}s failed: {exc}")

    if not boxes:
        log("webcam auto-detect: all samples failed; using default panel")
        return Panel(*DEFAULT_PANEL, source="default")

    xs0 = sorted(b[0] for b in boxes)
    ys0 = sorted(b[1] for b in boxes)
    xs1 = sorted(b[2] for b in boxes)
    ys1 = sorted(b[3] for b in boxes)
    med = len(boxes) // 2
    panel = Panel(xs0[med], ys0[med], xs1[med], ys1[med], source="typesafe")

    # Sanity: a webcam panel should be a sensible chunk of the frame.
    if (panel.x1 - panel.x0) < 0.10 or (panel.y1 - panel.y0) < 0.10:
        log(f"webcam auto-detect: implausible box {panel.as_tuple()}; using default")
        return Panel(*DEFAULT_PANEL, source="default")

    try:
        cache.write_text(json.dumps(panel.to_json(), indent=2))
    except Exception:  # noqa: BLE001
        pass
    log(f"webcam panel detected: {panel.as_tuple()}")
    return panel


def parse_manual(spec: str) -> Panel:
    """Parse '--webcam x0,y0,x1,y1' (relative 0..1) into a Panel."""
    parts = [p.strip() for p in spec.replace(":", ",").split(",")]
    if len(parts) != 4:
        raise ValueError("--webcam expects x0,y0,x1,y1 in 0..1")
    x0, y0, x1, y1 = (float(p) for p in parts)
    x0, x1 = sorted((max(0.0, x0), min(1.0, x1)))
    y0, y1 = sorted((max(0.0, y0), min(1.0, y1)))
    if x1 - x0 < 0.02 or y1 - y0 < 0.02:
        raise ValueError("--webcam box too small")
    return Panel(x0, y0, x1, y1, source="manual")
