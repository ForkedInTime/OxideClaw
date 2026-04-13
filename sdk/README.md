# RustyClaw SDK

Embed RustyClaw in any application. One binary, NDJSON over stdio, zero dependencies.

```bash
rustyclaw --headless
```

This starts a long-running server that reads JSON requests from stdin and writes JSON responses + streaming notifications to stdout. Pipe it from any language — Python, TypeScript, Go, shell scripts, CI/CD.

---

## Quick Start

### 1. Health check (5 seconds)

```bash
(echo '{"id":"1","type":"health/check"}'; sleep 1) | rustyclaw --headless
```

```json
{"type":"health/check","id":"1","status":"ok","version":"0.2.0","active_sessions":0,"uptime_seconds":0}
```

### 2. Ask a question

```bash
(echo '{"id":"1","type":"session/start","prompt":"What does the main function do?","max_turns":1}'; sleep 30) \
  | rustyclaw --headless 2>/dev/null
```

You'll see a stream of NDJSON lines:

```
{"type":"session/started","id":"1","session_id":"abc-123","model":"claude-sonnet-4-6"}
{"type":"message/delta","session_id":"abc-123","content":"The main"}
{"type":"message/delta","session_id":"abc-123","content":" function"}
...
{"type":"cost/updated","session_id":"abc-123","turn_cost_usd":0.003,...}
{"type":"context/health","session_id":"abc-123","used_pct":4,...}
{"type":"turn/completed","session_id":"abc-123","response":"The main function...","cost_usd":0.003,...}
```

### 3. Search the codebase (no API key needed)

```bash
(echo '{"id":"1","type":"rag/search","query":"authentication","limit":5}'; sleep 1) \
  | rustyclaw --headless 2>/dev/null
```

Returns matching code symbols from the RAG index.

---

## How It Works

```
Your App                    RustyClaw
────────                    ─────────
   │                            │
   │──── stdin (NDJSON) ───────>│  Requests
   │                            │
   │<─── stdout (NDJSON) ──────│  Responses + Notifications
   │                            │
   │     stderr ───────────────>│  Debug logs (ignored)
```

- **Requests** go to stdin, one JSON object per line
- **Responses** come from stdout, one JSON object per line
- Every message has a `"type"` field for dispatch
- Requests have an `"id"` field — responses echo it back for correlation
- Notifications stream without an `id` (they're server-initiated events)

---

## Request Types

| Type | Purpose | Response |
|------|---------|----------|
| `health/check` | Is the server alive? | `health/check` |
| `session/start` | Start a conversation + run first prompt | `session/started` + streaming |
| `session/list` | List saved sessions | `session/list` |
| `rag/search` | Search the codebase index | `rag/search` |
| `tool/approve` | Approve a pending tool execution | *(routed to session)* |
| `tool/deny` | Deny a pending tool execution | *(routed to session)* |

### Notification Types (streamed during a turn)

| Type | When |
|------|------|
| `message/delta` | Text chunk from the model |
| `thinking/delta` | Reasoning chunk (when `showThinkingSummaries` is on) |
| `tool/started` | Tool execution began |
| `tool/approval_needed` | Tool needs host approval |
| `tool/completed` | Tool finished |
| `cost/updated` | Token usage + cost after each API call |
| `model/routed` | Smart router switched models for this turn |
| `context/health` | Context window usage % |
| `progress/updated` | Estimated progress through the task |
| `turn/completed` | Turn finished — final response, total cost, duration |
| `error` | Something went wrong |

Full protocol reference: [protocol.md](protocol.md)

---

## Tool Approval

By default, the SDK asks for approval before running tools. Control this with the `policy` field on `session/start`:

```json
{
  "id": "1",
  "type": "session/start",
  "prompt": "Fix the failing tests",
  "policy": {
    "allow": ["Read", "Glob", "Grep"],
    "auto_approve": ["Edit", "Write"],
    "ask": ["Bash"],
    "deny": []
  }
}
```

**Priority order:** deny > ask > auto_approve > allow.

- **allow** — execute silently (no notification)
- **auto_approve** — execute with a `tool/started` notification
- **ask** — send `tool/approval_needed`, block until you respond with `tool/approve` or `tool/deny`
- **deny** — reject immediately

Tools not in any list default to `ask` (if `interactive_approval` is true in capabilities) or `deny`.

---

## Cost Tracking

Every API call streams a `cost/updated` notification:

```json
{
  "type": "cost/updated",
  "session_id": "abc-123",
  "turn_cost_usd": 0.003,
  "session_total_usd": 0.015,
  "budget_remaining_usd": 4.985,
  "input_tokens": 1200,
  "output_tokens": 85,
  "model": "claude-sonnet-4-6"
}
```

Set a budget limit with `max_budget_usd` on `session/start`. The session stops automatically when the budget is exceeded.

---

## Context Health

Every API call also streams a `context/health` notification:

```json
{
  "type": "context/health",
  "session_id": "abc-123",
  "used_pct": 42,
  "tokens_used": 84000,
  "tokens_max": 200000,
  "compaction_imminent": false
}
```

Monitor `used_pct` to know when the context window is filling up. `compaction_imminent` flips to `true` at 85%.

---

## Examples

See [`examples/`](examples/) for runnable scripts:

- [`health-check.sh`](examples/health-check.sh) — simplest possible test
- [`ask-question.sh`](examples/ask-question.sh) — send a prompt, read the response
- [`tool-approval.sh`](examples/tool-approval.sh) — interactive tool approval flow

---

## What's Coming (Phase B/C)

- gRPC transport + auto-generated Python/TypeScript/Go SDK clients
- Multiplexed sessions over one connection
- `session/resume` — resume a saved session
- `cost/report` — detailed cost breakdown by model and tool
- Agent orchestration (`agent/spawn`, `agent/subscribe`, `agent/merge`)
- WebSocket transport for browser embedding
- Diff preview/approve/rollback

---

## Integration Ideas

- **VS Code extension** — spawn `rustyclaw --headless`, pipe requests
- **CI/CD** — run code review or test generation as a build step
- **Chat UI** — build a web frontend that talks to RustyClaw over WebSocket (coming Phase C)
- **Scripts** — automate repetitive coding tasks with shell scripts
- **Monitoring** — poll `health/check` to verify your coding agent is alive
