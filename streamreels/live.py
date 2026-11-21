"""Live Twitch capture: resolve the HLS stream and keep a rolling ring buffer.

The live stream cannot be seeked backwards, so we continuously record it into
short independent segments (ffmpeg `segment` muxer).  A monitor thread tracks
each finished segment together with an approximate wall-clock end time, so we
can later cut a clip that starts a few seconds *before* an event we only
noticed after the fact (pre-roll).

Usage:
    buf = RingBuffer(url, workdir, segment_secs=4, keep_secs=120)
    buf.start()
    ...
    clip_path = buf.extract_clip(t0_wall, t1_wall, out_path)
    ...
    buf.stop()
"""
from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from .utils import ensure_dir, log, run, run_out, venv_python

# Quality selector for the live stream (720p60 is plenty for a 720x1280 reel).
_LIVE_FORMAT = "best[height<=720]/best"


def _die_with_parent() -> None:
    """preexec_fn: ask the kernel to SIGKILL this child if its parent dies.

    Uses PR_SET_PDEATHSIG (Linux).  This guarantees the ffmpeg recorder never
    outlives a killed/crashed/timeout-ed pipeline, which would otherwise keep
    writing segments and corrupt the next run's buffer.
    """
    try:
        import ctypes
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        PR_SET_PDEATHSIG = 1
        libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL, 0, 0, 0)
    except Exception:  # noqa: BLE001
        pass


def _quiet_duration(path: Path) -> float:
    """ffprobe duration without spewing stderr on a still-writing segment."""
    try:
        out = subprocess.check_output(
            ["ffprobe", "-v", "error", "-show_entries", "format=duration",
             "-of", "default=nw=1:nk=1", str(path)],
            text=True, stderr=subprocess.DEVNULL,
        )
        return float(out.strip())
    except Exception:  # noqa: BLE001
        return 0.0


def _seg_energy(path: Path, sr: int = 8000) -> float:
    """Mean RMS loudness of a segment's audio (0..~1), for hype detection.

    Cheap: decodes a few-second segment at low sample rate.  Returns 0.0 on
    any failure (missing audio track, still-writing file, etc.).
    """
    try:
        import numpy as np

        raw = subprocess.check_output(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(path),
             "-vn", "-ac", "1", "-ar", str(sr), "-f", "f32le", "-"],
            stderr=subprocess.DEVNULL,
        )
        if not raw:
            return 0.0
        a = np.frombuffer(raw, dtype=np.float32)
        if a.size == 0:
            return 0.0
        return float(np.sqrt(np.mean(a.astype(np.float64) ** 2)))
    except Exception:  # noqa: BLE001
        return 0.0



def channel_from_url(url: str) -> str:
    """Extract the Twitch channel/login from a URL or a bare channel name."""
    u = url.strip().rstrip("/")
    if "twitch.tv/" in u:
        tail = u.split("twitch.tv/", 1)[1]
        return tail.split("/")[0].split("?")[0].lower()
    return u.lower()


def resolve_live_url(url: str) -> str:
    """Resolve a Twitch channel/page URL to a direct HLS (.m3u8) URL."""
    log(f"resolving live stream: {url}")
    out = run_out(
        [venv_python(), "-m", "yt_dlp", "-g", "-f", _LIVE_FORMAT,
         "--no-warnings", url],
        stderr=subprocess.STDOUT,
    )
    lines = [ln.strip() for ln in out.splitlines() if ln.strip()]
    if not lines:
        raise RuntimeError("yt-dlp returned no stream URL (channel offline?)")
    m3u8 = lines[0]
    log(f"live HLS: {m3u8[:80]}...")
    return m3u8


# Upper bound on continuous evidence fed into the fused hype score: a signal at
# 4x its own trigger is already "maximally hot", so one very loud segment can't
# drown out the other signal and the fusion weights stay meaningful.
_MAX_EVIDENCE = 4.0


@dataclass
class Segment:
    index: int
    path: Path
    duration: float
    content_start: float  # cumulative content time (sec) where this seg starts
    content_end: float    # cumulative content time where it ends
    energy: float = 0.0   # mean RMS loudness (0..1-ish) over the segment


class RingBuffer:
    """Continuously record a live HLS stream into short segments."""

    def __init__(self, url: str, workdir: Path, segment_secs: float = 4.0,
                 keep_secs: float = 120.0):
        self.url = url
        self.workdir = ensure_dir(workdir)
        self.segment_secs = segment_secs
        self.keep_secs = keep_secs
        self._proc: subprocess.Popen | None = None
        self._monitor: threading.Thread | None = None
        self._rec_log = None
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._segments: list[Segment] = []
        self._seen: set[str] = set()
        self._content_cursor = 0.0  # cumulative content time of finalized segs
        self._start_wall = 0.0
        self._latency = 0.0  # recording delay (wall - content), estimated
        # Watchdog: ffmpeg can hang forever on a stale/expired HLS URL while
        # its process stays alive (blocked in futex).  Track when we last
        # finalized a segment; if nothing new arrives for too long, the
        # recorder is considered dead so the pipeline can restart it with a
        # freshly resolved URL.
        self._last_progress_wall = 0.0
        self._stall_secs = max(45.0, segment_secs * 8)

    # -- lifecycle ---------------------------------------------------------
    def start(self) -> None:
        m3u8 = resolve_live_url(self.url)
        ensure_dir(self.workdir)
        for old in self.workdir.glob("seg_*.mp4"):
            old.unlink(missing_ok=True)
        cmd = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning",
            "-i", m3u8,
            "-c", "copy",
            "-f", "segment",
            "-segment_time", f"{self.segment_secs:.3f}",
            "-reset_timestamps", "1",
            "-segment_format", "mp4",
            str(self.workdir / "seg_%05d.mp4"),
        ]
        log("ring buffer: starting ffmpeg recorder")
        # Keep the recorder's stderr in a file: when ffmpeg dies we need to
        # know *why* (stale HLS URL, network reset, disk full...).  DEVNULL
        # here used to leave us completely blind on failures.
        self._rec_log = (self.workdir.parent / "recorder.log").open("w")
        # Own session/process-group so stop() can reliably kill ffmpeg even if
        # it spawned helpers.  PDEATHSIG makes the kernel kill ffmpeg the
        # instant this process dies (even on SIGKILL / timeout), so a killed
        # run can never leave an orphan recorder behind.
        self._proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                                      stderr=self._rec_log,
                                      start_new_session=True,
                                      preexec_fn=_die_with_parent)
        self._last_progress_wall = time.time()
        self._monitor = threading.Thread(target=self._watch, daemon=True)
        self._monitor.start()

    def _watch(self) -> None:
        while not self._stop.is_set():
            self._scan()
            self._prune()
            time.sleep(0.5)

    def _scan(self) -> None:
        files = sorted(self.workdir.glob("seg_*.mp4"))
        if len(files) < 2:
            return
        # A segment is only finalized once ffmpeg has moved on to a newer one
        # (the segment muxer writes the mp4 'moov' atom on close).  The newest
        # file is still being written, so we never touch it.
        finalized = files[:-1]
        for f in finalized:
            name = f.name
            if name in self._seen:
                continue
            try:
                dur = _quiet_duration(f)
            except Exception:  # noqa: BLE001
                continue
            if dur <= 0:
                continue
            idx = int(name.replace("seg_", "").replace(".mp4", ""))
            energy = _seg_energy(f)
            with self._lock:
                seg = Segment(index=idx, path=f, duration=dur,
                              content_start=self._content_cursor,
                              content_end=self._content_cursor + dur,
                              energy=energy)
                self._content_cursor += dur
                self._seen.add(name)
                self._segments.append(seg)
                self._last_progress_wall = time.time()
            log(f"buffer: +seg {idx:05d} ({dur:.1f}s, e={energy:.3f}) "
                f"[content {seg.content_start:.1f}-{seg.content_end:.1f}]")

    def _prune(self) -> None:
        with self._lock:
            if not self._segments:
                return
            newest = self._segments[-1].content_end
            cutoff = newest - self.keep_secs
            keep: list[Segment] = []
            for seg in self._segments:
                if seg.content_end < cutoff:
                    seg.path.unlink(missing_ok=True)
                else:
                    keep.append(seg)
            self._segments = keep

    def snapshot(self) -> list[Segment]:
        with self._lock:
            return list(self._segments)

    def is_alive(self) -> bool:
        """True while the recorder is actually making progress.

        A live ffmpeg can hang forever on a stale/expired HLS URL while the
        process itself stays alive (blocked in futex, no new segments).  We
        therefore treat the recorder as dead once the process has exited OR
        no new segment has been finalized for ``_stall_secs``.
        """
        if self._proc is None or self._proc.poll() is not None:
            return False
        if self._last_progress_wall:
            stalled = time.time() - self._last_progress_wall
            if stalled > self._stall_secs:
                return False
        return True

    def content_time(self) -> float:
        """Total finalized content time recorded so far (seconds)."""
        with self._lock:
            return self._content_cursor

    def _noise_model(self, segs: list[Segment], newest: float,
                     lookback_secs: float, baseline_secs: float,
                     floor: float) -> tuple[float, float, float] | None:
        """Return (baseline_median, robust_sigma, silence_gate) for the window.

        ``robust_sigma`` is a MAD-based estimate of the channel's natural
        loudness fluctuation, but **capped** relative to the median: stream
        audio is heavy-tailed (quiet gameplay vs. loud talking), which inflates
        a raw MAD so much that a z-score threshold would miss real laughs.  A
        small lower bound keeps flat streams sensitive.

        ``silence_gate`` is the absolute level below which nothing counts as a
        burst.  It scales down for genuinely quiet channels instead of being a
        fixed constant (the old ``audio_floor``), but never below a tiny global
        minimum so digital silence cannot trigger.
        """
        import statistics

        base = [s.energy for s in segs
                if newest - baseline_secs <= s.content_end
                <= newest - lookback_secs]
        if not base:
            return None
        med = statistics.median(base)
        mad = statistics.median(abs(e - med) for e in base)
        sigma = 1.4826 * mad
        # Heavy-tail guard: sigma must not dwarf the median, or the z-score
        # threshold becomes unreachable.  Lower bound keeps quiet/flat streams
        # sensitive to real jumps.
        sigma = min(sigma, med * 0.6)
        sigma = max(sigma, med * 0.15, 0.002)
        gate = max(0.002, min(floor, med * 0.6))
        return med, sigma, gate

    def _audio_peak(self, lookback_secs: float, baseline_secs: float,
                    ratio: float, floor: float, adaptive: bool, k: float
                    ) -> tuple[Segment, float, float, float, float] | None:
        """Return ``(peak_segment, threshold, gate, base_median, sigma)``.

        Shared by ``energy_spike`` (hard trigger) and ``audio_evidence``
        (continuous ratio) so the two can never disagree about the model.
        """
        with self._lock:
            segs = list(self._segments)
        if len(segs) < 4:
            return None
        newest = segs[-1].content_end
        recent = [s for s in segs if s.content_end > newest - lookback_secs]
        model = self._noise_model(segs, newest, lookback_secs, baseline_secs,
                                  floor)
        if not recent or model is None:
            return None
        base_med, sigma, gate = model
        peak = max(recent, key=lambda s: s.energy)
        if adaptive:
            threshold = max(base_med + k * sigma, base_med * ratio, gate)
        else:
            threshold = max(base_med, floor) * ratio
        return peak, threshold, gate, base_med, sigma

    def energy_spike(self, lookback_secs: float = 6.0,
                     baseline_secs: float = 60.0,
                     ratio: float = 1.6,
                     floor: float = 0.012,
                     adaptive: bool = True,
                     k: float = 3.0) -> float | None:
        """Detect a loudness burst (laughter/shout/hype) in recent audio.

        Compares the loudest of the last ``lookback_secs`` of finalized
        segments against an older ``baseline_secs`` window.  Returns the
        *content-time* centre of the loud segment, or None.  Working in
        content time avoids the wall-clock mapping entirely.

        With ``adaptive`` (default) the trigger is a robust z-score
        ``peak > median + k * sigma`` with a MAD-based, heavy-tail-capped
        sigma, so the threshold self-calibrates to each channel's noise level.
        ``ratio`` and ``floor`` act as guardrails.  With ``adaptive=False`` it
        falls back to the old fixed ``peak >= max(median, floor) * ratio`` rule.
        """
        r = self._audio_peak(lookback_secs, baseline_secs, ratio, floor,
                             adaptive, k)
        if r is None:
            return None
        peak, threshold, gate, _med, _sigma = r
        if peak.energy > gate and peak.energy >= threshold:
            return (peak.content_start + peak.content_end) / 2.0
        return None

    def audio_evidence(self, lookback_secs: float = 6.0,
                       baseline_secs: float = 60.0,
                       ratio: float = 1.6,
                       floor: float = 0.012,
                       adaptive: bool = True,
                       k: float = 3.0
                       ) -> tuple[float | None, float,
                                  tuple[float, float, float]]:
        """Continuous loudness evidence normalised to the trigger threshold.

        Returns ``(content_center, evidence, (peak, median, sigma))`` where
        ``evidence`` measures the *excess over the baseline median*, scaled so
        that 0.0 == background (peak at the median), 1.0 == exactly at the
        trigger threshold, >1 above it.  Normalising against the median (not
        silence) keeps ordinary background loudness from contributing a
        constant offset to the fused score.  Silence below the gate gives 0.0.

        The live loop fuses this with chat evidence into one weighted score
        instead of a hard audio-OR-chat trigger.
        """
        r = self._audio_peak(lookback_secs, baseline_secs, ratio, floor,
                             adaptive, k)
        if r is None:
            return None, 0.0, (0.0, 0.0, 0.0)
        peak, threshold, gate, med, sigma = r
        stats = (peak.energy, med, sigma)
        if peak.energy <= gate:
            return None, 0.0, stats
        center = (peak.content_start + peak.content_end) / 2.0
        span = threshold - med
        if span <= 0:
            evidence = peak.energy / threshold if threshold > 0 else 0.0
        else:
            evidence = (peak.energy - med) / span
        # Cap so one very loud segment can't dominate the fused score (and the
        # signal weights stay meaningful); anything at 4x its trigger is "max".
        return center, min(max(0.0, evidence), _MAX_EVIDENCE), stats

    def stop(self) -> None:
        self._stop.set()
        if self._proc is not None and self._proc.poll() is None:
            try:
                # Kill the whole process group (start_new_session=True above).
                os.killpg(os.getpgid(self._proc.pid), signal.SIGTERM)
            except Exception:  # noqa: BLE001
                self._proc.terminate()
            try:
                self._proc.wait(timeout=5)
            except Exception:  # noqa: BLE001
                try:
                    os.killpg(os.getpgid(self._proc.pid), signal.SIGKILL)
                except Exception:  # noqa: BLE001
                    self._proc.kill()
        if self._monitor is not None:
            self._monitor.join(timeout=3)
        if self._rec_log is not None:
            try:
                self._rec_log.close()
            except Exception:  # noqa: BLE001
                pass
            self._rec_log = None
        log("ring buffer: stopped")

    # -- clip extraction ---------------------------------------------------
    def wait_for_content(self, target: float, timeout: float = 120.0) -> bool:
        """Block until `target` seconds of content have been finalized."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.content_time() >= target:
                return True
            time.sleep(0.3)
        return False

    def extract_clip(self, c0: float, c1: float,
                     out_path: Path) -> Path | None:
        """Cut the [c0, c1] window (content seconds) from the buffer.

        Waits for c1 to be recorded, then concatenates the covering segments
        and trims precisely by content offset.  Returns None if the window is
        not covered by the buffer (e.g. it predates the buffer or timed out).
        """
        if not self.wait_for_content(c1, timeout=max(60.0, (c1 - self.content_time()) + 60)):
            log(f"clip: timed out waiting for content {c1:.1f}s")
            return None
        segs = self.snapshot()
        if not segs:
            return None
        covering = [s for s in segs
                    if s.content_end > c0 and s.content_start < c1]
        if not covering:
            log(f"clip: no buffered segments cover content [{c0:.1f},{c1:.1f}]")
            return None
        covering.sort(key=lambda s: s.index)

        ensure_dir(out_path.parent)
        concat_list = out_path.with_suffix(".concat.txt")
        concat_list.write_text(
            "\n".join(f"file '{s.path.as_posix()}'" for s in covering) + "\n"
        )
        joined = out_path.with_suffix(".joined.mp4")
        try:
            run(["ffmpeg", "-hide_banner", "-loglevel", "error",
                 "-f", "concat", "-safe", "0", "-i", str(concat_list),
                 "-c", "copy", "-y", str(joined)])
            offset = max(0.0, c0 - covering[0].content_start)
            dur = max(0.5, c1 - c0)
            run(["ffmpeg", "-hide_banner", "-loglevel", "error",
                 "-ss", f"{offset:.3f}", "-t", f"{dur:.3f}",
                 "-i", str(joined), "-c", "copy", "-y", str(out_path)])
            return out_path
        finally:
            concat_list.unlink(missing_ok=True)
            joined.unlink(missing_ok=True)
