# RustyClaw SDK Protocol Reference

NDJSON (newline-delimited JSON) over stdio. One JSON object per line.

- **Requests** (host -> RustyClaw): have `"id"` and `"type"` fields
- **Responses** (RustyClaw -> host): have `"id"` (matching the request) and `"type"` fields
- **Notifications** (RustyClaw -> host): have `"session_id"` and `"type"` fields, no `"id"`

---

## Requests

### `health/check`

Check if the server is alive. No API key needed.

```json
{"id": "1", "type": "health/check"}
```

**Response:**

```json
{
  "type": "health/check",
  "id": "1",
  "status": "ok",
  "version": "0.3.1",
  "protocol_version": 1,
  "active_sessions": 0,
  "uptime_seconds": 42
}
```

`protocol_version` is the wire-compatibility number. It changes only when a request, response
or notification shape changes incompatibly; new optional fields do not bump it. Hosts should
gate on it rather than on `version` (the crate version).

---

### `session/start`

Start a new conversation and execute the first prompt.

```json
{
  "id": "req-1",
  "type": "session/start",
  "prompt": "Fix the failing tests in src/auth.rs",
  "cwd": "/home/user/project",
  "model": "claude-sonnet-5",
  "max_turns": 10,
  "max_budget_usd": 5.0,
  "record": true,
  "policy": {
    "allow": ["Read", "Glob", "Grep"],
    "auto_approve": ["Edit", "Write"],
    "ask": ["Bash"],
    "deny": [],
    "approval_timeout_seconds": 60
  },
  "capabilities": {
    "show_diff": true,
    "open_browser": false,
    "play_audio": false,
    "interactive_approval": true,
    "supports_images": false,
    "max_file_size_bytes": 1048576
  }
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | yes | | Request correlation ID |
| `prompt` | yes | | The user's message |
| `cwd` | no | server cwd | Working directory for tools |
| `model` | no | from config | Model name (e.g. `claude-sonnet-5`, `ollama:llama3`) |
| `max_turns` | no | 50 | Max agentic loop iterations |
| `max_budget_usd` | no | unlimited | Budget cap |
| `record` | no | false | Save session to disk |
| `policy` | no | ask all | Tool approval policy |
| `capabilities` | no | see below | Host environment capabilities |

**Immediate response:**

```json
{
  "type": "session/started",
  "id": "req-1",
  "session_id": "65f9c008-dcef-4d7b-8f16-3f3fb13e7409",
  "model": "claude-sonnet-5"
}
```

Then a stream of notifications follows (see [Notifications](#notifications)).

---

### `session/list`

List saved sessions from disk.

```json
{"id": "2", "type": "session/list", "limit": 10}
```

**Response:**

```json
{
  "type": "session/list",
  "id": "2",
  "sessions": [
    {
      "id": "832b9543-c5ca-461c-a0ac-f2bcc8b787aa",
      "name": "Wed Apr 8, 9:57 PM",
      "created_at": "1775710660",
      "preview": "Fix auth middleware..."
    }
  ]
}
```

---

### `rag/search`

Search the codebase index. No API key needed — uses the local SQLite FTS5 index.

```json
{"id": "3", "type": "rag/search", "query": "authentication handler", "limit": 5}
```

**Response:**

```json
{
  "type": "rag/search",
  "id": "3",
  "results": [
    {
      "file": "src/auth.rs",
      "line": 42,
      "symbol": "handle_auth",
      "kind": "function",
      "snippet": "pub async fn handle_auth(req: Request) -> Response { ... }"
    }
  ]
}
```

The index must be built first (run `rustyclaw` interactively and use `/rag index`, or the index is built automatically on first run).

---

### `tool/approve`

Approve a pending tool execution. Sent in response to a `tool/approval_needed` notification.

```json
{"id": "4", "type": "tool/approve", "approval_id": "appr-abc123"}
```

---

### `tool/deny`

Deny a pending tool execution with an optional reason.

```json
{"id": "5", "type": "tool/deny", "approval_id": "appr-abc123", "reason": "No shell access in CI"}
```

The reason is passed back to the model so it can adapt its approach.

---

## Notifications

Notifications are streamed from the server during turn execution. They have `session_id` but no `id`.

### `message/delta`

A text chunk from the model's response. Collect these to build the full response.

```json
{"type": "message/delta", "session_id": "abc-123", "content": "Looking at the code"}
```

---

### `tool/started`

A tool is about to execute (sent for `auto_approve` tools).

```json
{
  "type": "tool/started",
  "session_id": "abc-123",
  "tool": "Edit",
  "args": {"file_path": "/src/main.rs", "old_string": "foo", "new_string": "bar"},
  "tool_use_id": "toolu_abc"
}
```

---

### `tool/approval_needed`

A tool requires host approval before executing. Respond with `tool/approve` or `tool/deny`.

```json
{
  "type": "tool/approval_needed",
  "session_id": "abc-123",
  "approval_id": "appr-abc123",
  "tool": "Bash",
  "args": {"command": "rm -rf /tmp/test"},
  "tool_use_id": "toolu_xyz"
}
```

If no response within `approval_timeout_seconds` (default 60), the tool is automatically denied.

---

### `tool/completed`

A tool finished executing.

```json
{
  "type": "tool/completed",
  "session_id": "abc-123",
  "tool": "Edit",
  "tool_use_id": "toolu_abc",
  "success": true,
  "output_summary": "Applied edit to src/main.rs",
  "duration_ms": 12
}
```

---

### `cost/updated`

Token usage and cost after each API call.

```json
{
  "type": "cost/updated",
  "session_id": "abc-123",
  "turn_cost_usd": 0.003,
  "session_total_usd": 0.015,
  "budget_remaining_usd": 4.985,
  "input_tokens": 1200,
  "output_tokens": 85,
  "model": "claude-sonnet-5"
}
```

`budget_remaining_usd` is `null` if no budget was set.

---

### `context/health`

Context window usage after each API call.

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

`compaction_imminent` is `true` when `used_pct >= 85`.

---

### `progress/updated`

Estimated progress through the current task.

```json
{
  "type": "progress/updated",
  "session_id": "abc-123",
  "percent": 35,
  "stage": "Turn 2/10",
  "tools_executed": 4,
  "tools_remaining_estimate": 0
}
```

---

### `turn/completed`

The turn is finished. Contains the full response and summary stats.

```json
{
  "type": "turn/completed",
  "session_id": "abc-123",
  "response": "I fixed the failing tests by...",
  "structured_output": null,
  "cost_usd": 0.015,
  "total_session_cost_usd": 0.015,
  "tokens": {"input": 14000, "output": 250},
  "model": "claude-sonnet-5",
  "tools_used": ["Read", "Edit", "Bash"],
  "duration_ms": 12500
}
```

---

### `error`

Something went wrong during the turn.

```json
{
  "type": "error",
  "session_id": "abc-123",
  "code": "budget_exceeded",
  "message": "Budget exceeded: $5.0012"
}
```

Error codes: `budget_exceeded`, `max_turns_exceeded`, `turn_error`, `internal_error`.

---

## Error Responses

Request-level errors include the request `id`:

```json
{
  "type": "error",
  "id": "req-1",
  "code": "not_implemented",
  "message": "This request type is not yet implemented"
}
```

Error codes: `internal_error`, `not_implemented`, `session_not_found`.

---

## Policy Reference

The `policy` object on `session/start` controls tool approval:

```json
{
  "allow": ["Read", "Glob", "Grep"],
  "auto_approve": ["Edit", "Write"],
  "ask": ["Bash"],
  "deny": ["WebFetch"],
  "approval_timeout_seconds": 60
}
```

**Evaluation order:** deny > ask > auto_approve > allow.

| List | Behavior | Notification |
|------|----------|-------------|
| `deny` | Rejected immediately | None |
| `ask` | Blocks until host responds | `tool/approval_needed` |
| `auto_approve` | Executes immediately | `tool/started` |
| `allow` | Executes silently | None |
| *(unlisted)* | `ask` if interactive, `deny` if not | Depends |

---

## Capabilities Reference

The `capabilities` object tells the agent what the host environment supports:

```json
{
  "show_diff": true,
  "open_browser": false,
  "play_audio": false,
  "interactive_approval": true,
  "supports_images": false,
  "max_file_size_bytes": 1048576
}
```

| Field | Default | Effect |
|-------|---------|--------|
| `show_diff` | `true` | If false, agent describes changes in text |
| `open_browser` | `true` | If false, agent provides URLs as text |
| `play_audio` | `false` | If true, agent may use voice features |
| `interactive_approval` | `true` | If false, unlisted tools are denied instead of asked |
| `supports_images` | `false` | If true, agent may include image content |
| `max_file_size_bytes` | `null` | Max file size the host can handle |

---

## Transport Notes

- One JSON object per line (NDJSON)
- Max line size: 4MB
- Blank lines are ignored
- Stderr is used for debug logs (redirect to /dev/null in production)
- Close stdin to shut down the server
- UTF-8 encoding required
