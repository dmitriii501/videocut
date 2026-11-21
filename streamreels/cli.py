"""Command-line interface for streamreels."""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

from . import config, fetch
from .pipeline import ReelJob, run_job
from .utils import check_ffmpeg, log, parse_segment


def _build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="streamreels",
        description="Cut stream VODs into vertical reels with face tracking, "
                    "subtitles and a distracting gameplay strip.",
    )
    p.add_argument("url", nargs="?",
                   help="YouTube or Twitch VOD URL (omit if using --local)")
    p.add_argument("-s", "--segment", action="append", metavar="START-END",
                   help="Segment as 'start-end' or 'start+dur', e.g. 12:30-13:05 or 90+30. "
                        "Repeat for multiple reels. Required unless --auto is used.")
    p.add_argument("--auto", type=int, metavar="N", default=0,
                   help="Auto-detect N highlight moments (by audio energy + speech "
                        "cues) and cut those instead of using -s.")
    p.add_argument("-o", "--out", default=str(config.OUTPUT), help="output directory")
    p.add_argument("--local", help="use this local video file as the source (skips download)")
    p.add_argument("--gameplay", help="use this local video instead of auto-downloading")
    p.add_argument("--gameplay-query", action="append",
                   help="YouTube search query for gameplay (repeatable)")
    p.add_argument("--zoom", type=float, default=1.15, help="stream crop zoom (default 1.15)")
    p.add_argument("--crf", type=int, default=20, help="x264 CRF (default 20)")
    p.add_argument("--preset", default="veryfast", help="x264 preset (default veryfast)")
    p.add_argument("--no-subs", action="store_true", help="disable subtitles")
    p.add_argument("--subtitle-lang", metavar="LANG",
                   help="translate subtitles into this language, e.g. ru "
                        "(original is auto-detected; uses MyMemory, cached)")
    p.add_argument("--telegram", action="store_true",
                   help="send each finished reel to Telegram (bot token/chat via "
                        "STREAMREELS_TG_TOKEN / STREAMREELS_TG_CHAT_ID)")
    p.add_argument("--tg-chat", metavar="ID",
                   help="Telegram chat_id to send to (else auto-discover / env)")
    p.add_argument("--webcam", metavar="X0,Y0,X1,Y1",
                   help="manual webcam panel box in relative 0..1 coords "
                        "(skips auto-detection), e.g. 0.7,0.0,1.0,1.0")
    p.add_argument("--typesafe-key", metavar="KEY",
                   help="TypeSafe API key for webcam auto-detection "
                        "(or set TYPESAFE_API_KEY)")
    p.add_argument("--no-webcam-detect", action="store_true",
                   help="skip webcam auto-detection, use the default panel")
    p.add_argument("--bot", action="store_true",
                   help="run the Telegram bot: send it a stream/VOD URL and it "
                        "replies with finished reels (long-polls getUpdates)")

    # Live mode.
    lv = p.add_argument_group("live")
    lv.add_argument("--live", metavar="TWITCH_URL",
                    help="watch a live Twitch stream, detect chat spikes and cut "
                         "reels in real time (instead of -s/--auto)")
    lv.add_argument("--pre-roll", type=float, default=12.0, metavar="SEC",
                    help="seconds kept before the detected moment (default 12)")
    lv.add_argument("--post-roll", type=float, default=28.0, metavar="SEC",
                    help="seconds kept after the detected moment (default 28)")
    lv.add_argument("--buffer-secs", type=float, default=120.0, metavar="SEC",
                    help="ring buffer length to keep (default 120)")
    lv.add_argument("--chat-threshold", type=float, default=2.5, metavar="RATIO",
                    help="spike = chat rate this many times the baseline (default 2.5)")
    lv.add_argument("--no-audio", action="store_true",
                    help="disable audio-hype detection (laughter/shouts); use chat only")
    lv.add_argument("--audio-fixed", action="store_true",
                    help="disable adaptive audio threshold; use a fixed ratio "
                         "vs the baseline median")
    lv.add_argument("--audio-k", type=float, default=3.0, metavar="K",
                    help="adaptive: trigger when loudness exceeds the baseline "
                         "median by K robust sigmas (default 3.0)")
    lv.add_argument("--audio-ratio", type=float, default=1.6, metavar="RATIO",
                    help="fixed mode / guardrail: loudness burst = segment energy "
                         "this many times the baseline median (default 1.6)")
    lv.add_argument("--audio-weight", type=float, default=1.0, metavar="W",
                    help="weight of audio evidence in the fused hype score "
                         "(default 1.0)")
    lv.add_argument("--chat-weight", type=float, default=0.4, metavar="W",
                    help="weight of chat evidence in the fused hype score "
                         "(default 0.4; chat is noisy on its own, so it only "
                         "amplifies audio or fires on a truly explosive chat)")
    lv.add_argument("--score-threshold", type=float, default=0.7, metavar="S",
                    help="cut a reel when the weighted hype score reaches S "
                         "(default 0.7; tuned on a labelled clip bank — audio "
                         "alone at its trigger scores 1.0, a strong chat-only "
                         "moment needs chat_ev >= S/chat_weight)")
    lv.add_argument("--max-reels", type=int, default=0, metavar="N",
                    help="stop after N live reels (0 = unlimited)")
    lv.add_argument("--no-moment-filter", action="store_true",
                    help="do not drop moments that are music/silence or have "
                         "no speech after the peak")
    lv.add_argument("--min-words-after", type=int, default=3, metavar="N",
                    help="moment filter: require N words after the peak "
                         "(default 3)")
    lv.add_argument("--min-speech-ratio", type=float, default=0.03, metavar="R",
                    help="moment filter: require speech to cover R of the clip "
                         "(default 0.03)")
    lv.add_argument("--min-mean-conf", type=float, default=0.55, metavar="C",
                    help="moment filter: drop garbled transcripts whose mean "
                         "word confidence is below C (default 0.55; only fires "
                         "together with --max-low-conf-frac)")
    lv.add_argument("--max-low-conf-frac", type=float, default=0.35, metavar="R",
                    help="moment filter: ...and at least R of words are below "
                         "0.3 confidence (default 0.35)")
    lv.add_argument("--review-dir", metavar="DIR",
                    help="save every detection (kept + dropped) with index.csv so "
                         "you can label hit/miss and measure precision "
                         "(see tools/score_review.py)")

    # Download / anti-bot options (passed to yt-dlp).
    dl = p.add_argument_group("download")
    dl.add_argument("--cookies", help="path to a cookies.txt file for yt-dlp")
    dl.add_argument("--cookies-from-browser",
                    help="load cookies from a browser, e.g. firefox, chromium, brave")
    dl.add_argument("--js-runtime", help="JS runtime for yt-dlp challenge solving, e.g. node")
    dl.add_argument("--remote-components", action="store_true",
                    help="enable yt-dlp remote EJS component (helps YouTube bot-checks)")
    return p


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    check_ffmpeg()

    webcam_spec = args.webcam
    if args.no_webcam_detect and not webcam_spec:
        from .webcam import DEFAULT_PANEL
        webcam_spec = ",".join(f"{v:.3f}" for v in DEFAULT_PANEL)

    if args.tg_chat:
        import os
        os.environ["STREAMREELS_TG_CHAT_ID"] = str(args.tg_chat)

    if args.bot:
        from .bot import main as bot_main
        log("starting Telegram bot (send it a URL)...")
        try:
            return bot_main()
        except KeyboardInterrupt:
            return 0

    if args.live:
        from .live_pipeline import LiveJob, run_live
        lj = LiveJob(
            url=args.live,
            out_dir=Path(args.out),
            pre_roll=args.pre_roll,
            post_roll=args.post_roll,
            buffer_secs=args.buffer_secs,
            chat_threshold=args.chat_threshold,
            max_reels=args.max_reels,
            use_audio=not args.no_audio,
            audio_adaptive=not args.audio_fixed,
            audio_k=args.audio_k,
            audio_ratio=args.audio_ratio,
            audio_weight=args.audio_weight,
            chat_weight=args.chat_weight,
            score_threshold=args.score_threshold,
            moment_filter=not args.no_moment_filter,
            min_words_after=args.min_words_after,
            min_speech_ratio=args.min_speech_ratio,
            min_mean_conf=args.min_mean_conf,
            max_low_conf_frac=args.max_low_conf_frac,
            review_dir=Path(args.review_dir) if args.review_dir else None,
            crf=args.crf,
            preset=args.preset,
            no_subs=args.no_subs,
            subtitle_lang=args.subtitle_lang,
            webcam_spec=webcam_spec,
            typesafe_key=args.typesafe_key,
            send_telegram=args.telegram,
        )
        log(f"live mode: {args.live} (pre {args.pre_roll}s / post "
            f"{args.post_roll}s, threshold {args.chat_threshold}x)")
        try:
            outs = run_live(lj)
        except KeyboardInterrupt:
            log("live: interrupted by user")
            return 0
        except Exception as exc:  # noqa: BLE001
            log(f"FAILED: {exc}")
            return 1
        log(f"live: done, {len(outs)} reel(s)")
        for o in outs:
            log(f"  {o}")
        return 0

    if not args.url and not args.local:
        log("нужен либо URL источника, либо --local <файл>")
        return 2
    if not args.auto and not args.segment:
        log("нужен либо -s START-END (можно несколько), либо --auto N")
        return 2
    try:
        segments = [parse_segment(s) for s in (args.segment or [])]
    except ValueError as exc:
        log(f"bad --segment: {exc}")
        return 2

    fetch.OPTS.update(
        cookies=args.cookies,
        cookies_from_browser=args.cookies_from_browser,
        js_runtime=args.js_runtime,
        remote_components=args.remote_components,
    )

    job = ReelJob(
        source_url=args.url or args.local or "",
        segments=segments,
        gameplay_query=args.gameplay_query,
        gameplay_path=args.gameplay,
        out_dir=Path(args.out),
        zoom=args.zoom,
        crf=args.crf,
        preset=args.preset,
        no_subs=args.no_subs,
        local_source=Path(args.local) if args.local else None,
        auto_count=args.auto,
        webcam_spec=webcam_spec,
        typesafe_key=args.typesafe_key,
        channel=(args.live or args.url or ""),
        send_telegram=args.telegram,
        subtitle_lang=args.subtitle_lang,
    )
    try:
        outs = run_job(job)
    except Exception as exc:  # noqa: BLE001
        log(f"FAILED: {exc}")
        return 1
    log("produced:")
    for o in outs:
        log(f"  {o}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
