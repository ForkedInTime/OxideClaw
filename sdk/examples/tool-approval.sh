#!/usr/bin/env bash
# Interactive tool approval flow.
# The SDK asks permission before running Bash commands.
# Requires ANTHROPIC_API_KEY to be set.
#
# This example uses a named pipe (FIFO) so we can send requests
# at any time while the server is running.

set -euo pipefail

FIFO=$(mktemp -u)
mkfifo "$FIFO"
trap 'rm -f "$FIFO"' EXIT

# Start the server, reading from the FIFO
oxideclaw --headless < "$FIFO" 2>/dev/null &
SERVER_PID=$!

# Open the FIFO for writing (keeps it open)
exec 3>"$FIFO"

# Send a session/start that requires Bash (which needs approval)
cat >&3 <<'EOF'
{"id":"1","type":"session/start","prompt":"List the files in the current directory using ls -la","max_turns":1,"policy":{"allow":["Read","Glob","Grep"],"ask":["Bash"]}}
EOF

echo "Sent prompt. Waiting for tool approval request..."
echo ""

# Read server output line by line
while IFS= read -r line; do
  TYPE=$(echo "$line" | jq -r '.type // empty' 2>/dev/null) || continue

  case "$TYPE" in
    session/started)
      echo "[started] model=$(echo "$line" | jq -r '.model')"
      ;;
    message/delta)
      printf '%s' "$(echo "$line" | jq -r '.content')"
      ;;
    tool/approval_needed)
      APPROVAL_ID=$(echo "$line" | jq -r '.approval_id')
      TOOL=$(echo "$line" | jq -r '.tool')
      ARGS=$(echo "$line" | jq -r '.args')
      echo ""
      echo "---"
      echo "APPROVAL NEEDED: $TOOL"
      echo "Args: $ARGS"
      echo ""
      read -p "Approve? (y/n): " ANSWER
      if [ "$ANSWER" = "y" ]; then
        echo "{\"id\":\"approve-1\",\"type\":\"tool/approve\",\"approval_id\":\"$APPROVAL_ID\"}" >&3
        echo "[approved]"
      else
        echo "{\"id\":\"deny-1\",\"type\":\"tool/deny\",\"approval_id\":\"$APPROVAL_ID\",\"reason\":\"User denied\"}" >&3
        echo "[denied]"
      fi
      ;;
    tool/completed)
      echo "[tool done] $(echo "$line" | jq -r '.tool') ($(echo "$line" | jq -r '.duration_ms')ms)"
      ;;
    turn/completed)
      echo ""
      echo "---"
      echo "Turn complete. Cost: \$$(echo "$line" | jq -r '.cost_usd')"
      break
      ;;
    error)
      echo "ERROR: $(echo "$line" | jq -r '.message')" >&2
      break
      ;;
  esac
done < <(cat /proc/$SERVER_PID/fd/1 2>/dev/null || wait $SERVER_PID)

# Clean up
exec 3>&-
wait $SERVER_PID 2>/dev/null || true
