#!/usr/bin/env python3
"""bench.py — reproducible startup/footprint numbers for terminal coding agents.

No dependencies. For each tool it measures the on-disk size of the
executable, the wall-clock time of `<tool> --version` (cold start of the
runtime, N runs, median and p95), and the peak resident set of that process
(ru_maxrss of the child). Prints a Markdown table.

    scripts/bench.py                      # rustyclaw + whatever rivals are on PATH
    scripts/bench.py --runs 30 rustyclaw claude codex gemini goose opencode
"""
import argparse
import os
import platform
import resource
import shutil
import statistics
import subprocess
import sys
import time

DEFAULT_TOOLS = ["rustyclaw", "claude", "codex", "gemini", "goose", "opencode", "jcode", "codewhale", "claurst", "ante"]


def real_size(path):
    """Follow wrappers: a tiny shell shim is not the runtime's size."""
    p = os.path.realpath(path)
    try:
        with open(p, "rb") as f:
            head = f.read(2)
    except OSError:
        return os.path.getsize(p)
    if head == b"#!":
        return None  # script wrapper; size is not meaningful
    return os.path.getsize(p)


def measure(cmd, runs):
    times, rss = [], []
    for _ in range(runs):
        before = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        t0 = time.perf_counter()
        r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        t1 = time.perf_counter()
        after = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        if r.returncode not in (0, 1, 2):
            return None
        times.append((t1 - t0) * 1000.0)
        # ru_maxrss for CHILDREN is the max over all children so far; the
        # first run for each tool is the honest per-tool value, later runs
        # can only report a value >= it.
        rss.append(after if after >= before else before)
    times.sort()
    return {
        "median_ms": statistics.median(times),
        "p95_ms": times[int(len(times) * 0.95) - 1] if len(times) >= 20 else times[-1],
        "rss_mb": min(rss) / 1024.0,
    }


def version_of(tool):
    try:
        out = subprocess.run([tool, "--version"], capture_output=True, text=True, timeout=30)
        line = (out.stdout or out.stderr).strip().splitlines()
        return line[0][:40] if line else "?"
    except Exception:
        return "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tools", nargs="*", default=DEFAULT_TOOLS)
    ap.add_argument("--runs", type=int, default=20)
    args = ap.parse_args()
    rows = []
    # Measure each tool in its own subprocess so ru_maxrss(CHILDREN) is per-tool.
    for tool in args.tools:
        path = shutil.which(tool)
        if not path:
            continue
        if os.environ.get("BENCH_CHILD") == tool:
            m = measure([path, "--version"], args.runs)
            print(f"{m['median_ms']:.1f} {m['p95_ms']:.1f} {m['rss_mb']:.1f}" if m else "x")
            return
        sub = subprocess.run(
            [sys.executable, __file__, "--runs", str(args.runs), tool],
            env={**os.environ, "BENCH_CHILD": tool},
            capture_output=True,
            text=True,
        )
        out = sub.stdout.strip()
        if out == "x" or not out:
            rows.append((tool, version_of(tool), real_size(path), None))
            continue
        med, p95, rss = (float(x) for x in out.split())
        rows.append((tool, version_of(tool), real_size(path), (med, p95, rss)))
    print(f"Machine: {platform.node()} · {platform.machine()} · {platform.system()} {platform.release()}")
    print(f"Date: {time.strftime('%Y-%m-%d')} · runs per tool: {args.runs} · command: `<tool> --version`\n")
    print("| Tool | Version | Executable | Cold start (median) | p95 | Peak RSS |")
    print("|---|---|---|---|---|---|")
    for tool, ver, size, m in rows:
        size_s = f"{size / 1_048_576:.1f} MB" if size else "script wrapper"
        if m is None:
            print(f"| {tool} | {ver} | {size_s} | failed | | |")
            continue
        med, p95, rss = m
        print(f"| {tool} | {ver} | {size_s} | {med:.0f} ms | {p95:.0f} ms | {rss:.0f} MB |")


if __name__ == "__main__":
    main()
