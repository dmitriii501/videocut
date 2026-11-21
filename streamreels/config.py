"""Global config + on-disk layout."""
from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CACHE = Path(os.environ.get("STREAMREELS_CACHE", ROOT / "cache"))
ASSETS = ROOT / "assets"
OUTPUT = Path(os.environ.get("STREAMREELS_OUTPUT", ROOT / "out"))

# Final reel canvas: 9:16, 1080x1920 is heavy on CPU; 720x1280 is a good local default.
REEL_W = int(os.environ.get("REEL_W", 720))
REEL_H = int(os.environ.get("REEL_H", 1280))
FPS = int(os.environ.get("REEL_FPS", 30))

# Max source download resolution.
MAX_SOURCE_HEIGHT = int(os.environ.get("MAX_SOURCE_HEIGHT", 720))

# Whisper model size: tiny/base/small/medium.
WHISPER_MODEL = os.environ.get("WHISPER_MODEL", "large-v3-turbo")
WHISPER_LANG = os.environ.get("WHISPER_LANG") or None  # None = autodetect

YUNET_URL = (
    "https://huggingface.co/opencv/face_detection_yunet/resolve/main/"
    "face_detection_yunet_2023mar.onnx"
)
YUNET_PATH = ASSETS / "face_detection_yunet_2023mar.onnx"

SUBTITLE_FONT = os.environ.get("SUBTITLE_FONT", "Noto Sans")
SUBTITLE_WORDS_PER_CAPTION = int(os.environ.get("SUBTITLE_WORDS", 4))

# Render tuning.
VIDEO_CRF = int(os.environ.get("REEL_CRF", 20))
VIDEO_PRESET = os.environ.get("REEL_PRESET", "veryfast")
AUDIO_BITRATE = os.environ.get("REEL_AUDIO_BITRATE", "128k")
BLUR_RADIUS = int(os.environ.get("REEL_BLUR_RADIUS", 20))
BLUR_POWER = int(os.environ.get("REEL_BLUR_POWER", 2))

# Description generation.
# Extra hashtags appended to every auto-description (comma-separated).
DEFAULT_HASHTAGS = [
    t.strip() for t in os.environ.get(
        "STREAMREELS_DEFAULT_HASHTAGS", "#стрим,#twitch").split(",") if t.strip()
]
DESCRIBE_TITLE_MAX = int(os.environ.get("STREAMREELS_TITLE_MAX", 70))
DESCRIBE_DESC_MAX = int(os.environ.get("STREAMREELS_DESC_MAX", 400))
DESCRIBE_CAPTION_MAX = int(os.environ.get("STREAMREELS_CAPTION_MAX", 1000))
LLM_URL = os.environ.get("STREAMREELS_LLM_URL", "https://api.openai.com/v1")
LLM_MODEL = os.environ.get("STREAMREELS_LLM_MODEL", "gpt-4o-mini")
LLM_TEMPERATURE = float(os.environ.get("STREAMREELS_LLM_TEMPERATURE", 0.7))

# Webcam panel auto-detection via TypeSafe ("jev") box localization.
TYPESAFE_URL = os.environ.get("TYPESAFE_URL", "https://api.typesafe.ai")
TYPESAFE_MODEL = os.environ.get("TYPESAFE_MODEL", "jev-latest")
TYPESAFE_API_KEY = os.environ.get("TYPESAFE_API_KEY") or None
TYPESAFE_SAMPLES = int(os.environ.get("TYPESAFE_SAMPLES", 3))
PANELS = CACHE / "panels"


@dataclass
class Paths:
    work: Path
    source: Path | None = None
    gameplay: Path | None = None
    extra: dict = field(default_factory=dict)

    def sub(self, name: str) -> Path:
        p = self.work / name
        p.parent.mkdir(parents=True, exist_ok=True)
        return p
