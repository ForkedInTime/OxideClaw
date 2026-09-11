#!/usr/bin/env bash
# Record the OxideClaw hero demo for the README.
#
# What this does:
#   1. Starts asciinema at 120x30 with idle pauses compressed to 2 sec.
#   2. Launches ./target/release/oxideclaw inside the recording.
#   3. You drive the 8-scene demo manually (see SCENES below).
#   4. When you /exit oxideclaw, the recording stops automatically.
#
# Output: assets/demo.cast
#
# Usage:
#   ./assets/record-demo.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CAST_FILE="$SCRIPT_DIR/demo.cast"
BINARY="$REPO_ROOT/target/release/oxideclaw"

if [[ ! -x "$BINARY" ]]; then
  echo "ERROR: $BINARY not found. Run 'cargo build --release' first." >&2
  exit 1
fi

if [[ -f "$CAST_FILE" ]]; then
  echo "Existing $CAST_FILE will be overwritten. Ctrl-C within 3 sec to abort."
  sleep 3
  rm -f "$CAST_FILE"
fi

cat <<'EOF'
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 OxideClaw — demo recording
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

 SCENES (target: ~60 sec total)

   1. Let the banner breathe for ~2 sec
   2. what does src/session/mod.rs do?
   3. /rag search TOCTOU
   4. /model    (then Esc to close picker)
   5. /budget
   6. /voice    (then Esc to close picker)
   7. /spawn refactor the banner config
   8. /exit

 When oxideclaw exits, the .cast will be saved automatically.

 Starting in 3 sec...
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
EOF

sleep 3

asciinema rec \
  --cols 120 \
  --rows 30 \
  --idle-time-limit 2 \
  --title "OxideClaw — AI coding CLI in Rust" \
  --command "$BINARY" \
  "$CAST_FILE"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " Recording saved: $CAST_FILE"
echo ""
echo " Preview it with:"
echo "   asciinema play $CAST_FILE"
echo ""
echo " When happy, convert to SVG for the README:"
echo "   svg-term --in $CAST_FILE --out $SCRIPT_DIR/demo.svg --window --width 120 --height 30"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
