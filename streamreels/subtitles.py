"""Transcription (faster-whisper) + ASS subtitle generation with word highlight."""
from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path

from . import config
from .utils import log


@dataclass
class Word:
    start: float
    end: float
    text: str
    confidence: float = 1.0


def _load_model():
    from faster_whisper import WhisperModel

    log(f"loading whisper '{config.WHISPER_MODEL}' (int8, cpu)...")
    return WhisperModel(
        config.WHISPER_MODEL,
        device="cpu",
        compute_type="int8",
        cpu_threads=8,
        download_root=None,
    )


def _decode_audio(path: Path):
    """Decode any media to 16 kHz mono float32 via ffmpeg (avoids PyAV issues)."""
    import numpy as np

    cmd = [
        "ffmpeg", "-hide_banner", "-loglevel", "error",
        "-i", str(path), "-vn", "-ac", "1", "-ar", "16000",
        "-f", "f32le", "-",
    ]
    raw = subprocess.check_output(cmd)
    return np.frombuffer(raw, dtype=np.float32)


# Phantom phrases Whisper hallucinates on silence/laughter/music. Matched
# case-insensitively against the whole segment text; such segments are dropped.
_HALLUCINATIONS = (
    "субтитры сделал", "субтитры делал", "субтитры создал",
    "субтитры подготовил", "корректор", "редактор субтитров",
    "продолжение следует", "спасибо за просмотр", "подписывайтесь",
    "dimatorzok", "dima torzok", "amara.org", "thanks for watching",
    "subscribe", "субтитры от",
)


def _is_hallucination(text: str) -> bool:
    low = text.lower()
    return any(h in low for h in _HALLUCINATIONS)


def transcribe(media: Path, language: str | None = None) -> list[Word]:
    """Word-level transcription of the given media file (absolute times)."""
    model = _load_model()
    audio = _decode_audio(media)
    segments, info = model.transcribe(
        audio,
        language=language or config.WHISPER_LANG,
        vad_filter=True,
        vad_parameters={"min_silence_duration_ms": 300},
        word_timestamps=True,
        beam_size=5,
        # Disable cross-segment conditioning: it makes Whisper loop/repeat and
        # invent text on long silence or laughter.
        condition_on_previous_text=False,
    )
    log(f"whisper lang={info.language} p={info.language_probability:.2f}")
    words: list[Word] = []
    dropped = 0
    for seg in segments:
        seg_words = [w for w in (seg.words or []) if w.word.strip()]
        if not seg_words:
            continue
        seg_text = " ".join(w.word.strip() for w in seg_words)
        if _is_hallucination(seg_text):
            dropped += 1
            continue
        for w in seg_words:
            words.append(Word(float(w.start), float(w.end), w.word.strip(),
                              float(getattr(w, "probability", 1.0) or 1.0)))
    if dropped:
        log(f"dropped {dropped} hallucinated segment(s)")
    log(f"transcribed {len(words)} words")
    return words


# ----------------------------- ASS rendering -----------------------------

def _ass_time(t: float) -> str:
    t = max(0.0, t)
    h = int(t // 3600)
    m = int((t % 3600) // 60)
    s = t % 60
    return f"{h:d}:{m:02d}:{s:05.2f}"


def _ass_escape(text: str) -> str:
    return text.replace("\\", "\\\\").replace("{", "(").replace("}", ")")


def _group_words(words: list[Word], per_caption: int) -> list[list[Word]]:
    groups: list[list[Word]] = []
    cur: list[Word] = []
    for w in words:
        # break on long pause too
        if cur and (w.start - cur[-1].end > 0.6):
            groups.append(cur)
            cur = []
        cur.append(w)
        if len(cur) >= per_caption:
            groups.append(cur)
            cur = []
    if cur:
        groups.append(cur)
    return groups


def _style_header(width: int, height: int, font: str, size: int) -> str:
    # ASS colors are &HAABBGGRR. Primary white, highlight yellow.
    return f"""[Script Info]
ScriptType: v4.00+
PlayResX: {width}
PlayResY: {height}
WrapStyle: 2
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Default,{font},{size},&H00FFFFFF,&H0000FFFF,&H00000000,&H80000000,-1,0,0,0,100,100,0,0,1,3,1,2,40,40,40,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
"""


def build_ass(
    words: list[Word],
    out_path: Path,
    *,
    offset: float,
    clip_dur: float,
    region_top: int,
    region_height: int,
    width: int,
    height: int,
) -> Path:
    """Write ASS where each caption is centered inside the top (stream) region.

    Times are shifted by -offset (source -> clip-local). Words outside the clip
    are dropped. The current word is highlighted using inline color overrides.
    """
    font_size = max(20, int(region_height * 0.075))
    margin_v = int(height * 0.02) + max(0, height - region_top - region_height) + int(region_height * 0.06)
    header = _style_header(width, height, config.SUBTITLE_FONT, font_size)
    header = header.replace("Style: Default,", "Style: Default,")
    # override Alignment=2 (bottom center) margin so text sits at bottom of top region
    header = header.replace(",2,40,40,40,1", f",2,40,40,{margin_v},1")

    groups = _group_words(words, config.SUBTITLE_WORDS_PER_CAPTION)
    lines: list[str] = []
    hl = r"{\c&H00FFFF&}"   # yellow (BGR)
    reset = r"{\c&HFFFFFF&}"  # white

    for grp in groups:
        grp = [w for w in grp if offset <= w.start < offset + clip_dur]
        if not grp:
            continue
        cap_start = grp[0].start - offset
        cap_end = min(grp[-1].end - offset, clip_dur)
        if cap_end <= cap_start:
            continue
        # for each word, emit a sub-event so only the active word is highlighted
        for i, w in enumerate(grp):
            seg_start = max(0.0, w.start - offset)
            seg_end = min(clip_dur, w.end - offset)
            if seg_end <= seg_start:
                seg_end = min(clip_dur, seg_start + 0.12)
            parts = []
            for j, other in enumerate(grp):
                txt = _ass_escape(other.text)
                parts.append(f"{hl}{txt}{reset}" if j == i else txt)
            body = " ".join(parts)
            lines.append(
                f"Dialogue: 0,{_ass_time(seg_start)},{_ass_time(seg_end)},"
                f"Default,,0,0,0,,{body}"
            )
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(header + "\n".join(lines) + "\n", encoding="utf-8")
    log(f"wrote {len(lines)} subtitle events -> {out_path}")
    return out_path
