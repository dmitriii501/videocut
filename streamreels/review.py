"""Review log: persist every detected hype moment for manual quality scoring.

When ``--review-dir`` is set the live loop saves the raw clip for *every*
detection -- both the ones the moment filter keeps and the ones it drops -- plus
one row per detection in ``index.csv``.  You open the clips, label the ``label``
column (``hit`` / ``miss``), and ``tools/score_review.py`` turns that into a
precision number plus tuning hints for ``--score-threshold`` and the weights.

Saving dropped clips too is deliberate: it lets you spot false *negatives* of
the moment filter (a clip it threw away that was actually a good moment) and
false positives of detection, not just what survived.
"""
from __future__ import annotations

import csv
import shutil
from pathlib import Path

from .utils import ensure_dir, log

# Fixed column order; tools/score_review.py and your spreadsheet rely on it.
COLUMNS = [
    "idx", "time", "channel", "content_center",
    "score", "audio_ev", "chat_ev",
    "audio_peak", "audio_base", "audio_sigma",
    "chat_rate", "chat_base",
    "filter_keep", "filter_reason",
    "mean_conf", "low_conf_frac", "garbage_frac",
    "clip", "label",
]


class ReviewLog:
    """Append-only CSV + per-clip sidecars under ``root``."""

    def __init__(self, root: Path | str):
        self.root = ensure_dir(root)
        self.index = self.root / "index.csv"
        if not self.index.exists():
            with self.index.open("w", newline="", encoding="utf-8") as fh:
                csv.DictWriter(fh, fieldnames=COLUMNS).writeheader()
        else:
            self._migrate_header()

    def _migrate_header(self) -> None:
        """Rewrite index.csv if its header is missing newer COLUMNS.

        Old review dirs were written before columns like ``mean_conf`` existed;
        appending rows with the current COLUMNS would misalign them.  Read all
        rows, pad missing fields with blanks, rewrite with the current header.
        """
        try:
            with self.index.open("r", newline="", encoding="utf-8") as fh:
                reader = csv.DictReader(fh)
                header = reader.fieldnames or []
                if all(col in header for col in COLUMNS):
                    return
                rows = list(reader)
            with self.index.open("w", newline="", encoding="utf-8") as fh:
                writer = csv.DictWriter(fh, fieldnames=COLUMNS,
                                        extrasaction="ignore")
                writer.writeheader()
                for r in rows:
                    writer.writerow({k: r.get(k, "") for k in COLUMNS})
            log(f"review: migrated index.csv to {len(COLUMNS)} columns")
        except Exception as exc:  # noqa: BLE001
            log(f"review: could not migrate index.csv: {exc}")

    def record(self, clip: Path, row: dict) -> None:
        """Copy ``clip`` into the review dir and append one row to index.csv.

        ``row`` is a plain dict; missing columns are written empty.  Any
        exception is swallowed and logged -- review bookkeeping must never
        break the live loop.
        """
        idx = int(row.get("idx", 0))
        channel = row.get("channel", "stream")
        name = f"{channel}-{idx:03d}.mp4"
        dest = self.root / name
        try:
            shutil.copy2(clip, dest)
        except Exception as exc:  # noqa: BLE001
            log(f"review: could not save clip {name}: {exc}")
            return
        row = dict(row)
        row["clip"] = name
        row.setdefault("label", "")
        try:
            with self.index.open("a", newline="", encoding="utf-8") as fh:
                writer = csv.DictWriter(fh, fieldnames=COLUMNS,
                                        extrasaction="ignore")
                writer.writerow({k: row.get(k, "") for k in COLUMNS})
        except Exception as exc:  # noqa: BLE001
            log(f"review: could not write index.csv: {exc}")
            return
        self._write_sidecar(dest.with_suffix(".txt"), row)

    @staticmethod
    def _write_sidecar(path: Path, row: dict) -> None:
        try:
            lines = [f"clip:    {row.get('clip', '')}",
                     f"channel: {row.get('channel', '')}",
                     f"time:    {row.get('time', '')}",
                     f"score:   {row.get('score', '')} "
                     f"(audio {row.get('audio_ev', '')}, "
                     f"chat {row.get('chat_ev', '')})",
                     f"center:  {row.get('content_center', '')}s "
                     f"(content time)",
                     f"filter:  keep={row.get('filter_keep', '')} "
                     f"{row.get('filter_reason', '')}",
                     "",
                     "Watch the clip and label it in index.csv:",
                     "  hit  = good moment, keep this reel",
                     "  miss = bad/empty/mistimed, should not be a reel",
                     ]
            path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        except Exception:  # noqa: BLE001
            pass
