"""Download sources (YouTube/Twitch) and background gameplay clips."""
from __future__ import annotations

import random
import sys
from pathlib import Path

from . import config
from .utils import (duration, ensure_dir, log, retry, run_out, short_hash,
                    video_size)

# Queries used to auto-fetch distracting gameplay for the bottom strip.
DEFAULT_GAMEPLAY_QUERIES = [
    "subway surfers gameplay no commentary",
    "minecraft parkour gameplay no commentary",
    "gta 5 driving gameplay no commentary",
    "satisfying slime asmr",
    "hydraulic press satisfying",
]

# Populated from CLI (see cli.py) before any download happens.
OPTS: dict = {
    "cookies": None,          # path to cookies.txt
    "cookies_from_browser": None,  # e.g. "firefox", "chromium"
    "js_runtime": None,       # e.g. "node"
    "remote_components": False,
}


def _yt_dlp() -> list[str]:
    """yt-dlp lives in the venv; run it via the current interpreter."""
    return [sys.executable, "-m", "yt_dlp"]


def _common_args() -> list[str]:
    args = [
        "--no-playlist",
        "--no-progress",
        "--retries", "10",
        "--fragment-retries", "10",
        "--socket-timeout", "30",
        "--merge-output-format", "mp4",
        "--remux-video", "mp4",
    ]
    if OPTS.get("cookies"):
        args += ["--cookies", str(OPTS["cookies"])]
    elif OPTS.get("cookies_from_browser"):
        args += ["--cookies-from-browser", str(OPTS["cookies_from_browser"])]
    if OPTS.get("js_runtime"):
        args += ["--js-runtimes", str(OPTS["js_runtime"])]
    if OPTS.get("remote_components"):
        args += ["--remote-components", "ejs:github"]
    return args


def _download(url_or_query: str, outdir: Path, *, max_height: int,
              is_search: bool = False, limit: int = 6) -> Path:
    """Download a single video (or best search result) capped at max_height."""
    ensure_dir(outdir)

    def attempt() -> Path:
        before = set(outdir.glob("*.mp4"))
        target = f"ytsearch{limit}:{url_or_query}" if is_search else url_or_query
        # -S sorts/selects formats: prefer mp4+m4a at <= max_height, fall back gracefully.
        sort = f"res:{max_height},ext:mp4:m4a"
        cmd = [
            *_yt_dlp(),
            "-S", sort,
            "--format-sort-force",
            *_common_args(),
            "--output", str(outdir / "%(title).80s-%(id)s.%(ext)s"),
            "--print", "after_move:filepath",
            target,
        ]
        out = run_out(cmd)
        lines = [ln.strip() for ln in out.splitlines() if ln.strip().endswith(".mp4")]
        if lines:
            return Path(lines[-1])
        after = set(outdir.glob("*.mp4")) - before
        if after:
            return sorted(after, key=lambda p: p.stat().st_mtime)[-1]
        found = sorted(outdir.glob("*.mp4"), key=lambda p: p.stat().st_mtime)
        if found:
            return found[-1]
        raise RuntimeError(f"yt-dlp produced no mp4 for {target!r}")

    try:
        return retry(attempt, attempts=5, delay=3.0,
                     what=f"download {url_or_query[:50]}")
    except Exception as exc:  # noqa: BLE001
        msg = str(exc)
        if "not a bot" in msg or "403" in msg or "Sign in" in msg:
            raise RuntimeError(
                "YouTube заблокировал загрузку (bot-check / 403).\n"
                "  Решение: авторизуйтесь в YouTube в браузере и передайте куки:\n"
                "    --cookies-from-browser firefox   (или chromium/brave/edge)\n"
                "  либо экспортируйте cookies.txt и укажите:\n"
                "    --cookies /path/to/cookies.txt\n"
                "  Также помогает: --js-runtime node --remote-components\n"
                "  Альтернатива: скачайте видео вручную и передайте локальный файл "
                "как источник (см. --local)."
            ) from exc
        raise


def fetch_source(url: str, work: Path) -> Path:
    """Download the stream VOD from YouTube/Twitch capped at MAX_SOURCE_HEIGHT."""
    key = short_hash(url)
    outdir = ensure_dir(config.CACHE / "sources")
    for existing in outdir.glob(f"*{key}*.mp4"):
        if existing.stat().st_size > 0:
            log(f"reusing cached source: {existing}")
            return existing
    log(f"downloading source: {url}")
    path = _download(url, outdir, max_height=config.MAX_SOURCE_HEIGHT)
    log(f"source ready: {path} ({duration(path):.0f}s, {video_size(path)})")
    return path


def _cache_manifest(gameplay_dir: Path) -> list[Path]:
    ensure_dir(gameplay_dir)
    return sorted(gameplay_dir.glob("*.mp4"))


def ensure_gameplay(
    queries: list[str] | None,
    count: int,
    min_seconds: float,
    work: Path,
) -> list[Path]:
    """Make sure we have at least `count` cached gameplay clips of >= min_seconds."""
    gameplay_dir = ensure_dir(config.CACHE / "gameplay")
    queries = queries or DEFAULT_GAMEPLAY_QUERIES
    have = [p for p in _cache_manifest(gameplay_dir) if duration(p) >= min_seconds]
    if len(have) >= count:
        log(f"gameplay cache ok: {len(have)} clip(s)")
        return have[:count]

    rng = random.Random(work.name)
    pool = queries[:]
    rng.shuffle(pool)
    for q in pool:
        if len(have) >= count:
            break
        try:
            clip = _download(q, gameplay_dir, max_height=config.MAX_SOURCE_HEIGHT,
                             is_search=True)
            if duration(clip) >= min_seconds:
                have.append(clip)
                log(f"gameplay +1: {clip.name} ({duration(clip):.0f}s)")
            else:
                log(f"gameplay clip too short, ignored: {clip.name}")
        except Exception as exc:  # noqa: BLE001
            log(f"gameplay query failed ({q!r}): {exc}")
    if not have:
        raise RuntimeError(
            "не удалось скачать ни одного геймплей-клипа; "
            "передайте локальный файл через --gameplay"
        )
    return have[:count]


def local_gameplay(path: str | Path) -> Path:
    p = Path(path)
    if not p.exists():
        raise FileNotFoundError(f"gameplay file not found: {p}")
    return p
