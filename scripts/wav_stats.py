#!/usr/bin/env python3
"""Summarize DC offset, peaks, and clipping of desktop CLI recordings.

Usage:
    python3 scripts/wav_stats.py <session-dir-or-wav> [...]

Capture with `--wav-gain 1` so the WAV holds the SDK samples unchanged.
Only the Python standard library is used.
"""

import array
import pathlib
import sys
import wave

# The first 42 ms (672 samples at 16 kHz) is the span AudioProcessing::Clean mutes.
SKIP_SAMPLES = 672
# Recordings shorter than this after the skip are too short for a stable mean.
MIN_BODY_SAMPLES = 8000


def peak(samples):
    return max((abs(x) for x in samples), default=0)


def stats(path):
    with wave.open(str(path)) as wav:
        if wav.getsampwidth() != 2 or wav.getnchannels() != 1:
            raise ValueError(f"{path}: expected 16-bit mono PCM")
        rate = wav.getframerate()
        pcm = array.array("h", wav.readframes(wav.getnframes()))
    if sys.byteorder == "big":
        pcm.byteswap()

    ms = lambda n: n * rate // 1000
    body = pcm[SKIP_SAMPLES:]
    dc = sum(body) / len(body) if len(body) >= MIN_BODY_SAMPLES else None
    return {
        "seconds": len(pcm) / rate,
        "dc": dc,
        "peak_0_22ms": peak(pcm[: ms(22)]),
        "peak_22_42ms": peak(pcm[ms(22) : ms(42)]),
        "peak_after_42ms": peak(body),
        "clipped": sum(1 for x in pcm if x >= 32767 or x <= -32768),
    }


def wav_paths(args):
    for arg in args:
        p = pathlib.Path(arg)
        yield from sorted(p.rglob("recording-*.wav")) if p.is_dir() else [p]


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    print(f"{'file':40} {'sec':>5} {'dc>42ms':>8} {'pk<22ms':>8} {'pk22-42':>8} {'pk>42ms':>8} {'clip':>5}")
    for path in wav_paths(sys.argv[1:]):
        s = stats(path)
        dc = f"{s['dc']:8.1f}" if s["dc"] is not None else f"{'-':>8}"
        name = f"{path.parent.name}/{path.name}"[-40:]
        print(f"{name:40} {s['seconds']:5.2f} {dc} {s['peak_0_22ms']:8d} "
              f"{s['peak_22_42ms']:8d} {s['peak_after_42ms']:8d} {s['clipped']:5d}")


if __name__ == "__main__":
    main()
