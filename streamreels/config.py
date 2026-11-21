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
