#!/bin/bash
# readme-lint.sh — fail CI when README claims drift from source of truth.
#
# Checks:
#   1. sdk/README.md health-check example `"version":"X.Y.Z"` matches Cargo.toml
#   2. README.md "N CDP tools" claim matches count of browser tool impls
#   3. README.md "N providers" claim matches count of named entries in the
#      OpenAI-compat provider registry
#
# Exit non-zero on any drift, with a clear message pointing at both the
# claim and the source of truth so fixes take seconds, not minutes.

set -uo pipefail

fail=0
err() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; fail=1; }
ok()  { printf '\033[32m OK \033[0m %s\n' "$*"; }

# ── 1. Cargo.toml version vs sdk/README.md health-check example ──────────────
cargo_version=$(grep -E '^version\s*=' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
sdk_version=$(grep -oE '"version":"[^"]+"' sdk/README.md | head -1 | sed -E 's/"version":"([^"]+)"/\1/')

if [ "$cargo_version" = "$sdk_version" ]; then
  ok "sdk/README.md health-check version matches Cargo.toml (${cargo_version})"
else
  err "sdk/README.md health-check says \"version\":\"${sdk_version}\" but Cargo.toml is ${cargo_version}"
  err "  fix: edit sdk/README.md line 22 to \"version\":\"${cargo_version}\""
fi

# ── 2. Browser tool count ────────────────────────────────────────────────────
browser_tool_count=$(grep -cE '^\s*fn name\(&self\) -> &str \{' src/tools/browser_tools.rs)
browser_claim=$(grep -oE '[0-9]+ CDP tools' README.md | head -1 | grep -oE '^[0-9]+')

if [ "$browser_tool_count" = "$browser_claim" ]; then
  ok "README.md \"${browser_claim} CDP tools\" matches src/tools/browser_tools.rs"
else
  err "README.md claims \"${browser_claim} CDP tools\" but source has ${browser_tool_count}"
  err "  fix: edit README.md to say \"${browser_tool_count} CDP tools\" (check table row + feature tour)"
fi

# ── 3. OpenAI-compat provider count ──────────────────────────────────────────
provider_count=$(grep -cE '^\s*name: "' src/api/openai_compat.rs)
provider_claim=$(grep -oE '[0-9]+ providers' README.md | head -1 | grep -oE '^[0-9]+')

if [ "$provider_count" = "$provider_claim" ]; then
  ok "README.md \"${provider_claim} providers\" matches src/api/openai_compat.rs"
else
  err "README.md claims \"${provider_claim} providers\" but source has ${provider_count}"
  err "  fix: edit README.md (table row + /model line) to say \"${provider_count} providers\""
fi

# ── 4. Autonomous browser claim ──────────────────────────────────────────────
if grep -qF "Autonomous browser agent" README.md; then
  ok "README.md has 'Autonomous browser agent' row"
else
  err "README.md missing 'Autonomous browser agent' row (expected after /browse ship)"
  err "  fix: add '| Autonomous browser agent | No | No | ...' to the comparison table"
fi

# ── Result ───────────────────────────────────────────────────────────────────
if [ "$fail" -eq 0 ]; then
  echo ""
  echo "All README claims match source."
else
  echo ""
  echo "README claims are drifting. Fix the claims (or the code) and re-run."
  exit 1
fi
