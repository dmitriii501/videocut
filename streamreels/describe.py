"""Auto-generate a title / description / hashtags for a reel.

Without an LLM key this is a heuristic over the transcript: the first clean
sentence becomes the title, the full (trimmed) transcript the description, and
a few salient words become hashtags.  If an OpenAI-compatible endpoint is
configured (STREAMREELS_LLM_API_KEY + optional STREAMREELS_LLM_URL /
STREAMREELS_LLM_MODEL) we ask the model instead and fall back on any error.
"""
from __future__ import annotations

import json
import os
import re

from .utils import log, retry

# Filler / non-content words to skip when guessing hashtags (ru + en).
_STOP = {
    "и", "в", "во", "не", "что", "он", "на", "я", "с", "со", "как", "а", "то",
    "все", "она", "так", "его", "но", "да", "ты", "к", "у", "же", "вы", "за",
    "бы", "по", "только", "ее", "мне", "было", "вот", "от", "меня", "еще",
    "нет", "о", "из", "ему", "теперь", "когда", "даже", "ну", "вдруг", "ли",
    "если", "уже", "или", "ни", "быть", "был", "него", "до", "вас", "нибудь",
    "опять", "уж", "вам", "ведь", "там", "потом", "себя", "ничего", "ей",
    "может", "они", "тут", "где", "есть", "надо", "ней", "для", "мы", "тебя",
    "их", "чем", "была", "сам", "чтоб", "без", "будто", "чего", "раз", "тоже",
    "себе", "под", "будет", "ж", "тогда", "кто", "этот", "того", "потому",
    "этого", "какой", "совсем", "ним", "здесь", "этом", "один", "почти",
    "мой", "тем", "чтобы", "нее", "сейчас", "были", "куда", "зачем", "всех",
    "никогда", "можно", "при", "наконец", "два", "об", "другой", "хоть",
    "после", "над", "больше", "тот", "через", "эти", "нас", "про", "всего",
    "них", "какая", "много", "разве", "три", "эту", "моя", "впрочем", "хорошо",
    "свою", "этой", "перед", "иногда", "лучше", "чуть", "том", "нельзя",
    "такой", "им", "более", "всегда", "конечно", "всю", "между",
    "the", "a", "an", "and", "or", "but", "to", "of", "in", "on", "is", "it",
    "this", "that", "for", "with", "as", "at", "be", "are", "was", "were",
    "so", "you", "i", "we", "they", "he", "she", "just", "like", "get",
}


def _clean(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def _sentences(text: str) -> list[str]:
    parts = re.split(r"(?<=[.!?…])\s+", text)
    return [p.strip() for p in parts if p.strip()]


def _hashtags(text: str, limit: int = 5) -> list[str]:
    seen: list[str] = []
    for w in re.findall(r"[A-Za-zА-Яа-яЁё0-9]{4,}", text.lower()):
        if w in _STOP or w in seen:
            continue
        seen.append(w)
        if len(seen) >= limit:
            break
    return [f"#{w}" for w in seen]


def _short_title(text: str, limit: int = 70) -> str:
    """First sentence, trimmed to `limit` chars on a word boundary."""
    sents = _sentences(text)
    cand = next((s for s in sents if len(s) >= 12), text)
    cand = cand.strip(" .,!…-")
    if len(cand) <= limit:
        return cand
    cut = cand[:limit].rsplit(" ", 1)[0]
    return cut.rstrip(" .,!…-") + "…"


def _heuristic(transcript: str, channel: str) -> dict:
    text = _clean(transcript)
    title = _short_title(text) or "Стрим-момент"
    desc = text[:400] if text else "Момент со стрима."
    # Hashtags: skip words already in the title to avoid repetition.
    title_words = {w.lower() for w in re.findall(r"[A-Za-zА-Яа-яЁё0-9]+", title)}
    body = " ".join(w for w in re.findall(r"\S+", text)
                    if w.strip(".,!?…").lower() not in title_words)
    tags = _hashtags(body)
    tags += ["#стрим", "#twitch"]
    if channel:
        ch = re.sub(r"[^A-Za-z0-9_]", "", channel)
        if ch:
            tags.append(f"#{ch}")
    # dedupe, keep order
    out_tags: list[str] = []
    for t in tags:
        if t.lower() not in {x.lower() for x in out_tags}:
            out_tags.append(t)
    return {"title": title, "description": desc, "hashtags": out_tags}


def _llm(transcript: str, channel: str) -> dict | None:
    key = os.environ.get("STREAMREELS_LLM_API_KEY") or os.environ.get("OPENAI_API_KEY")
    if not key:
        return None
    base = os.environ.get("STREAMREELS_LLM_URL", "https://api.openai.com/v1")
    model = os.environ.get("STREAMREELS_LLM_MODEL", "gpt-4o-mini")
    prompt = (
        "Ты — редактор коротких вертикальных видео (Reels/Shorts/TikTok). "
        "По транскрипту момента со стрима придумай цепляющий заголовок (до 80 "
        "символов), описание (до 300 символов) и 3-6 хештегов. "
        "Ответь строго JSON: {\"title\":...,\"description\":...,\"hashtags\":[...]}.\n\n"
        f"Канал: {channel}\nТранскрипт:\n{transcript[:3000]}"
    )
    payload = json.dumps({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0.7,
    }).encode()

    def _call():
        import subprocess
        out = subprocess.check_output(
            ["curl", "-sS", "-m", "60", "-X", "POST", f"{base}/chat/completions",
             "-H", f"Authorization: Bearer {key}",
             "-H", "Content-Type: application/json",
             "--data-binary", "@-"],
            input=payload,
        )
        data = json.loads(out)
        content = data["choices"][0]["message"]["content"]
        m = re.search(r"\{.*\}", content, re.S)
        return json.loads(m.group(0) if m else content)

    try:
        res = retry(_call, attempts=3, delay=2.0, what="llm describe")
        if isinstance(res, dict) and res.get("title"):
            res.setdefault("description", "")
            res.setdefault("hashtags", [])
            return res
    except Exception as exc:  # noqa: BLE001
        log(f"describe: LLM failed ({exc}); using heuristic")
    return None


def describe(transcript: str, channel: str = "") -> dict:
    """Return {'title', 'description', 'hashtags'} for the given transcript."""
    res = _llm(transcript, channel)
    if res is None:
        res = _heuristic(transcript, channel)
    return res


def as_caption(info: dict, max_len: int = 1000) -> str:
    """Render description info into a Telegram caption (<= max_len)."""
    title = info.get("title", "").strip()
    desc = info.get("description", "").strip()
    tags = " ".join(info.get("hashtags", []))
    caption = f"{title}\n\n{desc}\n\n{tags}".strip()
    if len(caption) > max_len:
        caption = caption[: max_len - 1].rstrip() + "…"
    return caption


def as_text_file(info: dict) -> str:
    return (
        f"# {info.get('title', '')}\n\n"
        f"{info.get('description', '')}\n\n"
        f"{' '.join(info.get('hashtags', []))}\n"
    )
