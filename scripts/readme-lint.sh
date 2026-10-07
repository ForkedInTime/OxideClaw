#!/bin/bash
# readme-lint.sh — fail CI when README claims drift from source of truth.
#
# Checks:
#   1. sdk/README.md health-check example `"version":"X.Y.Z"` matches Cargo.toml
#   2. README.md "N CDP tools" / "Nine CDP-driven tools" claim matches the
#      count of browser tool impls
#   3. README.md "N providers" claim matches count of named entries in the
#      OpenAI-compat provider registry (the PROVIDERS table only, not the
#      tool names in that file's tests)
#   4. README.md keeps the 'Autonomous browser agent' row
#   5. README.md "Rust X.Y+" matches Cargo.toml rust-version
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
# Count tools named browser_*; browse_done is the agent-loop terminator, not a CDP tool.
browser_tool_count=$(grep -A1 -E '^\s*fn name\(&self\) -> &str \{' src/tools/browser_tools.rs | grep -c '"browser_')
# The claim may be written in digits ("9 CDP tools") or as a word at the
# start of a sentence ("Nine CDP-driven tools").
browser_claim=$(grep -oiE '\b([0-9]+|one|two|three|four|five|six|seven|eight|nine|ten|eleven|twelve) CDP(-driven)? tools' README.md \
  | head -1 | awk '{print tolower($1)}')
case "$browser_claim" in
  one) browser_claim=1 ;; two) browser_claim=2 ;; three) browser_claim=3 ;;
  four) browser_claim=4 ;; five) browser_claim=5 ;; six) browser_claim=6 ;;
  seven) browser_claim=7 ;; eight) browser_claim=8 ;; nine) browser_claim=9 ;;
  ten) browser_claim=10 ;; eleven) browser_claim=11 ;; twelve) browser_claim=12 ;;
esac

if [ "$browser_tool_count" = "$browser_claim" ]; then
  ok "README.md \"${browser_claim} CDP tools\" matches src/tools/browser_tools.rs"
else
  err "README.md claims \"${browser_claim} CDP tools\" but source has ${browser_tool_count}"
  err "  fix: edit README.md to say \"${browser_tool_count} CDP tools\" (check table row + feature tour)"
fi

# ── 3. OpenAI-compat provider count ──────────────────────────────────────────
# Only the registry: the same file's tests build tool definitions with
# `name: "Read"` and the like, which are not providers.
provider_count=$(awk '/^pub static PROVIDERS/ {on=1} on && /^\];/ {on=0} on' src/api/openai_compat.rs \
  | grep -cE '^\s*name: "')
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
  err "  fix: add an 'Autonomous browser agent' row to the comparison table"
fi

# ── 5. Minimum Rust version ──────────────────────────────────────────────────
msrv=$(grep -E '^rust-version\s*=' Cargo.toml | head -1 | sed -E 's/.*"([^"]+)".*/\1/')
msrv_claim=$(grep -oE 'Rust [0-9]+\.[0-9]+\+' README.md | head -1 | grep -oE '[0-9]+\.[0-9]+')

if [ -n "$msrv" ] && [ "$msrv" = "$msrv_claim" ]; then
  ok "README.md \"Rust ${msrv_claim}+\" matches Cargo.toml rust-version"
else
  err "README.md claims \"Rust ${msrv_claim}+\" but Cargo.toml rust-version is \"${msrv}\""
  err "  fix: keep README.md's Cargo install line and Cargo.toml rust-version in step"
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
