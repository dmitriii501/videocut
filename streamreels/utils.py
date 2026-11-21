"""Small helpers: logging, subprocess, ffprobe, retries, hashing."""
from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
import time
from pathlib import Path


def log(msg: str) -> None:
    print(f"[streamreels] {msg}", flush=True)


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    log("run: " + " ".join(str(c) for c in cmd))
    return subprocess.run([str(c) for c in cmd], check=True, **kw)


def run_out(cmd: list[str], **kw) -> str:
    return subprocess.check_output([str(c) for c in cmd], text=True, **kw)


def retry(fn, attempts: int = 6, delay: float = 2.0, what: str = ""):
    last = None
    for i in range(attempts):
        try:
            return fn()
        except Exception as exc:  # noqa: BLE001 - broad on purpose for flaky net
            last = exc
            log(f"retry {i + 1}/{attempts} {what}: {exc}")
            if i < attempts - 1:
                time.sleep(delay * (i + 1))
    assert last is not None
    raise last


def probe(path: Path | str) -> dict:
    out = subprocess.check_output(
        [
            "ffprobe", "-v", "error", "-print_format", "json",
            "-show_format", "-show_streams", str(path),
        ],
        text=True,
    )
    return json.loads(out)


def duration(path: Path | str) -> float:
    try:
        return float(probe(path)["format"]["duration"])
    except Exception:  # noqa: BLE001
        return 0.0


def video_size(path: Path | str) -> tuple[int, int]:
    for s in probe(path)["streams"]:
        if s.get("codec_type") == "video":
            return int(s["width"]), int(s["height"])
    raise RuntimeError(f"no video stream in {path}")


def has_audio(path: Path | str) -> bool:
    try:
        return any(s.get("codec_type") == "audio" for s in probe(path)["streams"])
    except Exception:  # noqa: BLE001
        return False


def slugify(text: str, maxlen: int = 60) -> str:
    s = re.sub(r"[^a-zA-Z0-9]+", "-", text).strip("-").lower()
    return (s or "x")[:maxlen]


def short_hash(text: str) -> str:
    return hashlib.sha1(text.encode("utf-8")).hexdigest()[:12]


def parse_timecode(value: str) -> float:
    """Accept 90, 1:30, 01:02:03.5 -> seconds."""
    value = value.strip()
    if not value:
        raise ValueError("empty timecode")
    parts = value.split(":")
    try:
        nums = [float(p) for p in parts]
    except ValueError as exc:
        raise ValueError(f"bad timecode: {value!r}") from exc
    if len(nums) == 1:
        return nums[0]
    if len(nums) == 2:
        return nums[0] * 60 + nums[1]
    if len(nums) == 3:
        return nums[0] * 3600 + nums[1] * 60 + nums[2]
    raise ValueError(f"bad timecode: {value!r}")


def parse_segment(text: str) -> tuple[float, float]:
    """Parse 'start-end' or 'start+duration' (any side timecode) -> (start, duration)."""
    text = text.strip()
    if not text:
        raise ValueError("empty segment")
    if "-" in text:
        a, b = text.split("-", 1)
        start = parse_timecode(a)
        end = parse_timecode(b)
        dur = end - start
    elif "+" in text:
        a, b = text.split("+", 1)
        start = parse_timecode(a)
        dur = parse_timecode(b)
    else:
        raise ValueError(f"segment must be 'start-end' or 'start+dur': {text!r}")
    if dur <= 0:
        raise ValueError(f"non-positive duration in segment: {text!r}")
    return start, dur


def fmt_ts(seconds: float) -> str:
    seconds = max(0.0, seconds)
    h = int(seconds // 3600)
    m = int((seconds % 3600) // 60)
    s = seconds % 60
    return f"{h:02d}:{m:02d}:{s:05.2f}"


def ensure_dir(path: Path | str) -> Path:
    p = Path(path)
    p.mkdir(parents=True, exist_ok=True)
    return p


def check_ffmpeg() -> None:
    for name in ("ffmpeg", "ffprobe"):
        try:
            subprocess.run([name, "-version"], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except Exception as exc:  # noqa: BLE001
            raise RuntimeError(f"{name} not found in PATH") from exc


def venv_python() -> str:
    return sys.executable
