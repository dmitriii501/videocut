"""Minimal Telegram Bot API client (sendVideo) over curl.

Network here is flaky (TLS resets), so every call is wrapped in retry().
Configure with env STREAMREELS_TG_TOKEN / STREAMREELS_TG_CHAT_ID, or pass
explicitly.  If no chat_id is configured we try to discover it from getUpdates
(the user must have sent the bot at least one message).
"""
from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

from .utils import log, retry

def _token() -> str:
    tok = os.environ.get("STREAMREELS_TG_TOKEN")
    if not tok:
        raise RuntimeError(
            "telegram: set STREAMREELS_TG_TOKEN (bot token from @BotFather); "
            "it is intentionally not stored in the repo")
    return tok


def _api(method: str) -> str:
    return f"https://api.telegram.org/bot{_token()}/{method}"


def _curl_json(args: list[str], timeout: int = 60) -> dict:
    out = subprocess.check_output(
        ["curl", "-sS", "-m", str(timeout), *args], text=True,
    )
    data = json.loads(out)
    if not data.get("ok"):
        raise RuntimeError(f"telegram error: {data}")
    return data


def get_updates(offset: int | None = None, timeout: int = 50) -> list[dict]:
    """Long-poll getUpdates. Returns the raw update dicts (possibly empty)."""
    args = ["-F", f"timeout={timeout}"]
    if offset is not None:
        args += ["-F", f"offset={offset}"]
    # curl must outlive the long poll (server holds the request `timeout` secs).
    data = retry(lambda: _curl_json([*args, _api("getUpdates")],
                                    timeout=timeout + 20),
                 attempts=5, delay=1.5, what="tg getUpdates")
    return data.get("result", [])


def send_message(text: str, chat: int | None = None,
                 timeout: float = 30.0) -> bool:
    """Send a plain text message. Returns True on success."""
    cid = chat if chat is not None else chat_id()
    if cid is None:
        log("telegram: no chat_id (send the bot a message first, or set "
            "STREAMREELS_TG_CHAT_ID)")
        return False

    def _call():
        return _curl_json([
            "-F", f"chat_id={cid}",
            "-F", f"text={text}",
            "-F", "disable_web_page_preview=true",
            _api("sendMessage"),
        ])

    try:
        retry(_call, attempts=3, delay=2.0, what="telegram sendMessage")
        return True
    except Exception as exc:  # noqa: BLE001
        log(f"telegram: sendMessage failed: {exc}")
        return False


def discover_chat_id() -> int | None:
    """Return the chat_id of the most recent message sent to the bot."""
    try:
        data = retry(lambda: _curl_json([_api("getUpdates")]),
                     attempts=4, delay=2.0, what="tg getUpdates")
        msgs = [u for u in data.get("result", []) if "message" in u]
        if msgs:
            return int(msgs[-1]["message"]["chat"]["id"])
    except Exception as exc:  # noqa: BLE001
        log(f"telegram: getUpdates failed: {exc}")
    return None


def chat_id() -> int | None:
    raw = os.environ.get("STREAMREELS_TG_CHAT_ID")
    if raw:
        try:
            return int(raw)
        except ValueError:
            pass
    return discover_chat_id()


def send_video(video: Path, caption: str = "", chat: int | None = None,
               timeout: float = 120.0) -> bool:
    """Upload a video with a caption. Returns True on success."""
    cid = chat if chat is not None else chat_id()
    if cid is None:
        log("telegram: no chat_id (send the bot a message first, or set "
            "STREAMREELS_TG_CHAT_ID)")
        return False

    def _call():
        return _curl_json([
            "-F", f"chat_id={cid}",
            "-F", f"video=@{video}",
            "-F", f"caption={caption}",
            "-F", "supports_streaming=true",
            _api("sendVideo"),
        ])

    try:
        retry(_call, attempts=4, delay=3.0, what="telegram sendVideo")
        log(f"telegram: sent {video.name} -> chat {cid}")
        return True
    except Exception as exc:  # noqa: BLE001
        log(f"telegram: send failed: {exc}")
        return False
