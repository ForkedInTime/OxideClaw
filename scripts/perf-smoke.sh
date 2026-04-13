#!/bin/bash
# perf-smoke.sh — fail CI on obvious perf regressions in the release binary.
#
# Checks:
#   1. Binary size ≤ 25 MB (README claims "19 MB static")
#   2. Cold-start time ≤ 500 ms (README claims "sub-50 ms" on bare metal;
#      CI runners are ~10× slower, so this is a generous ceiling that
#      still catches catastrophic regressions).
#
# Not a benchmark. Not a gate on the exact README numbers. A smoke test
# that fails when someone accidentally adds a 100 MB dependency or starts
# loading assets synchronously at startup.

set -euo pipefail

BIN="${1:-target/release/rustyclaw}"

if [ ! -x "$BIN" ]; then
  echo "binary not found at $BIN — build it first: cargo build --release" >&2
  exit 2
fi

fail=0
err() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; fail=1; }
ok()  { printf '\033[32m OK \033[0m %s\n' "$*"; }

# ── 1. Binary size ───────────────────────────────────────────────────────────
size_bytes=$(stat -c %s "$BIN" 2>/dev/null || stat -f %z "$BIN")
size_mb=$(( size_bytes / 1024 / 1024 ))
SIZE_LIMIT_MB=25

if [ "$size_mb" -le "$SIZE_LIMIT_MB" ]; then
  ok "binary size ${size_mb} MB (limit ${SIZE_LIMIT_MB} MB)"
else
  err "binary size ${size_mb} MB exceeds ${SIZE_LIMIT_MB} MB ceiling"
  err "  README claims '19 MB static' — something got heavy. Check recent Cargo.toml deps."
fi

# ── 2. Cold-start time ───────────────────────────────────────────────────────
if command -v hyperfine >/dev/null 2>&1; then
  # Use --help so we don't touch the network or ~/.env.
  # 5 runs, 2 warmup, export JSON so we can parse the mean reliably.
  hyperfine --runs 5 --warmup 2 --export-json /tmp/perf.json \
    "$BIN --help" >/dev/null

  mean_sec=$(python3 -c "import json; print(json.load(open('/tmp/perf.json'))['results'][0]['mean'])")
  mean_ms=$(awk -v s="$mean_sec" 'BEGIN{printf "%.0f", s * 1000}')
  STARTUP_LIMIT_MS=500

  if [ "$mean_ms" -le "$STARTUP_LIMIT_MS" ]; then
    ok "cold start ${mean_ms} ms (limit ${STARTUP_LIMIT_MS} ms)"
  else
    err "cold start ${mean_ms} ms exceeds ${STARTUP_LIMIT_MS} ms ceiling"
    err "  README claims 'sub-50 ms' on bare metal. CI runs ~10× slower but this is still too slow."
  fi
else
  echo "hyperfine not installed — skipping cold-start check (install: apt-get install hyperfine)" >&2
fi

# ── Result ───────────────────────────────────────────────────────────────────
if [ "$fail" -eq 0 ]; then
  echo ""
  echo "Perf smoke passed."
else
  echo ""
  echo "Perf regression detected. Investigate recent changes."
  exit 1
fi
