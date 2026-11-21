"""Automatic highlight detection.

Two cheap, CPU-only signals are combined:

* **Audio energy** - laughter, shouting, hype and sudden reactions produce
  short bursts of loudness well above the stream's rolling baseline.  We score
  every moment by how far its short-time RMS energy rises above the median.
* **Speech cues** - if a word-level transcript is available we boost windows
  that contain laughter / hype keywords (RU + EN) and exclamations.

The result is a list of ``(start, duration)`` segments, best first, that can be
fed straight into the normal rendering pipeline.
"""
from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass
from pathlib import Path

from . import config
from .utils import duration, log

# Words that usually surround funny / hype moments (lower-cased, no punctuation).
_LAUGH_RE = re.compile(
    r"(?:"
    r"ха+х|аха+х|хе+х|хи+х|хаха|ахах|лол|рофл|ору|ржу|смешн|смех|"
    r"пиздец|пздц|жесть|красав|ауф|нихуя|охуе|офиге|"
    r"lol|lmao|haha|hehe|omg|wtf|no way|insane|crazy"
    r")",
    re.IGNORECASE,
)


@dataclass
class Window:
    t: float          # window start (seconds)
    score: float      # combined 0..1-ish score
    energy: float     # raw normalised audio energy
    cue: bool         # has speech cue


def _decode_mono(source: Path) -> "object":
    """Decode the whole file to 16 kHz mono float32 (numpy array)."""
    import numpy as np

    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-i", str(source), "-vn", "-ac", "1", "-ar", "16000",
        "-f", "f32le", "-",
    ]
    raw = subprocess.check_output(cmd)
    return np.frombuffer(raw, dtype=np.float32)


def _energy_windows(audio, sr: int = 16000, win: float = 0.5, hop: float = 0.25):
    """Return (times, rms) arrays of short-time energy, lightly smoothed."""
    import numpy as np

    w = max(1, int(win * sr))
    h = max(1, int(hop * sr))
    if len(audio) < w:
        return np.array([0.0]), np.array([0.0])
    n = 1 + (len(audio) - w) // h
    # Strided view -> RMS per frame without a Python loop.
    idx = np.arange(w)[None, :] + h * np.arange(n)[:, None]
    frames = audio[idx]
    rms = np.sqrt(np.mean(frames.astype(np.float64) ** 2, axis=1))
    # smooth over ~2 s to capture "a burst of noise", not single samples
    k = max(1, int(2.0 / hop))
    kernel = np.ones(k) / k
    rms_s = np.convolve(rms, kernel, mode="same")
    times = np.arange(n) * hop
    return times, rms_s


def _cue_times(words) -> list[float]:
    return [w.start for w in words if _LAUGH_RE.search(w.text)]


def detect_highlights(
    source: Path,
    count: int = 5,
    *,
    min_dur: float = 15.0,
    max_dur: float = 45.0,
    words=None,
) -> list[tuple[float, float]]:
    """Pick up to ``count`` non-overlapping highlight segments, best first.

    ``words`` is an optional list of ``subtitles.Word`` (absolute times) used to
    boost funny moments.  Returns ``(start, duration)`` tuples.
    """
    import numpy as np

    total = duration(source)
    if total <= min_dur:
        return [(0.0, total)] if total > 0 else []

    audio = _decode_mono(source)
    times, rms = _energy_windows(audio)
    if len(rms) == 0 or float(np.max(rms)) == 0.0:
        log("highlight: silent audio, falling back to even split")
        return _even_split(total, count, min_dur, max_dur)

    # Robust baseline: median + a high percentile mark the "hype" level.
    med = float(np.median(rms))
    hi = float(np.percentile(rms, 95)) or med or 1.0
    denom = max(hi - med, 1e-6)
    energy = np.clip((rms - med) / denom, 0.0, 1.5)

    cue_list = _cue_times(words) if words else []
    cue_arr = np.zeros_like(energy)
    if cue_list:
        for t in cue_list:
            i = int(round(t / 0.25))
            lo, hi_i = max(0, i - 4), min(len(cue_arr), i + 4)  # +-1 s
            cue_arr[lo:hi_i] += 1.0
        if cue_arr.max() > 0:
            cue_arr /= cue_arr.max()

    score = 0.75 * energy + 0.6 * cue_arr

    # Greedy peak picking with non-overlap suppression.
    order = np.argsort(score)[::-1]
    chosen: list[tuple[float, float]] = []
    used: list[tuple[float, float]] = []

    def overlaps(a0: float, a1: float) -> bool:
        return any(not (a1 <= b0 or a0 >= b1) for b0, b1 in used)

    for i in order:
        if len(chosen) >= count:
            break
        peak_t = float(times[i])
        # segment length grows with how "busy" the surrounding 3 s is
        around = score[max(0, i - 6):i + 6]
        heat = float(np.mean(around)) if len(around) else 0.0
        dur = min_dur + (max_dur - min_dur) * min(1.0, heat)
        start = peak_t - dur * 0.35
        start = max(0.0, min(start, total - dur))
        if start + dur > total:
            dur = total - start
        if dur < min_dur * 0.6:
            continue
        if overlaps(start, start + dur):
            continue
        used.append((start, start + dur))
        chosen.append((round(start, 2), round(dur, 2)))

    if not chosen:
        return _even_split(total, count, min_dur, max_dur)

    chosen.sort(key=lambda s: s[0])
    log(f"highlight: picked {len(chosen)} segment(s) from {len(times)} windows")
    for s, d in chosen:
        log(f"  {s:7.1f}s +{d:4.1f}s")
    return chosen


def _even_split(total: float, count: int, min_dur: float,
                max_dur: float) -> list[tuple[float, float]]:
    count = max(1, count)
    dur = min(max_dur, max(min_dur, total / count))
    step = total / count
    out = []
    for k in range(count):
        start = k * step
        out.append((round(start, 2), round(min(dur, total - start), 2)))
    return out
