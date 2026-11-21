"""End-to-end pipeline orchestration."""
from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from . import config, describe, fetch, highlights, render, subtitles, telegram, webcam
from .utils import duration, ensure_dir, fmt_ts, log, slugify, short_hash


@dataclass
class ReelJob:
    source_url: str
    segments: list[tuple[float, float]]
    gameplay_query: list[str] | None = None
    gameplay_path: str | None = None
    out_dir: Path = config.OUTPUT
    zoom: float = 1.15
    crf: int = 20
    preset: str = "veryfast"
    no_subs: bool = False
    local_source: Path | None = None
    auto_count: int = 0  # >0 => ignore segments, auto-detect this many highlights
    webcam_spec: str | None = None  # manual 'x0,y0,x1,y1' override
    typesafe_key: str | None = None
    channel: str = ""  # streamer/channel name, used for descriptions
    send_telegram: bool = False  # upload each reel to Telegram on completion
    words: list | None = None  # precomputed transcript (clip-relative), skips whisper
    subtitle_lang: str | None = None  # translate subtitles to this language (e.g. 'ru')


def _workdir(url: str) -> Path:
    return ensure_dir(config.CACHE / "work" / short_hash(url))


def run_job(job: ReelJob) -> list[Path]:
    ensure_dir(job.out_dir)
    if job.local_source is not None:
        if not job.local_source.exists():
            raise FileNotFoundError(f"local source not found: {job.local_source}")
        source = job.local_source
        log(f"using local source: {source}")
    else:
        source = fetch.fetch_source(job.source_url, _workdir(job.source_url))
    total = duration(source)
    log(f"source duration: {fmt_ts(total)}")

    stem = slugify(source.stem or "reel")
    lay = render.layout()
    produced: list[Path] = []

    # Webcam panel is resolved per segment: the overlay can move over a long
    # stream (e.g. left for a while, then right), but is static within a clip.
    manual_panel = None
    if job.webcam_spec:
        manual_panel = webcam.parse_manual(job.webcam_spec)
        log(f"webcam panel (manual): {manual_panel.as_tuple()}")

    # Full-source word transcript (absolute times).  In auto mode it drives
    # highlight detection and is reused for every segment's subtitles; in manual
    # mode we still transcribe the whole source once so segments can be sliced.
    all_words = None
    if job.words is not None:
        # Caller already transcribed the (clip) source; reuse it verbatim.
        all_words = job.words
    elif not job.no_subs or job.auto_count > 0:
        full_audio = _workdir(job.source_url) / "full.wav"
        _extract_audio(source, 0.0, total, full_audio)
        all_words = subtitles.transcribe(full_audio)

    if job.auto_count > 0:
        segments = highlights.detect_highlights(
            source, count=job.auto_count, words=all_words,
        )
        if not segments:
            raise RuntimeError("auto-detect found no highlights; use -s instead")
        log(f"auto mode: {len(segments)} highlight(s)")
    else:
        segments = job.segments

    for idx, (start, dur) in enumerate(segments, 1):
        if start >= total:
            log(f"segment {idx} starts after source end, skipped")
            continue
        dur = min(dur, total - start)
        log(f"--- segment {idx}/{len(segments)}: {fmt_ts(start)} +{dur:.1f}s")

        ass_path = _workdir(job.source_url) / f"seg{idx}.ass"
        ass_for_render: Path | None = None
        seg_words = _slice_words(all_words or [], start, dur)
        sub_words_for_desc = seg_words
        if not job.no_subs:
            if seg_words:
                sub_words = seg_words
                if job.subtitle_lang:
                    from . import translate
                    src = config.WHISPER_LANG or "en"
                    sub_words = translate.translate_words(
                        seg_words, target=job.subtitle_lang, source=src)
                    log(f"translated subtitles {src}->{job.subtitle_lang}")
                sub_words_for_desc = sub_words
                subtitles.build_ass(
                    sub_words, ass_path, offset=start, clip_dur=dur,
                    region_top=0, region_height=lay.top_h,
                    width=lay.width, height=lay.height,
                )
                ass_for_render = ass_path
            else:
                log("no words transcribed; rendering without subtitles")

        name = f"{stem}-{idx:02d}.mp4"
        out = job.out_dir / name
        if manual_panel is not None:
            panel = manual_panel
        else:
            panel = webcam.detect_panel_window(
                source, start, dur, api_key=job.typesafe_key,
            )
        render.render_reel(
            source=source, start=start, dur=dur,
            ass_path=ass_for_render, out_path=out,
            crf=job.crf, preset=job.preset, panel=panel,
        )
        produced.append(out)

        # Description / title / hashtags from the reel's own transcript.
        # If subtitles were translated, describe the translated text so the
        # title/description match what viewers actually read.
        seg_text = " ".join(w.text for w in sub_words_for_desc)
        info = describe.describe(seg_text, channel=job.channel or stem)
        text_path = out.with_suffix(".txt")
        text_path.write_text(describe.as_text_file(info), encoding="utf-8")
        log(f"description -> {text_path.name}: {info['title']}")

        if job.send_telegram:
            caption = describe.as_caption(info)
            telegram.send_video(out, caption=caption)
    return produced


def _slice_words(words, start: float, dur: float):
    """Return words overlapping [start, start+dur] (absolute times kept)."""
    end = start + dur
    return [w for w in words if w.end > start and w.start < end]


def _extract_audio(source: Path, start: float, dur: float, out: Path) -> Path:
    from .utils import run
    ensure_dir(out.parent)
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
           "-ss", f"{start:.3f}", "-t", f"{dur:.3f}", "-i", str(source),
           "-vn", "-ac", "1", "-ar", "16000", "-y", str(out)]
    run(cmd)
    return out
