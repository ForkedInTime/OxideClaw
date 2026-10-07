#!/usr/bin/env bash
# Ask a question and stream the response.
# Requires ANTHROPIC_API_KEY to be set.

set -euo pipefail

PROMPT="${1:-What does the main function do in this project? Be brief.}"

(
  # jq escapes quotes and newlines in the prompt. Reading files and then
  # answering takes several agentic turns, so leave max_turns room.
  jq -nc --arg p "$PROMPT" \
    '{id:"1", type:"session/start", prompt:$p, max_turns:10, policy:{allow:["Read","Glob","Grep"]}}'
  # Keep stdin open while the model responds; EOF stops the server.
  sleep 60
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
