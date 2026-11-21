"""Post-filter a detected live moment: keep only clips with real speech.

A loud audio peak is necessary but not sufficient: it can be a music drop, an
instrumental advert, or a one-off scream followed by dead air.  After extracting
the clip we transcribe it once (the words are reused for subtitles and the
description, so whisper is not run twice) and judge whether there is enough
speech — especially *after* the peak — to make a watchable reel.

A second axis is transcript *quality*: a loud music/noise peak forces whisper to
emit words from noise, producing low-confidence, often multi-language garbage
that is not a watchable moment.  We gate on that too, but conservatively (see
``judge``): on a labelled 18-clip bank confidence alone did NOT separate good
from bad clips, so the quality gate only fires when *both* mean confidence is
low *and* a large fraction of words are very low confidence — otherwise it
would drop genuine hype reactions.

Note: a spoken advert still has speech and will pass; this filter targets
instrumental music / silence, not spoken ads (that needs audio fingerprinting).
"""
from __future__ import annotations

import re
from dataclasses import dataclass

_CJK = re.compile(r"[\u3000-\u9fff\uac00-\ud7af\uff00-\uffef]")


@dataclass
class Verdict:
    keep: bool
    reason: str
    speech_ratio: float
    words_after: int
    words_total: int
    speech_after_ratio: float
    mean_conf: float = 1.0
    low_conf_frac: float = 0.0
    garbage_frac: float = 0.0


def transcript_quality(words) -> tuple[float, float, float]:
    """Return (mean_confidence, frac_words_below_0.3, frac_non_lexical).

    ``confidence`` is the per-word whisper probability (0..1).  ``frac_non_lexical``
    counts words that are CJK/fullwidth or contain no letters — a cheap signal
    for script-garbage on a Latin/Cyrillic stream.
    """
    n = len(words)
    if not n:
        return 0.0, 1.0, 1.0
    confs = [float(getattr(w, "confidence", 1.0)) for w in words]
    low = sum(1 for c in confs if c < 0.3) / n
    garbage = 0
    for w in words:
        t = w.text or ""
        if _CJK.search(t) or not any(ch.isalpha() for ch in t):
            garbage += 1
    return sum(confs) / n, low, garbage / n


def judge(words, clip_dur: float, peak_offset: float, *,
          min_words_after: int = 3,
          min_speech_ratio: float = 0.03,
          min_speech_after_ratio: float = 0.0,
          min_mean_conf: float = 0.55,
          max_low_conf_frac: float = 0.35,
          max_garbage_frac: float = 0.5) -> Verdict:
    """Decide whether a clip is worth rendering.

    words:        Word objects with times relative to the clip start (0..clip_dur)
    clip_dur:     real clip length in seconds
    peak_offset:  seconds into the clip where the hype centre sits
    min_words_after:      require at least this many words starting after the peak
    min_speech_ratio:     require speech to cover at least this fraction of the clip
    min_speech_after_ratio: optional gate on speech density after the peak
    min_mean_conf / max_low_conf_frac: transcript-quality gate.  A clip is
        dropped as "garbled transcript" only when mean confidence is below
        ``min_mean_conf`` AND at least ``max_low_conf_frac`` of words are below
        0.3 — i.e. both must be bad.  A single bad condition is tolerated so a
        genuine loud reaction (short, noisy but real speech) is not dropped.
    max_garbage_frac: hard drop if over half the words are non-lexical/CJK
        (clear script garbage), regardless of confidence.
    """
    clip_dur = max(0.1, float(clip_dur))
    peak = min(max(0.0, float(peak_offset)), clip_dur)

    def speech_dur(ws) -> float:
        return sum(max(0.0, w.end - w.start) for w in ws)

    total_words = len(words)
    total_speech = speech_dur(words)
    after = [w for w in words if w.start >= peak - 0.05]
    after_words = len(after)
    after_speech = speech_dur(after)

    speech_ratio = min(1.0, total_speech / clip_dur)
    after_span = max(0.1, clip_dur - peak)
    speech_after_ratio = min(1.0, after_speech / after_span)

    mean_conf, low_conf, garbage = transcript_quality(words)

    if total_words == 0:
        return Verdict(False, "no speech at all (music/silence?)", speech_ratio,
                       after_words, total_words, speech_after_ratio,
                       mean_conf, low_conf, garbage)
    if garbage > max_garbage_frac:
        return Verdict(False, f"garbled transcript (garbage {garbage:.2f} > "
                       f"{max_garbage_frac:.2f})", speech_ratio,
                       after_words, total_words, speech_after_ratio,
                       mean_conf, low_conf, garbage)
    if mean_conf < min_mean_conf and low_conf > max_low_conf_frac:
        return Verdict(False, f"garbled transcript (conf {mean_conf:.2f} < "
                       f"{min_mean_conf:.2f} and {low_conf:.0%} words <0.3)",
                       speech_ratio, after_words, total_words,
                       speech_after_ratio, mean_conf, low_conf, garbage)
    if after_words < min_words_after:
        return Verdict(False, f"only {after_words} word(s) after peak "
                       f"(need {min_words_after})", speech_ratio,
                       after_words, total_words, speech_after_ratio,
                       mean_conf, low_conf, garbage)
    if speech_ratio < min_speech_ratio:
        return Verdict(False, f"speech ratio {speech_ratio:.2f} < "
                       f"{min_speech_ratio:.2f}", speech_ratio,
                       after_words, total_words, speech_after_ratio,
                       mean_conf, low_conf, garbage)
    if speech_after_ratio < min_speech_after_ratio:
        return Verdict(False, f"speech after peak {speech_after_ratio:.2f} < "
                       f"{min_speech_after_ratio:.2f}", speech_ratio,
                       after_words, total_words, speech_after_ratio,
                       mean_conf, low_conf, garbage)
    return Verdict(True, f"{after_words} words after peak, speech "
                   f"{speech_ratio:.2f}", speech_ratio, after_words,
                   total_words, speech_after_ratio, mean_conf, low_conf, garbage)
