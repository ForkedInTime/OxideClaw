#!/usr/bin/env bash
# Ask a question and stream the response.
# Requires ANTHROPIC_API_KEY to be set.

set -euo pipefail

PROMPT="${1:-What does the main function do in this project? Be brief.}"

(
  cat <<EOF
{"id":"1","type":"session/start","prompt":"$PROMPT","max_turns":1,"policy":{"allow":["Read","Glob","Grep"]}}
EOF
  # Keep stdin open while the model responds
  sleep 30
) | oxideclaw --headless 2>/dev/null | while IFS= read -r line; do
  TYPE=$(echo "$line" | jq -r '.type // empty')
  case "$TYPE" in
    session/started)
      echo "Session: $(echo "$line" | jq -r '.session_id')"
      echo "Model:   $(echo "$line" | jq -r '.model')"
      echo "---"
      ;;
    message/delta)
      printf '%s' "$(echo "$line" | jq -r '.content')"
      ;;
    turn/completed)
      echo ""
      echo "---"
      echo "Cost:     \$$(echo "$line" | jq -r '.cost_usd')"
      echo "Tokens:   $(echo "$line" | jq -r '.tokens.input') in / $(echo "$line" | jq -r '.tokens.output') out"
      echo "Duration: $(echo "$line" | jq -r '.duration_ms')ms"
      echo "Tools:    $(echo "$line" | jq -r '.tools_used | join(", ")')"
      break
      ;;
    error)
      echo "ERROR: $(echo "$line" | jq -r '.message')" >&2
      break
      ;;
  esac
done
