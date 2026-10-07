#!/usr/bin/env bash
# Interactive tool approval flow.
# The SDK asks permission before running Bash commands.
# Requires ANTHROPIC_API_KEY to be set.
#
# The server talks NDJSON over two named pipes (FIFOs): one we write
# requests into at any time, one we read its events from. Your y/n answer
# comes from the terminal, not from either pipe.

set -euo pipefail

DIR=$(mktemp -d)
IN="$DIR/in"
OUT="$DIR/out"
mkfifo "$IN" "$OUT"
trap 'rm -rf "$DIR"' EXIT

oxideclaw --headless < "$IN" > "$OUT" 2>/dev/null &
SERVER_PID=$!

# Hold the request pipe open for the whole conversation; closing it is EOF,
# which shuts the server down.
exec 3>"$IN"

# Bash is in "ask", so running it needs approval. Calling a tool and then
# answering takes more than one agentic turn, so leave max_turns room.
jq -nc '{id:"1", type:"session/start",
         prompt:"List the files in the current directory using ls -la",
         max_turns:10,
         policy:{allow:["Read","Glob","Grep"], ask:["Bash"]}}' >&3

echo "Sent prompt. Waiting for tool approval request..."
echo ""

N=0
while IFS= read -r line; do
  TYPE=$(jq -r '.type // empty' <<<"$line" 2>/dev/null) || continue

  case "$TYPE" in
    session/started)
      echo "[started] model=$(jq -r '.model' <<<"$line")"
      ;;
    message/delta)
      printf '%s' "$(jq -r '.content' <<<"$line")"
      ;;
    tool/approval_needed)
      APPROVAL_ID=$(jq -r '.approval_id' <<<"$line")
      echo ""
      echo "---"
      echo "APPROVAL NEEDED: $(jq -r '.tool' <<<"$line")"
      echo "Args: $(jq -c '.args' <<<"$line")"
      echo ""
      read -r -p "Approve? (y/n): " ANSWER </dev/tty
      N=$((N + 1))
      if [ "$ANSWER" = "y" ]; then
        jq -nc --arg id "approve-$N" --arg a "$APPROVAL_ID" \
          '{id:$id, type:"tool/approve", approval_id:$a}' >&3
        echo "[approved]"
      else
        jq -nc --arg id "deny-$N" --arg a "$APPROVAL_ID" \
          '{id:$id, type:"tool/deny", approval_id:$a, reason:"User denied"}' >&3
        echo "[denied]"
      fi
      ;;
    tool/completed)
      echo "[tool done] $(jq -r '.tool' <<<"$line") ($(jq -r '.duration_ms' <<<"$line")ms)"
      ;;
    turn/completed)
      echo ""
      echo "---"
      echo "Turn complete. Cost: \$$(jq -r '.cost_usd' <<<"$line")"
      break
      ;;
    error)
      echo "ERROR: $(jq -r '.message' <<<"$line")" >&2
      break
      ;;
  esac
done < "$OUT"

# EOF on the request pipe stops the server.
exec 3>&-
wait "$SERVER_PID" 2>/dev/null || true
