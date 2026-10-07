# OxideClaw SDK Protocol Reference

NDJSON (newline-delimited JSON) over stdio. One JSON object per line.

- **Requests** (host -> OxideClaw): have `"id"` and `"type"` fields
- **Responses** (OxideClaw -> host): have `"id"` (matching the request) and `"type"` fields
- **Notifications** (OxideClaw -> host): have `"session_id"` and `"type"` fields, no `"id"`

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
  "version": "0.3.2",
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
  "policy": {
    "allow": ["Read", "Glob", "Grep"],
    "auto_approve": ["Edit", "Write"],
    "ask": ["Bash"],
    "deny": [],
    "approval_timeout_seconds": 60
  },
  "capabilities": {
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
| `cwd` | no | server cwd | Project directory: tools run there, and its CLAUDE.md, AGENTS.md and project settings apply |
| `model` | no | from config | Model name (e.g. `claude-sonnet-5`, `ollama:llama3`) |
| `max_turns` | no | 50 | Max agentic loop iterations |
| `max_budget_usd` | no | unlimited | Budget cap |
| `record` | no | false | Reserved: accepted but currently ignored. SDK sessions are not saved, so they do not appear in `session/list`. |
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

The index must be built first: the TUI builds it automatically when started inside a git repository, or run `/rag index` there. It lives in `$XDG_CACHE_HOME/oxideclaw/rag/` (default `~/.cache/oxideclaw/rag/`), skips everything git ignores, and is never built for the home directory or `/`. Without one, `rag/search` returns a `rag_search_failed` error saying so; it never creates an index. Result paths are relative to the session's working directory when below it, absolute elsewhere in the repository.

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

### `browse/start`

Run the autonomous browser agent toward a goal. `policy` is `pattern` (default: only actions matching the approval patterns ask), `ask` (every non-read-only action asks) or `yolo` (no approvals; requires `"yolo_ack": true`). `max_steps` defaults to `browseMaxSteps`.

```json
{"id": "6", "type": "browse/start", "goal": "Find the latest release notes on example.com", "policy": "pattern", "max_steps": 30}
```

**Response:**

```json
{"type": "browse/started", "id": "6", "session_id": "browse-1712345678901"}
```

Progress then streams as `browse/progress`, `browse/approval_needed` and exactly one `browse/completed` per run, including runs that fail before the first step.

---

### `browse/approval_reply`

Answer a `browse/approval_needed` prompt. It has no `id` and gets no response. `session_id` and `approval_id` must match the pending prompt; anything else is logged to stderr and ignored. `step` is optional and informational: a step number repeats after a denied or expired prompt, so it cannot identify one. An unanswered prompt is denied after 60 seconds.

```json
{"type": "browse/approval_reply", "session_id": "browse-1712345678901", "approval_id": 7, "step": 4, "approved": true}
```

---

## Notifications

Notifications are streamed from the server during turn execution. They have `session_id` but no `id`.

### `message/delta`

A text chunk from the model's response. Collect these to build the full response.

```json
{"type": "message/delta", "session_id": "abc-123", "content": "Looking at the code"}
```

---

### `thinking/delta`

The model's reasoning for one response, one notification per non-empty thinking block once that response has finished streaming. Sent only when `showThinkingSummaries` is on in settings.json; models or backends that return no thinking blocks never send it.

```json
{"type": "thinking/delta", "session_id": "abc-123", "content": "The failing test points at the parser"}
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

If no response within `approval_timeout_seconds` (default 60), the tool is automatically denied. A denied or timed-out call is closed with a `tool/completed` carrying `success: false` and `duration_ms: 0`.

---

### `tool/completed`

A tool finished executing, or a call waiting on `tool/approval_needed` was denied or timed out (`success: false`).

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

`tokens_max` is the session model's context window (1,000,000 on Opus/Sonnet 4.6+, Claude 5 and Fable; 200,000 on Haiku 4.5 and older models). `compaction_imminent` is `true` when `used_pct >= 85`.

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

Error codes:

| Code | Meaning |
|------|---------|
| `budget_exceeded` | The session spent its `max_budget_usd`. |
| `max_turns` | The turn hit `max_turns` agentic iterations before the model finished. |
| `max_tokens` | The model hit its output-token or context-window limit. |
| `refusal` | The model declined the request. |
| `turn_error` | The turn failed (API, network or tool-loop error); `message` has the cause. |

---

### `browse/progress`

A browser action the agent took.

```json
{"type": "browse/progress", "session_id": "browse-1712345678901", "step": 3, "action": "browser_click", "target": "Releases"}
```

---

### `browse/approval_needed`

The approval gate is holding an action. Answer with `browse/approval_reply`.

```json
{
  "type": "browse/approval_needed",
  "session_id": "browse-1712345678901",
  "approval_id": 7,
  "step": 4,
  "tool_name": "browser_click",
  "target_text": "Delete repository",
  "url": "https://example.com/settings",
  "reason": "destructive action"
}
```

---

### `browse/completed`

The run ended. `reason` is one of `done`, `bailed`, `step_cap`, `stagnation`, `budget`, `browser_crashed`, `user_denied`, `cancelled`. A run that fails to start (for example, no credential) reports `achieved: false`, `reason: "bailed"` and the error in `summary`.

```json
{
  "type": "browse/completed",
  "session_id": "browse-1712345678901",
  "result": {"achieved": true, "summary": "Release notes for v2.1 ...", "reason": "done", "steps_used": 6, "final_url": "https://example.com/releases"}
}
```

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

Error codes:

| Code | Meaning |
|------|---------|
| `parse_error` | The line is not JSON. |
| `invalid_request` | The line is JSON but not a known request. |
| `not_implemented` | The request type is reserved but not implemented yet. |
| `no_session` | `tool/approve` or `tool/deny` arrived while no session was running to receive it. |
| `invalid_cwd` | `session/start` named a `cwd` that cannot be used. |
| `session_create_failed` | The session could not be created (for example, no credential). |
| `session_list_failed` | Saved sessions could not be listed. |
| `rag_search_failed` | The codebase index search failed. |
| `yolo_ack_required` | `browse/start` used `policy: "yolo"` without `yolo_ack: true`. |

A line that is not a valid request still gets an `error` reply: `parse_error` when it is not JSON, `invalid_request` when it is JSON but not a known request (unknown `type`, a missing required field, or a non-string `id`). The reply echoes the line's `id` when it has one (a numeric `id` comes back as a string) and is `""` otherwise.

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
  "open_browser": false,
  "play_audio": false,
  "interactive_approval": true,
  "supports_images": false,
  "max_file_size_bytes": 1048576
}
```

| Field | Default | Effect |
|-------|---------|--------|
| `show_diff` | `true` | Reserved: accepted but currently has no effect |
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
