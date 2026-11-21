"""Translate short subtitle phrases into another language.

Uses the free MyMemory API (no key, ~5k chars/day for anonymous callers).
Results are cached on disk so repeated phrases cost nothing.  Any failure
(quota, network, empty response) falls back to the original text so a reel
still renders -- subtitles are just left untranslated instead of breaking the
run.
"""
from __future__ import annotations

import html
import json
import os
import re
import subprocess
import urllib.parse

from . import config
from .utils import log, retry

_CACHE: dict[tuple[str, str, str], str] = {}
_CACHE_LOADED = False

# MyMemory wants a plain ISO code; these are the ones we expect to use.
_SUPPORTED = {"ru", "en", "uk", "es", "de", "fr", "pt", "it", "pl", "tr"}

_CYRILLIC = re.compile(r"[\u0400-\u04ff]")
_LATIN = re.compile(r"[A-Za-z]")


def _looks_like(text: str, lang: str) -> bool:
    """Cheap script check: avoid translating ru->ru or en->ru on English text."""
    if lang == "ru" or lang == "uk":
        return bool(_CYRILLIC.search(text)) and not _LATIN.search(text)
    if lang == "en":
        return bool(_LATIN.search(text)) and not _CYRILLIC.search(text)
    return False


def _cache_file(source: str, target: str):
    return config.CACHE / "translate" / f"{source}-{target}.json"


def _load_cache(source: str, target: str) -> None:
    global _CACHE_LOADED
    if _CACHE_LOADED:
        return
    _CACHE_LOADED = True
    path = _cache_file(source, target)
    try:
        if path.exists():
            data = json.loads(path.read_text(encoding="utf-8"))
            for k, v in data.items():
                _CACHE[(source, target, k)] = v
            log(f"translate: loaded {len(data)} cached phrase(s) ({source}->{target})")
    except Exception:  # noqa: BLE001
        pass


def _save_cache(source: str, target: str) -> None:
    path = _cache_file(source, target)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        data = {k[2]: v for k, v in _CACHE.items()
                if k[0] == source and k[1] == target}
        path.write_text(json.dumps(data, ensure_ascii=False, indent=0),
                        encoding="utf-8")
    except Exception:  # noqa: BLE001
        pass


def _split(text: str, limit: int = 450) -> list[str]:
    """Split into <=limit-char chunks on word boundaries (MyMemory caps length)."""
    if len(text) <= limit:
        return [text]
    chunks: list[str] = []
    cur = ""
    for word in text.split():
        if cur and len(cur) + 1 + len(word) > limit:
            chunks.append(cur)
            cur = word
        else:
            cur = f"{cur} {word}".strip()
    if cur:
        chunks.append(cur)
    return chunks


def _mymemory(text: str, source: str, target: str) -> str:
    q = urllib.parse.quote(text)
    url = (f"https://api.mymemory.translated.net/get?q={q}"
           f"&langpair={source}|{target}")
    out = subprocess.check_output(["curl", "-sS", "-m", "30", url])
    data = json.loads(out)
    if data.get("quotaFinished"):
        raise RuntimeError("MyMemory daily quota finished")
    status = data.get("responseStatus")
    rd = data.get("responseData") or {}
    translated = (rd.get("translatedText") or "").strip()
    if status not in (200, "200", None) and not translated:
        raise RuntimeError(f"MyMemory status {status}: {data.get('responseDetails')}")
    if not translated:
        raise RuntimeError("empty translation")
    if translated.upper().startswith(("MYMEMORY WARNING", "PLEASE SELECT",
                                      "QUERY LENGTH LIMIT")):
        raise RuntimeError(translated[:80])
    return html.unescape(translated)


def translate_words(words, target: str = "ru", source: str = "en",
                    per_caption: int | None = None):
    """Translate a Word list caption-group by caption-group.

    Words are grouped the same way the subtitle builder groups them (pause >
    0.6s or ``per_caption`` words).  Each group's phrase is translated as a
    whole (far better than word-by-word) and the translated tokens are spread
    evenly across the group's time span, so per-word highlighting still works.
    """
    from .subtitles import Word

    if per_caption is None:
        per_caption = config.SUBTITLE_WORDS_PER_CAPTION
    groups: list[list] = []
    cur: list = []
    for w in words:
        if cur and (w.start - cur[-1].end > 0.6):
            groups.append(cur)
            cur = []
        cur.append(w)
        if len(cur) >= per_caption:
            groups.append(cur)
            cur = []
    if cur:
        groups.append(cur)

    out = []
    for grp in groups:
        phrase = " ".join(w.text for w in grp).strip()
        translated = translate_text(phrase, target=target, source=source)
        tokens = translated.split()
        if not tokens:
            out.extend(grp)
            continue
        g_start = grp[0].start
        g_end = max(grp[-1].end, g_start + 0.05)
        span = g_end - g_start
        step = span / len(tokens)
        conf = sum(getattr(w, "confidence", 1.0) for w in grp) / len(grp)
        for i, tok in enumerate(tokens):
            out.append(Word(g_start + i * step, g_start + (i + 1) * step,
                            tok, conf))
    return out


def translate_text(text: str, target: str = "ru", source: str = "en") -> str:
    """Translate ``text`` from ``source`` to ``target`` (falls back to input)."""
    text = (text or "").strip()
    if not text:
        return ""
    if target not in _SUPPORTED:
        return text
    if target == source or _looks_like(text, target):
        return text
    _load_cache(source, target)
    key = (source, target, text)
    if key in _CACHE:
        return _CACHE[key]

    parts: list[str] = []
    for chunk in _split(text):
        ck = (source, target, chunk)
        if ck in _CACHE:
            parts.append(_CACHE[ck])
            continue
        try:
            res = retry(lambda c=chunk: _mymemory(c, source, target),
                        attempts=3, delay=2.0, what="translate")
        except Exception as exc:  # noqa: BLE001
            log(f"translate: failed ({exc}); keeping original phrase")
            return text
        _CACHE[ck] = res
        parts.append(res)
    result = " ".join(parts)
    _CACHE[key] = result
    _save_cache(source, target)
    return result
