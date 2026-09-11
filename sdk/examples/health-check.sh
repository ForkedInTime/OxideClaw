#!/usr/bin/env bash
# Simplest SDK test — check if oxideclaw --headless is working.
# No API key needed.

set -euo pipefail

(
  echo '{"id":"1","type":"health/check"}'
  sleep 1
) | oxideclaw --headless 2>/dev/null
