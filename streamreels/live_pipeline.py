"""Live orchestration: watch a Twitch stream, score hype, cut reels.

Flow:
  1. Start a RingBuffer recording the live stream into short segments.
  2. Start a ChatMonitor on the channel's IRC.
  3. Every tick each signal reports a continuous *evidence* ratio to its own
     trigger level (1.0 == exactly at the trigger): audio loudness vs. its
     adaptive threshold, chat vs. its spike threshold.  They are fused into one
     score ``audio_weight*ev_audio + chat_weight*ev_chat`` and a reel is cut
     when the score crosses ``score_threshold``.  Audio is the trustworthy
     signal; chat is down-weighted because it spikes on meme-spam/idle chatter
     just as much as on real hype (measured on a labelled clip bank).  A loud
     laugh fires on its own, a truly explosive chat still fires, and a "both
     warm" moment also fires even though neither alone would.
  4. The clip window is [center - pre_roll, center + post_roll]; the audio peak
     supplies a content-accurate centre, chat falls back to the current content
     time.  The pre-roll guarantees the actual moment is included even though
     we noticed it late.
  5. Extract that window from the ring buffer and render it as a reel via the
     normal pipeline (transcription + subtitles + webcam layout).
  6. Repeat until stopped or --max-reels is reached.
"""
from __future__ import annotations

import os
import time
from dataclasses import dataclass
from pathlib import Path

from . import config, chat, live, moment, review, subtitles
from .pipeline import ReelJob, run_job
from .utils import ensure_dir, log, slugify

# Stop the whole live loop once the user hits Ctrl-C; main() handles it.


@dataclass
class LiveJob:
    url: str
    out_dir: Path = config.OUTPUT
    pre_roll: float = 12.0
    post_roll: float = 28.0
    buffer_secs: float = 120.0
    segment_secs: float = 4.0
    chat_threshold: float = 2.5   # spike ratio vs baseline
    min_rate: float = 1.5         # absolute msgs/sec floor to count as a spike
    cooldown: float = 30.0        # min seconds between reels
    max_reels: int = 0            # 0 = unlimited
    # Audio-hype detection (laughter/shouts): the primary quality signal.
    use_audio: bool = True
    audio_adaptive: bool = True   # self-calibrate threshold to channel noise
    audio_k: float = 3.0          # adaptive: peak > median + k * robust_sigma
    audio_ratio: float = 1.6      # guardrail / fixed mode: vs baseline median
    audio_floor: float = 0.012    # ignore quiet "peaks" below this RMS
    audio_baseline_secs: float = 60.0
    # Weighted fusion of the two signals into one hype score.
    # Measured on a labelled 18-clip bank (5 channels): audio evidence separated
    # real moments from misses almost perfectly (audio-dominated detections were
    # 100% hits) while chat alone was noisy (~33%) because meme-spam and idle
    # chatter spike it too.  So chat is down-weighted: it still fires on a truly
    # explosive chat (needs chat_ev >= threshold/chat_weight) and still adds to a
    # "both warm" moment, but it can no longer carry a weak clip on its own.
    audio_weight: float = 1.0     # score contribution of audio evidence
    chat_weight: float = 0.4      # score contribution of chat evidence
    score_threshold: float = 0.7  # cut a reel when weighted score >= this
    # Post-filter: drop moments that are music/silence or die right after the
    # peak (no speech => not a watchable reel).
    moment_filter: bool = True
    min_words_after: int = 3      # words starting after the hype peak
    min_speech_ratio: float = 0.03  # speech-covered fraction of the clip
    min_mean_conf: float = 0.55   # transcript-quality gate (mean word confidence)
    max_low_conf_frac: float = 0.35  # ...and fraction of words below 0.3
    crf: int = 20
    preset: str = "veryfast"
    no_subs: bool = False
    subtitle_lang: str | None = None  # translate subtitles into this language
    webcam_spec: str | None = None
    typesafe_key: str | None = None
    send_telegram: bool = False
    # Review mode: save every detection (kept + dropped) with a CSV row so you
    # can label hit/miss and measure moment-selection precision.
    review_dir: Path | None = None
    on_reel: object | None = None  # optional callback(path) after each reel
    should_stop: object | None = None  # optional callback() -> bool, checked each tick


def run_live(job: LiveJob) -> list[Path]:
    ensure_dir(job.out_dir)
    channel = live.channel_from_url(job.url)
    # Unique per-run workdir: a killed/crashed run may leave an orphaned ffmpeg
    # recorder alive; giving each run its own dir stops those stray processes
    # from interleaving their segment counters into ours.
    run_id = f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
    work = ensure_dir(config.CACHE / "live" / slugify(channel) / run_id)

    buf = live.RingBuffer(job.url, work / "ring",
                          segment_secs=job.segment_secs,
                          keep_secs=job.buffer_secs)
    mon = chat.ChatMonitor(channel,
                           spike_ratio=job.chat_threshold,
                           min_rate=job.min_rate,
                           cooldown=job.cooldown)

    review_log = review.ReviewLog(job.review_dir) if job.review_dir else None
    if review_log:
        log(f"live: review mode -> {review_log.root} (label hit/miss in index.csv)")

    produced: list[Path] = []
    try:
        buf.start()
        mon.start()
        # Warm up so we have enough buffered pre-roll and a chat baseline.
        warmup = max(job.buffer_secs, 30.0)
        log(f"live: warming up {warmup:.0f}s (buffer + chat baseline)...")
        time.sleep(warmup)

        idx = 0
        last_center = -1e9
        log("live: watching audio + chat for hype (until the stream ends)...")
        while True:
            if job.max_reels and len(produced) >= job.max_reels:
                log(f"live: reached --max-reels {job.max_reels}, stopping")
                break
            if job.should_stop and job.should_stop():  # type: ignore[misc]
                log("live: stop requested; finishing")
                break
            # Poll instead of blocking so we notice the stream ending.
            center = None
            reason = ""
            while True:
                if not buf.is_alive():
                    log("live: recorder stopped (stream ended); finishing")
                    break
                # Each signal reports continuous evidence: 1.0 == at its own
                # trigger level, 0.0 == background.  Fuse them into one score
                # so "both warm" counts even when neither alone would fire.
                a_center = None
                a_ev = 0.0
                a_stats = (0.0, 0.0, 0.0)
                if job.use_audio:
                    a_center, a_ev, a_stats = buf.audio_evidence(
                        ratio=job.audio_ratio,
                        floor=job.audio_floor,
                        baseline_secs=job.audio_baseline_secs,
                        adaptive=job.audio_adaptive,
                        k=job.audio_k)
                c_ev, c_rate, c_base = mon.evidence()
                score = job.audio_weight * a_ev + job.chat_weight * c_ev
                if score >= job.score_threshold:
                    # Prefer the audio peak's content time (precise); fall back
                    # to the current content time for a chat-driven moment.
                    cand = a_center if (a_center is not None
                                        and a_ev >= c_ev) else buf.content_time()
                    if cand - last_center >= job.cooldown:
                        center = cand
                        peak, med, sigma = a_stats
                        parts = []
                        if job.use_audio and a_ev > 0:
                            if job.audio_adaptive:
                                thr = med + job.audio_k * sigma
                                parts.append(
                                    f"audio {a_ev:.2f} (e={peak:.3f} vs base "
                                    f"{med:.3f}+{job.audio_k:g}*{sigma:.3f}"
                                    f" thr {thr:.3f})")
                            else:
                                parts.append(
                                    f"audio {a_ev:.2f} (e={peak:.3f} vs "
                                    f"base {med:.3f})")
                        if c_ev > 0:
                            parts.append(f"chat {c_ev:.2f} ({c_rate:.1f}/s vs "
                                         f"{c_base:.1f}/s)")
                        reason = (f"score {score:.2f} = "
                                  + " + ".join(parts))
                        break
                time.sleep(0.5)
            if center is None:
                break
            last_center = center
            idx += 1
            log(f"live: HYPE #{idx} ({reason}; content {center:.1f}s)")

            c0 = max(0.0, center - job.pre_roll)
            c1 = center + job.post_roll
            clip = work / f"clip_{idx:03d}.mp4"
            try:
                out_clip = buf.extract_clip(c0, c1, clip)
            except Exception as exc:  # noqa: BLE001
                log(f"live: clip extraction failed: {exc}")
                continue
            if out_clip is None:
                log("live: buffer did not cover the clip window; skipping")
                continue

            clip_dur = max(1.0, c1 - c0)
            clip_words = None
            if job.moment_filter or not job.no_subs:
                # Transcribe the clip once; reuse the words for the moment
                # filter, subtitles and the description (no double whisper).
                try:
                    clip_words = subtitles.transcribe(out_clip)
                except Exception as exc:  # noqa: BLE001
                    log(f"live: clip transcription failed: {exc}")
                    clip_words = None
            verdict = None
            if job.moment_filter:
                verdict = moment.judge(
                    clip_words or [], clip_dur, peak_offset=center - c0,
                    min_words_after=job.min_words_after,
                    min_speech_ratio=job.min_speech_ratio,
                    min_mean_conf=job.min_mean_conf,
                    max_low_conf_frac=job.max_low_conf_frac)
                if not verdict.keep:
                    log(f"live: skip HYPE #{idx}: {verdict.reason}")

            if review_log is not None:
                # Save every detection -- kept AND dropped -- so you can spot
                # both detection misses and moment-filter false negatives.
                peak, med, sigma = a_stats
                review_log.record(out_clip, {
                    "idx": idx,
                    "time": time.strftime("%Y-%m-%d %H:%M:%S"),
                    "channel": channel,
                    "content_center": f"{center:.1f}",
                    "score": f"{score:.2f}",
                    "audio_ev": f"{a_ev:.2f}",
                    "chat_ev": f"{c_ev:.2f}",
                    "audio_peak": f"{peak:.4f}",
                    "audio_base": f"{med:.4f}",
                    "audio_sigma": f"{sigma:.4f}",
                    "chat_rate": f"{c_rate:.2f}",
                    "chat_base": f"{c_base:.2f}",
                    "filter_keep": "" if verdict is None else int(verdict.keep),
                    "filter_reason": "" if verdict is None else verdict.reason,
                    "mean_conf": "" if verdict is None else f"{verdict.mean_conf:.2f}",
                    "low_conf_frac": "" if verdict is None else f"{verdict.low_conf_frac:.2f}",
                    "garbage_frac": "" if verdict is None else f"{verdict.garbage_frac:.2f}",
                })

            if verdict is not None and not verdict.keep:
                out_clip.unlink(missing_ok=True)
                continue
            if verdict is not None:
                log(f"live: moment ok ({verdict.reason})")

            reel_out = job.out_dir
            reel_job = ReelJob(
                source_url=f"live:{channel}",
                segments=[(0.0, clip_dur)],
                out_dir=reel_out,
                crf=job.crf,
                preset=job.preset,
                no_subs=job.no_subs,
                local_source=out_clip,
                webcam_spec=job.webcam_spec,
                typesafe_key=job.typesafe_key,
                channel=channel,
                send_telegram=job.send_telegram,
                words=clip_words,
                subtitle_lang=job.subtitle_lang,
            )
            try:
                outs = run_job(reel_job)
            except Exception as exc:  # noqa: BLE001
                log(f"live: reel render failed: {exc}")
                continue
            # Rename to a stable live name.
            for o in outs:
                final = reel_out / f"{slugify(channel)}-live-{idx:03d}.mp4"
                try:
                    o.replace(final)
                except Exception:  # noqa: BLE001
                    final = o
                # Move the sidecar title/description file alongside the reel.
                sidecar = o.with_suffix(".txt")
                if sidecar.exists() and sidecar != final.with_suffix(".txt"):
                    try:
                        sidecar.replace(final.with_suffix(".txt"))
                    except Exception:  # noqa: BLE001
                        pass
                produced.append(final)
                log(f"live: produced {final}")
                if job.on_reel:
                    try:
                        job.on_reel(final)  # type: ignore[misc]
                    except Exception:  # noqa: BLE001
                        pass
            # Clean the intermediate clip.
            out_clip.unlink(missing_ok=True)
    finally:
        mon.stop()
        buf.stop()
    return produced
