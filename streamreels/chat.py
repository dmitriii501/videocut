"""Anonymous Twitch IRC chat monitor with activity-spike detection.

We join the channel anonymously (Twitch allows a read-only connection with the
`justinfan` nickname and no OAuth), count messages in short windows, and emit an
event when the recent rate spikes well above the recent baseline.  Chat usually
reacts *before* the funny moment is over, which is why the caller adds a
pre-roll when cutting the clip.

No external deps: plain sockets.
"""
from __future__ import annotations

import collections
import random
import re
import socket
import threading
import time
from dataclasses import dataclass

from .utils import log

_IRC_HOST = "irc.chat.twitch.tv"
_IRC_PORT = 6667

# Emotes that signal a genuinely funny / hyped moment.  A message carrying one
# of these counts for more than a plain "hi" - chat spamming KEKW/LUL is a much
# stronger reaction than an equal number of neutral messages.
_HYPE_EMOTES = {
    "kekw", "kekwa", "lul", "lulw", "omegalul", "omegalulw", "pog", "pogchamp",
    "pogu", "poggers", "poggg", "kappa", "monkas", "pepelaugh", "pepehands",
    "pepega", "icant", "ez", "clap", "4head", "hahaa", "sadge", "biblethump",
    "catjam", "weirdchamp", "jebaited", "residentsleeper", "hypers", "lmaooo",
    "xdd", "lulul",
}
# Cap on continuous evidence fed into the fused hype score (mirror of live.py).
_MAX_EVIDENCE = 4.0

# Laughter in any spelling: ru "хаха/ахах", en "haha/hehe", lol/lmao/kek/xD/:D.
_LAUGH_RE = re.compile(
    r"(?:ха){2,}|(?:ах){2,}|(?:h[ae]){2,}|lol|lmao|kek|x[dд]{1,}|:d\b",
    re.IGNORECASE,
)


def _weight(text: str) -> float:
    """Weight of a single chat message as a hype signal (>=1.0).

    Plain message = 1.0.  Each hype emote adds +0.5 (capped), laughter +1.0,
    all-caps shouting +0.5.  Total capped at 4.0 so one spammy user can't
    dominate the whole window.
    """
    if not text:
        return 1.0
    w = 1.0
    hype = 0
    for tok in text.split():
        clean = tok.strip("!?.,;:()[]\"'").lower()
        if clean in _HYPE_EMOTES:
            hype += 1
    if hype:
        w += min(hype, 4) * 0.5
    if _LAUGH_RE.search(text):
        w += 1.0
    letters = [c for c in text if c.isalpha()]
    if len(letters) >= 4 and all(c.isupper() for c in letters):
        w += 0.5
    return min(w, 4.0)


@dataclass
class Spike:
    wall: float          # wall-clock time the spike was detected
    rate: float          # weighted msgs/sec in the short window
    baseline: float      # baseline weighted msgs/sec
    count: int = 0       # raw messages in the short window
    weight: float = 1.0  # average weight per message (hype intensity)


class ChatMonitor:
    """Read-only Twitch IRC client that detects chat activity spikes."""

    def __init__(self, channel: str, spike_ratio: float = 2.5,
                 min_rate: float = 1.5, short_secs: float = 5.0,
                 baseline_secs: float = 60.0, cooldown: float = 30.0):
        self.channel = channel.lower().lstrip("#")
        self.spike_ratio = spike_ratio
        self.min_rate = min_rate
        self.short_secs = short_secs
        self.baseline_secs = baseline_secs
        self.cooldown = cooldown

        self._sock: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._lock = threading.Lock()

        # (timestamp, weight) of recent messages, weight >1 for hype messages.
        self._weighted: collections.deque[tuple[float, float]] = (
            collections.deque(maxlen=20000)
        )
        # (timestamp, text) of recent messages, for emote / laughter detection.
        self._messages: collections.deque[tuple[float, str]] = (
            collections.deque(maxlen=20000)
        )
        self._last_spike = 0.0
        self._total = 0

    # -- lifecycle ---------------------------------------------------------
    def start(self) -> None:
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        try:
            if self._sock is not None:
                self._sock.close()
        except Exception:  # noqa: BLE001
            pass
        if self._thread is not None:
            self._thread.join(timeout=3)
        log("chat: stopped")

    @property
    def total_messages(self) -> int:
        return self._total

    # -- networking --------------------------------------------------------
    def _connect(self) -> socket.socket:
        sock = socket.create_connection((_IRC_HOST, _IRC_PORT), timeout=30)
        nick = f"justinfan{random.randint(10000, 99999)}"
        sock.sendall(b"PASS SCHMOOPIIE\r\n")
        sock.sendall(f"NICK {nick}\r\n".encode())
        sock.sendall(b"CAP REQ :twitch.tv/tags twitch.tv/commands\r\n")
        sock.sendall(f"JOIN #{self.channel}\r\n".encode())
        sock.settimeout(60)
        log(f"chat: joined #{self.channel} as {nick}")
        return sock

    def _run(self) -> None:
        backoff = 2.0
        while not self._stop.is_set():
            try:
                self._sock = self._connect()
                backoff = 2.0
                buf = b""
                while not self._stop.is_set():
                    try:
                        data = self._sock.recv(4096)
                    except socket.timeout:
                        # Keep the connection alive; Twitch also sends PING.
                        try:
                            self._sock.sendall(b"PING :tmi.twitch.tv\r\n")
                        except Exception:  # noqa: BLE001
                            break
                        continue
                    if not data:
                        break
                    buf += data
                    while b"\r\n" in buf:
                        line, buf = buf.split(b"\r\n", 1)
                        self._handle_line(line.decode("utf-8", "replace"))
            except Exception as exc:  # noqa: BLE001
                if self._stop.is_set():
                    break
                log(f"chat: connection error ({exc}); reconnecting in "
                    f"{backoff:.0f}s")
                time.sleep(backoff)
                backoff = min(backoff * 2, 30.0)

    def _handle_line(self, line: str) -> None:
        if line.startswith("PING"):
            try:
                self._sock.sendall(b"PONG :tmi.twitch.tv\r\n")
            except Exception:  # noqa: BLE001
                pass
            return
        if " PRIVMSG " in line:
            now = time.time()
            text = ""
            try:
                text = line.split(" PRIVMSG ", 1)[1].split(" :", 1)[1]
            except Exception:  # noqa: BLE001
                text = ""
            with self._lock:
                self._weighted.append((now, _weight(text)))
                self._messages.append((now, text))
                self._total += 1

    # -- spike detection ---------------------------------------------------
    def _windows(self, now: float) -> tuple[list[tuple[float, float]],
                                            list[tuple[float, float]]]:
        """Return (recent, baseline) weighted windows under the lock."""
        with self._lock:
            recent = [(t, w) for t, w in self._weighted
                      if now - t <= self.short_secs]
            baseline_window = [(t, w) for t, w in self._weighted
                               if now - self.baseline_secs <= t
                               <= now - self.short_secs]
        return recent, baseline_window

    def rate(self, now: float | None = None) -> tuple[float, float]:
        """Return (current_rate, baseline_rate) in weighted msgs/sec.

        Pure read: no cooldown side effects.  Weights up hype emotes / laughter
        so an equal number of KEKW messages scores higher than neutral chatter.
        """
        now = now or time.time()
        recent, baseline_window = self._windows(now)
        cur = sum(w for _, w in recent) / self.short_secs
        base = (sum(w for _, w in baseline_window)
                / (self.baseline_secs - self.short_secs)
                if baseline_window else 0.0)
        return cur, base

    def evidence(self, now: float | None = None) -> tuple[float, float, float]:
        """Continuous chat evidence normalised to the spike threshold.

        Returns ``(evidence, rate, baseline)`` where ``evidence`` measures the
        *excess over the baseline*, scaled so that 0.0 == calm chatter at the
        baseline, 1.0 == exactly the hard-spike level, >1 hotter.  Normalising
        against the baseline (not zero) keeps a busy-but-normal chat from
        adding a constant offset to the fused score.

        Pure read: no cooldown side effects, safe to poll every tick for
        weighted fusion with the audio signal.
        """
        now = now or time.time()
        recent, baseline_window = self._windows(now)
        rate = sum(w for _, w in recent) / self.short_secs
        baseline = (sum(w for _, w in baseline_window)
                    / (self.baseline_secs - self.short_secs)
                    if baseline_window else 0.0)
        trigger = max(self.min_rate, baseline * self.spike_ratio)
        span = trigger - baseline
        if span > 0:
            evidence = (rate - baseline) / span
        else:
            evidence = rate / trigger if trigger > 0 else 0.0
        return min(max(0.0, evidence), _MAX_EVIDENCE), rate, baseline

    def check_spike(self, now: float | None = None) -> Spike | None:
        """Return a Spike if the current chat activity is anomalous, else None."""
        now = now or time.time()
        recent, baseline_window = self._windows(now)
        if not recent:
            return None
        rate = sum(w for _, w in recent) / self.short_secs
        baseline = (sum(w for _, w in baseline_window)
                    / (self.baseline_secs - self.short_secs)
                    if baseline_window else 0.0)

        if now - self._last_spike < self.cooldown:
            return None
        if rate < self.min_rate:
            return None
        # Spike if rate is well above baseline, or there is chat where there
        # was basically none before.
        if baseline > 0 and rate < baseline * self.spike_ratio:
            return None
        if baseline == 0 and rate < self.min_rate:
            return None

        count = len(recent)
        avg_weight = sum(w for _, w in recent) / count if count else 1.0
        self._last_spike = now
        return Spike(wall=now, rate=rate, baseline=baseline,
                     count=count, weight=avg_weight)

    def wait_for_spike(self, poll: float = 0.5) -> Spike | None:
        """Block until a spike is detected or the monitor is stopped."""
        while not self._stop.is_set():
            spike = self.check_spike()
            if spike is not None:
                return spike
            time.sleep(poll)
        return None
