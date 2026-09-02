#!/usr/bin/env python3
"""Compare downloader settings on a link honestly.

The trap this exists to avoid: running every trial of A, then every trial of B,
and reading the difference as a result. Paths drift. Measuring one 100 MB file
from one host on an ordinary connection, a *single* connection took 35 s, then
269 s, then 112 s within twenty minutes — a spread far larger than any
difference between the settings under test. Sequential A/B would have "proved"
whichever variant happened to run during the good minutes.

So: every variant is run once per round, the order rotates each round, and the
verdict is withheld unless the variants separate by more than the noise within
them.

    python3 packaging/bench.py URL
    python3 packaging/bench.py URL --connections 1,4,16 --rounds 3
    python3 packaging/bench.py URL --label 16=stock --label 1=single
"""

from __future__ import annotations

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time


def run_once(url: str, connections: int, directory: str) -> tuple[float, int]:
    """One download. Returns (seconds, bytes), or (nan, 0) if it failed."""
    out = os.path.join(directory, "bench.part")
    for stale in (out, out + ".aria2"):
        if os.path.exists(stale):
            os.remove(stale)

    started = time.monotonic()
    result = subprocess.run(
        [
            "aria2c",
            f"-x{connections}",
            f"-s{connections}",
            "-k1M",
            "--file-allocation=falloc",
            "--console-log-level=error",
            "--summary-interval=0",
            "--allow-overwrite=true",
            "-d", directory,
            "-o", "bench.part",
            url,
        ],
        capture_output=True,
        text=True,
    )
    elapsed = time.monotonic() - started
    if result.returncode != 0:
        sys.stderr.write(f"    ! aria2 exited {result.returncode}: {result.stderr.strip()[:120]}\n")
        return float("nan"), 0
    size = os.path.getsize(out) if os.path.exists(out) else 0
    return elapsed, size


def rate(seconds: float, size: int) -> float:
    return size / seconds / 1e6 if seconds > 0 else 0.0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("url")
    parser.add_argument("--connections", default="1,16",
                        help="comma-separated connection counts to compare (default 1,16)")
    parser.add_argument("--rounds", type=int, default=3,
                        help="how many times to run the whole set (default 3)")
    parser.add_argument("--keep", action="store_true",
                        help="keep the downloaded data instead of discarding it")
    args = parser.parse_args()

    if not shutil.which("aria2c"):
        sys.stderr.write("aria2c is not on PATH\n")
        return 1

    variants = [int(v) for v in args.connections.split(",") if v.strip()]
    if len(variants) < 2:
        sys.stderr.write("give at least two connection counts to compare\n")
        return 1

    directory = tempfile.mkdtemp(prefix="mdm-bench-")
    samples: dict[int, list[float]] = {v: [] for v in variants}

    print(f"{len(variants)} variants x {args.rounds} rounds, interleaved\n")
    try:
        for round_no in range(args.rounds):
            # Rotate, so no variant is always first and always warmest.
            order = variants[round_no % len(variants):] + variants[:round_no % len(variants)]
            for connections in order:
                seconds, size = run_once(args.url, connections, directory)
                if seconds != seconds:  # NaN
                    continue
                mbps = rate(seconds, size)
                samples[connections].append(mbps)
                print(f"  round {round_no + 1}  {connections:2d} conn  "
                      f"{seconds:7.1f}s  {size / 1e6:7.1f} MB  {mbps:6.2f} MB/s")
            print()
    except KeyboardInterrupt:
        print("\ninterrupted")
    finally:
        if not args.keep:
            shutil.rmtree(directory, ignore_errors=True)

    usable = {v: s for v, s in samples.items() if len(s) >= 2}
    if len(usable) < 2:
        print("not enough successful runs to compare")
        return 1

    print("median MB/s (spread across rounds):")
    medians = {}
    spreads = {}
    for connections, values in sorted(usable.items()):
        medians[connections] = statistics.median(values)
        spreads[connections] = max(values) - min(values)
        print(f"  {connections:2d} conn  {medians[connections]:6.2f}  "
              f"(±{spreads[connections] / 2:.2f}, n={len(values)})")

    best = max(medians, key=medians.get)
    worst = min(medians, key=medians.get)
    separation = medians[best] - medians[worst]
    noise = max(spreads.values())

    print()
    if separation <= noise:
        print(f"INCONCLUSIVE: variants differ by {separation:.2f} MB/s but a single "
              f"variant varies by {noise:.2f} MB/s between rounds.")
        print("The path is moving more than the setting is. Raise --rounds, or "
              "accept that on this link the setting does not decide anything.")
        return 0

    print(f"{best} connections wins: {medians[best]:.2f} vs {medians[worst]:.2f} MB/s "
          f"({medians[best] / medians[worst]:.2f}x), separation {separation:.2f} "
          f"MB/s exceeds the {noise:.2f} MB/s noise floor.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
