"""Telegram bot: accept a stream/VOD URL and send back finished reels.

Design
------
* Long-polls ``getUpdates`` (no webhook needed) on the main thread.
* All heavy work runs in a **single** worker thread consuming a queue, so only
  one whisper job is ever in flight (this box OOMs with two in parallel).
* A message that contains a URL is classified:
    - Twitch channel (``twitch.tv/<name>``, no ``/videos/``) -> live mode:
      watch the stream, cut ``max_reels`` reels, stream them as they finish.
    - Twitch VOD / clip / YouTube -> VOD mode: auto-detect highlights and cut.
* ``/stop`` aborts the current live job (sets a threading.Event that
  ``run_live`` polls through ``LiveJob.should_stop``).
* ``/status`` reports what the worker is doing.

Auth: only chats in ``STREAMREELS_TG_ALLOWED`` (comma ids), or the configured
``STREAMREELS_TG_CHAT_ID``, are served.  If neither is set the bot accepts the
first chat that talks to it and locks onto it (logged), to avoid strangers
triggering CPU-heavy jobs.
"""
from __future__ import annotations

import os
import queue
import re
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import config, telegram
from .pipeline import ReelJob, run_job
from .utils import ensure_dir, log

URL_RE = re.compile(r"https?://\S+", re.IGNORECASE)


def _env_int(name: str, default: int) -> int:
    try:
        return int(os.environ.get(name, default))
    except (TypeError, ValueError):
        return default


def _default_lang() -> str | None:
    lang = os.environ.get("STREAMREELS_BOT_LANG", "ru").strip().lower()
    return lang or None


def _allowed_chats() -> set[int]:
    ids: set[int] = set()
    raw = os.environ.get("STREAMREELS_TG_ALLOWED", "")
    for part in raw.split(","):
        part = part.strip()
        if part:
            try:
                ids.add(int(part))
            except ValueError:
                pass
    cid = os.environ.get("STREAMREELS_TG_CHAT_ID")
    if cid:
        try:
            ids.add(int(cid))
        except ValueError:
            pass
    return ids


def classify(url: str) -> str:
    """Return 'live' or 'vod' for a URL."""
    u = url.lower()
    if "twitch.tv/" in u:
        tail = u.split("twitch.tv/", 1)[1]
        # VODs (/videos/<id>), clips (/<name>/clip/<slug>) and clips.twitch.tv.
        if "/videos/" in u or "/clip/" in u or "clips.twitch.tv" in u:
            return "vod"
        # A bare channel page (optionally /channel) is a live stream.
        if tail.split("/")[0].split("?")[0]:
            return "live"
    return "vod"


@dataclass
class _Task:
    chat: int
    url: str
    kind: str
    stop: threading.Event = field(default_factory=threading.Event)


class Bot:
    def __init__(self) -> None:
        self.allowed = _allowed_chats()
        self.lock_chat: int | None = None
        self.q: "queue.Queue[_Task | None]" = queue.Queue()
        self.current: _Task | None = None
        self.current_desc = "idle"
        self.offset: int | None = None
        self.lang = _default_lang()
        self.auto = _env_int("STREAMREELS_BOT_AUTO", 3)
        self.max_live = _env_int("STREAMREELS_BOT_LIVE_REELS", 3)

    # ---------------------------------------------------------------- auth
    def _authorized(self, chat: int) -> bool:
        if self.allowed:
            return chat in self.allowed
        # No allowlist configured: lock onto the first chat that appears.
        if self.lock_chat is None:
            self.lock_chat = chat
            log(f"bot: no allowlist; locked to chat {chat} "
                f"(set STREAMREELS_TG_ALLOWED to change)")
            return True
        return chat == self.lock_chat

    # -------------------------------------------------------------- worker
    def _worker(self) -> None:
        while True:
            task = self.q.get()
            if task is None:
                return
            self.current = task
            try:
                if task.kind == "live":
                    self._run_live(task)
                else:
                    self._run_vod(task)
            except Exception as exc:  # noqa: BLE001
                log(f"bot: task failed: {exc}")
                telegram.send_message(f"Не получилось: {exc}", chat=task.chat)
            finally:
                self.current = None
                self.current_desc = "idle"
                self.q.task_done()

    def _send_sidecar(self, video: Path, chat: int) -> None:
        sidecar = video.with_suffix(".txt")
        caption = ""
        if sidecar.exists():
            caption = sidecar.read_text(encoding="utf-8").strip()
        telegram.send_video(video, caption=caption, chat=chat)

    def _run_vod(self, task: _Task) -> None:
        self.current_desc = f"VOD {task.url}"
        telegram.send_message(
            f"Взял в работу: {task.url}\nИщу {self.auto} лучших моментов…",
            chat=task.chat)
        out_dir = ensure_dir(
            config.OUTPUT / "bot" / f"{time.strftime('%Y%m%d-%H%M%S')}-vod")
        job = ReelJob(
            source_url=task.url,
            segments=[],
            auto_count=self.auto,
            out_dir=out_dir,
            channel=task.url,
            subtitle_lang=self.lang,
        )
        produced = run_job(job)
        if not produced:
            telegram.send_message("Моментов не нашёл.", chat=task.chat)
            return
        telegram.send_message(
            f"Готово: {len(produced)} ролик(ов), отправляю…", chat=task.chat)
        for video in produced:
            if task.stop.is_set():
                telegram.send_message("Остановлено.", chat=task.chat)
                return
            self._send_sidecar(video, task.chat)

    def _run_live(self, task: _Task) -> None:
        from .live_pipeline import LiveJob, run_live
        self.current_desc = f"live {task.url}"
        telegram.send_message(
            f"Смотрю стрим: {task.url}\nСделаю {self.max_live} ролика и пришлю. "
            f"Отменить — /stop", chat=task.chat)
        out_dir = ensure_dir(
            config.OUTPUT / "bot" / f"{time.strftime('%Y%m%d-%H%M%S')}-live")

        def _on_reel(video: Path) -> None:
            if not task.stop.is_set():
                self._send_sidecar(video, task.chat)

        job = LiveJob(
            url=task.url,
            out_dir=out_dir,
            max_reels=self.max_live,
            subtitle_lang=self.lang,
            should_stop=task.stop.is_set,
            on_reel=_on_reel,
        )
        produced = run_live(job)
        if task.stop.is_set():
            telegram.send_message("Стрим остановлен по команде /stop.",
                                  chat=task.chat)
        elif not produced:
            telegram.send_message("Не поймал ни одного момента, попробуй ещё.",
                                  chat=task.chat)
        else:
            telegram.send_message(
                f"Готово: {len(produced)} ролик(ов) со стрима.", chat=task.chat)

    # -------------------------------------------------------------- commands
    HELP = (
        "Привет! Пришли ссылку — сделаю вертикальные рилсы с русскими "
        "субтитрами.\n\n"
        "• Ссылка на Twitch-канал → смотрю live и ловлю моменты\n"
        "• Ссылка на VOD (Twitch/YouTube) → нарезаю лучшие моменты\n\n"
        "Команды:\n"
        "/status — что сейчас в работе\n"
        "/stop — остановить текущий live\n"
        "/help — эта справка"
    )

    def _handle(self, update: dict) -> None:
        msg = update.get("message") or update.get("edited_message")
        if not msg:
            return
        chat = int(msg["chat"]["id"])
        if not self._authorized(chat):
            log(f"bot: ignoring unauthorized chat {chat}")
            return
        text = (msg.get("text") or "").strip()
        if not text:
            telegram.send_message("Пришли ссылку на стрим или VOD.", chat=chat)
            return

        low = text.lower()
        if low.startswith("/start") or low.startswith("/help"):
            telegram.send_message(self.HELP, chat=chat)
            return
        if low.startswith("/status"):
            if self.current is None:
                pending = self.q.qsize()
                tail = f" В очереди: {pending}." if pending else ""
                telegram.send_message(f"Сейчас ничего не делаю.{tail}", chat=chat)
            else:
                telegram.send_message(
                    f"В работе: {self.current_desc}", chat=chat)
            return
        if low.startswith("/stop"):
            task = self.current
            if task is not None and task.kind == "live":
                task.stop.set()
                telegram.send_message("Останавливаю стрим…", chat=chat)
            else:
                telegram.send_message("Сейчас нечего останавливать.", chat=chat)
            return

        m = URL_RE.search(text)
        if not m:
            telegram.send_message(
                "Не вижу ссылку. Пришли URL стрима или VOD.\n\n" + self.HELP,
                chat=chat)
            return

        url = m.group(0).rstrip(".,)>]")
        kind = classify(url)
        task = _Task(chat=chat, url=url, kind=kind)
        self.q.put(task)
        ahead = self.q.qsize() - 1
        wait = f" Перед тобой в очереди: {ahead}." if ahead else ""
        label = "live-стрим" if kind == "live" else "VOD"
        telegram.send_message(
            f"Принял ({label}): {url}.{wait} Начну, как освобожусь.",
            chat=chat)

    # -------------------------------------------------------------- main loop
    def run(self) -> None:
        worker = threading.Thread(target=self._worker, daemon=True)
        worker.start()
        log("bot: polling for messages (Ctrl-C to stop)")
        if not self.allowed:
            log("bot: WARNING no STREAMREELS_TG_ALLOWED/CHAT_ID; will lock to "
                "the first chat that writes")
        try:
            while True:
                try:
                    updates = telegram.get_updates(self.offset, timeout=50)
                except Exception as exc:  # noqa: BLE001
                    log(f"bot: getUpdates failed: {exc}")
                    time.sleep(5)
                    continue
                for upd in updates:
                    self.offset = int(upd["update_id"]) + 1
                    try:
                        self._handle(upd)
                    except Exception as exc:  # noqa: BLE001
                        log(f"bot: handle failed: {exc}")
        except KeyboardInterrupt:
            log("bot: stopping")
        finally:
            self.q.put(None)
            worker.join(timeout=5)


def main() -> int:
    Bot().run()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
