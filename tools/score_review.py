#!/usr/bin/env python3
"""Turn a labelled review directory into a moment-selection quality report.

Workflow:
  1. Run the live loop with ``--review-dir review/``.  Every detection (kept
     and dropped) lands in ``review/index.csv`` with a ``.mp4`` and ``.txt``.
  2. Watch the clips and fill the ``label`` column: ``hit`` (good moment) or
     ``miss`` (bad/empty/mistimed).  Leave blank to ignore a row.
  3. Run this script:

        .venv/bin/python tools/score_review.py review/
        .venv/bin/python tools/score_review.py review/ --precision 0.8

It prints the current precision, a score-threshold sweep (precision vs. how
many clips you'd keep), per-signal evidence for hits vs misses (to see whether
to lean on audio or chat via --audio-weight / --chat-weight), the score
threshold that best trades precision for coverage, and a grid search over
(audio_weight, chat_weight, threshold) recomputed from the raw audio_ev/chat_ev
columns — copy its top line into the live flags.  Pass --no-weights to skip the
grid search.
"""
from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path

HITS = {"hit", "1", "y", "yes", "good", "ok", "true"}
MISSES = {"miss", "0", "n", "no", "bad", "false"}


def _f(row: dict, key: str, default: float = 0.0) -> float:
    try:
        return float(row.get(key, "") or default)
    except (TypeError, ValueError):
        return default


def load(index: Path) -> list[dict]:
    with index.open(newline="", encoding="utf-8") as fh:
        rows = list(csv.DictReader(fh))
    for r in rows:
        lab = (r.get("label") or "").strip().lower()
        if lab in HITS:
            r["_label"] = 1
        elif lab in MISSES:
            r["_label"] = 0
        else:
            r["_label"] = None
    return rows


def _mean(xs: list[float]) -> float:
    return sum(xs) / len(xs) if xs else 0.0


def _median(xs: list[float]) -> float:
    if not xs:
        return 0.0
    s = sorted(xs)
    n = len(s)
    return s[n // 2] if n % 2 else (s[n // 2 - 1] + s[n // 2]) / 2


def _pct(hit: int, total: int) -> str:
    return f"{100.0 * hit / total:.0f}%" if total else "n/a"


def sweep(labelled: list[dict], thresholds: list[float]) -> list[tuple]:
    """For each score threshold: (thr, kept, hits, precision, coverage)."""
    out = []
    n = len(labelled)
    for thr in thresholds:
        kept = [r for r in labelled if _f(r, "score") >= thr]
        hits = sum(1 for r in kept if r["_label"] == 1)
        prec = hits / len(kept) if kept else 0.0
        cov = len(kept) / n if n else 0.0
        out.append((thr, len(kept), hits, prec, cov))
    return out


def grid_search(labelled: list[dict], target: float,
                audio_weights=(1.0,), chat_weights=(0.0, 0.2, 0.4, 0.6, 0.8, 1.0),
                steps: int = 24) -> list[tuple]:
    """Find (audio_w, chat_w, thr, kept, hits, precision, coverage) combos.

    Recomputes the fused score from the raw ``audio_ev`` / ``chat_ev`` columns
    (independent of the weights used at capture time), sweeps a threshold for
    each weight pair, and keeps only combos that meet ``target`` precision.
    Sorted by coverage (desc) so the best trade-off is first.
    """
    n = len(labelled)
    if not n:
        return []
    a = [_f(r, "audio_ev") for r in labelled]
    c = [_f(r, "chat_ev") for r in labelled]
    lab = [r["_label"] for r in labelled]
    combos = []
    for aw in audio_weights:
        for cw in chat_weights:
            scores = [aw * a[i] + cw * c[i] for i in range(n)]
            lo, hi = min(scores), max(scores)
            if hi <= lo:
                continue
            step = (hi - lo) / max(1, steps)
            for k in range(steps + 1):
                thr = lo + k * step
                kept = [i for i in range(n) if scores[i] >= thr]
                if not kept:
                    continue
                hits = sum(1 for i in kept if lab[i] == 1)
                prec = hits / len(kept)
                if prec >= target:
                    combos.append((aw, cw, thr, len(kept), hits, prec,
                                   len(kept) / n))
    combos.sort(key=lambda t: (-t[6], -t[5]))
    # De-dup near-identical thresholds per (aw, cw): keep the lowest thr that
    # still meets target (widest coverage) — it's already first after sort.
    seen = set()
    uniq = []
    for combo in combos:
        key = (combo[0], combo[1], combo[3], combo[4])
        if key in seen:
            continue
        seen.add(key)
        uniq.append(combo)
    return uniq


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("review_dir", help="directory containing index.csv")
    ap.add_argument("--precision", type=float, default=0.7,
                    help="target precision when recommending a threshold "
                         "(default 0.7)")
    ap.add_argument("--steps", type=int, default=20,
                    help="number of thresholds to sweep (default 20)")
    ap.add_argument("--no-weights", action="store_true",
                    help="skip the weight/threshold grid search over audio_ev/"
                         "chat_ev (shown by default)")
    args = ap.parse_args(argv)

    index = Path(args.review_dir) / "index.csv"
    if not index.exists():
        print(f"no index.csv in {args.review_dir}", file=sys.stderr)
        return 2

    rows = load(index)
    labelled = [r for r in rows if r["_label"] is not None]
    if not labelled:
        print(f"{len(rows)} detection(s), none labelled yet.\n"
              f"Open {index} and set the 'label' column to hit/miss, "
              f"then re-run.")
        return 0

    hits = sum(1 for r in labelled if r["_label"] == 1)
    misses = len(labelled) - hits
    print(f"detections: {len(rows)}   labelled: {len(labelled)} "
          f"(hit {hits} / miss {misses})")
    print(f"overall precision: {_pct(hits, len(labelled))}")

    # Split by what the moment filter did: kept clips were actual reels;
    # dropped ones are false negatives of the filter if labelled 'hit'.
    kept = [r for r in labelled if r.get("filter_keep", "") == "1"]
    dropped = [r for r in labelled if r.get("filter_keep", "") == "0"]
    if kept:
        kh = sum(1 for r in kept if r["_label"] == 1)
        print(f"  kept by filter:    {len(kept)}  precision {_pct(kh, len(kept))}")
    if dropped:
        dh = sum(1 for r in dropped if r["_label"] == 1)
        print(f"  dropped by filter: {len(dropped)}  "
              f"false-negatives {dh} ({_pct(dh, len(dropped))} of drops were good)")

    # Per-signal evidence: do hits look different from misses?
    h = [r for r in labelled if r["_label"] == 1]
    m = [r for r in labelled if r["_label"] == 0]
    print("\nper-signal evidence (median):")
    for key, name in (("audio_ev", "audio"), ("chat_ev", "chat"),
                      ("score", "score")):
        print(f"  {name:5s}  hit {_median([_f(r, key) for r in h]):5.2f}   "
              f"miss {_median([_f(r, key) for r in m]):5.2f}")
    print("  -> if one signal separates hit/miss better, raise its weight.")

    # Threshold sweep.
    scores = sorted({_f(r, "score") for r in labelled})
    lo, hi = (scores[0], scores[-1]) if scores else (0.0, 1.0)
    if hi <= lo:
        hi = lo + 1.0
    step = (hi - lo) / max(1, args.steps)
    thresholds = [lo + i * step for i in range(args.steps + 1)]
    table = sweep(labelled, thresholds)

    print("\nscore-threshold sweep (kept = clips you'd actually render):")
    print(f"  {'thr':>5} {'kept':>5} {'hits':>5} {'prec':>6} {'cover':>6}")
    for thr, k, hh, prec, cov in table:
        print(f"  {thr:5.2f} {k:5d} {hh:5d} {prec:6.0%} {cov:6.0%}")

    # Recommend the lowest threshold that still meets target precision (more
    # coverage at the same quality bar).
    target = args.precision
    ok = [row for row in table if row[3] >= target and row[1] > 0]
    if ok:
        thr, k, hh, prec, cov = ok[0]
        print(f"\nrecommended --score-threshold {thr:.2f}  "
              f"(precision {prec:.0%}, keeps {k}/{len(labelled)} = {cov:.0%})")
    else:
        best = max(table, key=lambda row: (row[3], row[1]))
        print(f"\nno threshold reaches {target:.0%} precision; best is "
              f"{best[0]:.2f} (precision {best[3]:.0%}, keeps {best[1]}). "
              f"Consider more training data or different weights.")

    # Weight/threshold grid search over the raw evidence columns.  This is what
    # turns one-off tuning into a loop: re-run after labelling a new bank and
    # copy the top recommendation into --audio-weight / --chat-weight /
    # --score-threshold.
    if not args.no_weights:
        combos = grid_search(labelled, target)
        print(f"\nweight search (recomputed from audio_ev/chat_ev, "
              f"target precision {target:.0%}):")
        if not combos:
            print("  no (audio_weight, chat_weight, threshold) combo reaches "
                  f"{target:.0%} precision on this data.")
        else:
            print(f"  {'a_w':>4} {'c_w':>4} {'thr':>5} {'kept':>5} "
                  f"{'hits':>5} {'prec':>6} {'cover':>6}")
            for aw, cw, thr, k, hh, prec, cov in combos[:8]:
                print(f"  {aw:4.1f} {cw:4.1f} {thr:5.2f} {k:5d} {hh:5d} "
                      f"{prec:6.0%} {cov:6.0%}")
            aw, cw, thr, k, hh, prec, cov = combos[0]
            print(f"\nrecommended: --audio-weight {aw:g} --chat-weight {cw:g} "
                  f"--score-threshold {thr:.2f}  "
                  f"(precision {prec:.0%}, keeps {k}/{len(labelled)} = {cov:.0%})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
