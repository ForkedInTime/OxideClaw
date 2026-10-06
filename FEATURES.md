# OxideClaw — Complete Feature Reference

Everything available in OxideClaw, organized by category.

---

## Table of Contents

- [Models & Providers](#models--providers)
- [Slash Commands](#slash-commands)
- [Tools](#tools)
- [Voice & TTS](#voice--tts)
- [RAG Indexing](#rag-indexing)
- [Smart Model Router](#smart-model-router)
- [Cost Tracking](#cost-tracking)
- [Session Management](#session-management)
- [SDK / Headless Mode](#sdk--headless-mode)
- [Editor Integration (ACP)](#editor-integration-acp)
- [Hooks](#hooks)
- [Sandboxing](#sandboxing)
- [Configuration](#configuration)
- [Keyboard Shortcuts](#keyboard-shortcuts)
- [Environment Variables](#environment-variables)

---

## Models & Providers

### Claude (Anthropic API)

Set `ANTHROPIC_API_KEY` in your `.env` or environment. All Claude models are supported.

```
/model claude-sonnet-4-6
/model claude-opus-4-6
/model claude-haiku-4-5
```

### Ollama (Local Models)

```bash
ollama pull dolphin3
oxideclaw
```

```
/model ollama:dolphin3     # switch to local model
/model                     # interactive picker (Claude + Ollama)
/model default             # back to Claude
```

Models that don't support tool use get automatic text-only fallback. Ollama models are always free in cost tracking.

### OpenAI-Compatible Providers

OxideClaw supports any OpenAI-compatible API endpoint. Pick one with a
`<prefix>:<model>` model string; the API key is read from that provider's
own environment variable (shell or `.env`):

| Provider | Model prefix | API key variable |
|----------|--------------|------------------|
| Groq | `groq:` | `GROQ_API_KEY` |
| OpenRouter | `openrouter:` | `OPENROUTER_API_KEY` |
| DeepSeek | `deepseek:` | `DEEPSEEK_API_KEY` |
| LM Studio | `lmstudio:` | none (`LM_STUDIO_HOST` overrides `http://localhost:1234/v1`) |
| Together | `together:` | `TOGETHER_API_KEY` |
| Mistral | `mistral:` | `MISTRAL_API_KEY` |
| Venice.ai | `venice:` | `VENICE_API_KEY` |
| OpenAI | `oai:` | `OPENAI_API_KEY` |
| Generic | `openai-compat:` | `OPENAI_API_KEY`, plus `OPENAI_BASE_URL` (required) |

Keys come from the environment, never from `settings.json`. Export the variable or put it in `~/.env` or `~/.config/oxideclaw/.env`, then pick the model:

```bash
echo 'GROQ_API_KEY=gsk_...' >> ~/.config/oxideclaw/.env
oxideclaw --model groq:llama-3.3-70b-versatile
```

Or switch at runtime:

```
/model groq:llama-3.3-70b-versatile
/model oai:gpt-4o
```

Each provider reads only its own variable, so your OpenAI key is never sent to Groq or OpenRouter.

---

## Slash Commands

### Navigation & Help

| Command | Description |
|---------|-------------|
| `/help` | Interactive two-level command menu |
| `/doctor` | Verify setup — API keys, voice, MCP, system tools |
| `/version` | Show version and build info |
| `/clear` | Clear chat history |

### Model Management

| Command | Description |
|---------|-------------|
| `/model` | Interactive model picker (Claude + Ollama) |
| `/model <name>` | Switch to specific model |
| `/model default` | Reset to default Claude model |
| `/model list` | List all available models |
| `/effort [low\|medium\|high\|max\|off]` | Set the API effort level (`output_config.effort`); prompt nudge on models without it |

### Session Management

| Command | Description |
|---------|-------------|
| `/session` | Browse and resume sessions |
| `/session list` | List saved sessions |
| `/session save` | Save current session |
| `/session export` | Export session to file |
| `/session delete` | Delete a session |

### Voice & TTS

| Command | Description |
|---------|-------------|
| `/voice enable` | Enable voice input |
| `/voice disable` | Disable voice input |
| `/voice speak on` | Enable TTS responses |
| `/voice speak off` | Disable TTS responses |
| `/voice model` | Interactive voice model picker |

### RAG (Codebase Indexing)

| Command | Description |
|---------|-------------|
| `/rag index` | Index current directory |
| `/rag search <query>` | Search the index |
| `/rag status` | Show index stats |
| `/rag clear` | Clear the index |

### Cost & Budget

| Command | Description |
|---------|-------------|
| `/cost` | Show session cost breakdown |
| `/budget <amount>` | Set budget limit (e.g., `/budget $5`) |
| `/budget clear` | Remove budget limit |

### Settings

| Command | Description |
|---------|-------------|
| `/reload` | Hot-reload settings, CLAUDE.md, AGENTS.md |
| `/config` | Show current configuration |

### Tools & MCP

| Command | Description |
|---------|-------------|
| `/mcp` | List MCP plugins |
| `/mcp add <uri>` | Add MCP plugin |
| `/tools` | List available tools |

---

## Tools

OxideClaw includes 30+ built-in tools that the AI agent can use:

### File System

| Tool | Description |
|------|-------------|
| `Read` | Read file contents |
| `Write` | Create or overwrite files |
| `Edit` | Precise string replacements in files |
| `Glob` | Find files by pattern |
| `Grep` | Search file contents with regex |

### Execution

| Tool | Description |
|------|-------------|
| `Bash` | Execute shell commands |
| `Agent` | Spawn sub-agents for parallel work |

### Web

| Tool | Description |
|------|-------------|
| `WebFetch` | Fetch URLs |
| `WebSearch` | Search the web |

### Advanced

| Tool | Description |
|------|-------------|
| `LSP` | Language Server Protocol integration |
| `NotebookEdit` | Edit Jupyter notebooks |
| `MCP` | Model Context Protocol plugins |

---

## Voice & TTS

### Voice Input (STT)

Uses Whisper for speech-to-text. Press `Ctrl+R` to start/stop recording.

### Text-to-Speech

Powered by XTTS v2. Supports voice cloning — speak in your own voice. GPU-accelerated when CUDA is available, falls back to CPU.

Run `/doctor` to check if your TTS setup is working, or `/voice test` to hear a quick sample.

### Voice Commands

| Command | Description |
|---------|-------------|
| `/voice` | Show voice input/output status |
| `/voice enable` | Enable voice input |
| `/voice disable` | Disable voice input |
| `/voice speak on` | Enable TTS responses |
| `/voice speak off` | Disable TTS responses |
| `/voice model` | Interactive voice model picker with preview |
| `/voice test` | Play a test TTS sample to verify setup |
| `/voice clone` | Record a custom voice for TTS (XTTS v2) |
| `/voice clone save` | Save a cloned voice |
| `/voice clone remove` | Remove a cloned voice |

---

## RAG Indexing

Local codebase search powered by tree-sitter AST parsing and SQLite FTS5.

### Supported Languages

Rust, Python, JavaScript, TypeScript, Go, Java, C, C++

### How It Works

1. tree-sitter parses source files into AST nodes (functions, structs, classes, methods)
2. Symbols and code snippets are stored in a local SQLite database with FTS5 full-text search
3. Queries match against symbol names, file paths, and code content
4. Results are ranked by relevance and injected into the AI context

### Usage

```
/rag index           # index the current directory
/rag search "auth"   # search for symbols/code matching "auth"
/rag status          # show index statistics
/rag clear           # clear the index
```

The index auto-updates when files change between queries.

---

## Smart Model Router

Automatically routes tasks to the most cost-effective model based on complexity analysis.

| Complexity | Routed To | Example |
|-----------|-----------|---------|
| Low | Haiku / Ollama | "What does this function do?" |
| Medium | Sonnet | "Refactor this module" |
| High | Opus | "Debug this race condition" |

The router analyzes prompt length, keyword signals (debug, refactor, audit), and context to classify complexity. Cost savings are tracked and shown in `/cost`.

---

## Cost Tracking

Real-time token usage and cost monitoring per session.

```
/cost               # show cost breakdown
/budget $5          # set budget limit
/budget clear       # remove limit
```

The status bar shows running cost. Budget limits halt execution before overspending.

### Per-Model Pricing

All Claude model pricing is built in. Ollama models are always free. OpenAI-compatible providers use configurable pricing.

---

## Session Management

Save, resume, search, and export conversations.

```
/session             # interactive session browser with previews
/session list        # list saved sessions
/session save        # save current session
/session export      # export to file
```

Sessions are stored in `$XDG_DATA_HOME/oxideclaw/sessions/` (default: `~/.local/share/oxideclaw/sessions/`).

---

## SDK / Headless Mode

Embed OxideClaw in editors, CI/CD, scripts, or custom UIs.

```bash
oxideclaw --headless
```

Starts a long-running NDJSON server on stdin/stdout. Full protocol reference: [`sdk/`](sdk/).

```bash
# Health check
(echo '{"id":"1","type":"health/check"}'; sleep 1) | oxideclaw --headless

# Ask a question
(echo '{"id":"1","type":"session/start","prompt":"What is 2+2?","max_turns":1}'; sleep 15) \
  | oxideclaw --headless 2>/dev/null
```

Features: streaming responses, tool approval policies, cost tracking, context health monitoring, RAG search, session management.

---

## Editor Integration (ACP)

`oxideclaw acp` runs OxideClaw as an [Agent Client Protocol](https://agentclientprotocol.com) agent: JSON-RPC 2.0 over stdio, one line per message. Any ACP client can drive it.

**Zed** (`settings.json`):

```json
{
  "agent_servers": {
    "OxideClaw": { "command": "oxideclaw", "args": ["acp"] }
  }
}
```

| ACP method / update | OxideClaw behaviour |
|---------------------|---------------------|
| `initialize` | Protocol version 1. Advertises `embeddedContext`; no image/audio prompts, no `loadSession`, no HTTP/SSE MCP. |
| `authenticate` | No-op. Credentials come from the normal chain (`ANTHROPIC_API_KEY`, `ant` profile, settings). |
| `session/new` | Requires an existing `cwd`; the session reads that directory's CLAUDE.md, AGENTS.md and project settings. Stdio `mcpServers` entries are recorded on the session config. |
| `session/prompt` | Text, `resource_link`, and embedded text resources are flattened into one prompt. Answers with `stopReason`: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal` (budget exceeded), or `cancelled`. |
| `session/update` | `agent_message_chunk`, `agent_thought_chunk`, `tool_call` (kind + title + raw input), `tool_call_update` (status + output summary). |
| `session/request_permission` | Sent for every tool the SDK policy marks *ask* (the default for tools not on an allow list). Options: allow once / reject once. A `cancelled` outcome denies the tool. |
| `session/cancel` | Stops the in-flight model stream and skips queued tools; the prompt is answered with `cancelled`. |
| `session/load`, `session/set_mode` | Not supported (`-32601`). |

Errors use JSON-RPC codes: `-32602` invalid params (bad `cwd`, unknown session, media prompt), `-32000` when a prompt is already running, `-32601` unsupported method.

---

## Hooks

User-defined shell commands that run at lifecycle events. Configure them under `"hooks"` in `settings.json`; each entry is `{ "matcher": "<tool name or *>", "command": "<shell command>" }`. The command runs via `$SHELL -c` when `$SHELL` is a POSIX-family shell (sh, bash, dash, zsh, ksh, mksh, ash, yash) and via `sh -c` otherwise, so fish or nu users' POSIX hooks still parse.

| Event | When | Environment |
|-------|------|-------------|
| `preToolUse` | Before a tool runs. Exit 2 blocks it. | `TOOL_NAME`, `TOOL_INPUT` |
| `postToolUse` | After a tool completes | `TOOL_NAME`, `TOOL_RESULT` |
| `userPromptSubmit` | When you send a message in the TUI. Stdout is appended as context; exit 2 (or `{"continue": false}`) stops the prompt before it is sent and puts it back in the input box. | `CLAUDE_MESSAGE` |
| `notification` | When a turn ends with a text reply (the reply's text blocks, joined) | `CLAUDE_MESSAGE` |
| `stop` | When the session ends | — |
| `sessionStart` | When a session begins | — |
| `preCompact` | Before a compact/summarize cycle | — |
| `postCompact` | After a compact/summarize cycle | — |

Every hook also receives `CLAUDE_HOOK_EVENT`, `CLAUDE_SESSION_ID`, and `CLAUDE_CWD`. Environment values over 64 KiB are cut (the OS limits one variable to about 128 KiB), and `TOOL_INPUT_TRUNCATED=1` (or `TOOL_RESULT_TRUNCATED`, `CLAUDE_MESSAGE_TRUNCATED`) is set when that happens. The full, uncapped event is always written to the hook's stdin as one JSON object: `hook_event_name`, `session_id`, `cwd`, and when present `tool_name`, `tool_input` (the parsed tool arguments), `tool_response`, and `prompt`. A guard that pattern-matches tool input should read stdin, for example `jq -r .tool_input.command`, so padding cannot push the dangerous part out of view.

`preToolUse`, `postToolUse` and `userPromptSubmit` also run in `-p`, `--headless` (SDK) and `oxideclaw acp`. There a `preToolUse` block runs before any host approval prompt, so the host is never asked about a call the guard refuses. `notification`, `stop`, `sessionStart` and the compact events are interactive-only.

```json
{
  "hooks": {
    "preToolUse": [
      { "matcher": "Bash", "command": "./scripts/guard.sh" }
    ],
    "postToolUse": [
      { "matcher": "*", "command": "echo \"$TOOL_NAME\" >> ~/.cache/oxideclaw/tool.log" }
    ]
  }
}
```

**Exit codes:** `0` allow and continue · `2` block (the tool is not run, or the turn stops; stdout or `stopReason` is shown) · anything else is logged and ignored.

**JSON on stdout (optional):** `{ "decision": "approve" | "block", "reason": "...", "continue": false, "stopReason": "...", "systemMessage": "...", "additionalContext": "..." }`.

Each hook has a 60-second timeout and runs in its own process group, so a timed-out hook cannot leave children behind. `"disableAllHooks": true` in settings or `--bare` on the command line skips every hook.

---

## Sandboxing

OxideClaw supports multiple sandbox backends for tool isolation:

| Backend | Description |
|---------|-------------|
| `bwrap` | bubblewrap — lightweight Linux sandboxing |
| `firejail` | Firejail — security sandbox with profiles |
| `strict` | Best-effort denylist of catastrophic command literals — NOT isolation; no filesystem or network restriction (the only option on macOS/Windows) |

The project directory stays writable inside the sandbox, `.git/` included, so OxideClaw's own git snapshots (auto-commit, `/undo`, `/redo`) never run repository hooks or `core.fsmonitor`, and they stop with a warning if a repo-local `filter.*` driver appears or changes mid-session instead of running it outside the sandbox. Restart after reviewing `.git/config` to resume them.

---

## Configuration

### Settings File

`~/.config/oxideclaw/settings.json` (or `$XDG_CONFIG_HOME/oxideclaw/settings.json`):

```json
{
  "model": "claude-sonnet-5",
  "showThinkingSummaries": true,
  "spinnerStyle": "themed"
}
```

| Setting | Values | Default | Description |
|---------|--------|---------|-------------|
| `model` | any model name | `claude-sonnet-5` | Default model |
| `showThinkingSummaries` | `true` / `false` | `false` | Show model reasoning. On Claude models that return empty thinking by default (Opus 4.7+, Claude 5, Fable), requests `display: "summarized"` |
| `thinkingBudgetTokens` | `0` or ≥ `1024` | unset | Extended thinking. Sent as `{"type":"adaptive"}` on Claude 4.6+ / Claude 5 and as `budget_tokens` on older models; `0` disables (ignored on Fable, Opus 5.5 and Sonnet 5.5, where the API does not allow thinking to be turned off, and on Opus 5 at `max` effort, where it only allows it at `high` or below). CLI: `--thinking enabled\|disabled`, `--max-thinking-tokens N` |
| `effort` | `low` / `medium` / `high` / `max` | unset | Sent as `output_config.effort` on Claude 4.6+ / Claude 5; older and non-Claude models get a prompt nudge. Set with `/effort` |
| `spinnerStyle` | `themed` / `minimal` / `silent` | `themed` | Spinner animation style |
| `env` | `{ "NAME": "value" }` | `{}` | Environment variables set on every Bash and PowerShell tool command. A project's `.claude/settings.json` may set them only in a folder you have `/trust`ed |

### CLAUDE.md / AGENTS.md

Drop a `CLAUDE.md` or `AGENTS.md` in your project root to give the agent project-specific context. These files are automatically injected into the system prompt.

### .env Files

Auto-loaded from (in order):
1. `$CWD/.env`
2. `~/.env`
3. `~/.config/oxideclaw/.env`

Only oxideclaw's own keys (provider API keys, `ANTHROPIC_MODEL`, `OLLAMA_HOST`, ...) are read. `OLLAMA_HOST` and `ANTHROPIC_MODEL` decide where your prompts are sent, so `$CWD/.env` may set them only in a folder you have `/trust`ed; otherwise they are ignored with a note.

### XDG Base Directories

| Purpose | Variable | Default |
|---------|----------|---------|
| Config | `$XDG_CONFIG_HOME/oxideclaw/` | `~/.config/oxideclaw/` |
| Data | `$XDG_DATA_HOME/oxideclaw/` | `~/.local/share/oxideclaw/` |
| Cache | `$XDG_CACHE_HOME/oxideclaw/` | `~/.cache/oxideclaw/` |

---

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` / `Alt+Enter` / `Ctrl+J` | Newline |
| `Esc` | Cancel request / stop TTS / close overlay |
| `Ctrl+S` | Stop TTS |
| `Ctrl+R` | Voice record toggle |
| `?` | Shortcuts overlay |
| `PgUp` / `PgDn` | Scroll chat |
| `Tab` | Autocomplete commands / models / history |
| `Shift+Click` | Select text |
| `Ctrl+Shift+C` | Copy selection |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Claude API key |
| `OLLAMA_HOST` | Ollama server URL (default: `http://localhost:11434`) |
| `GROQ_API_KEY`, `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `TOGETHER_API_KEY`, `MISTRAL_API_KEY`, `VENICE_API_KEY` | API key for the matching OpenAI-compatible provider |
| `OPENAI_API_KEY` | API key for `oai:` and `openai-compat:` models |
| `OPENAI_BASE_URL` | Endpoint for `openai-compat:` models (required for that prefix) |
| `LM_STUDIO_HOST` | LM Studio server URL (default: `http://localhost:1234/v1`) |
| `XDG_CONFIG_HOME` | Config directory base |
| `XDG_DATA_HOME` | Data directory base |
| `XDG_CACHE_HOME` | Cache directory base |

---

## Architecture

```
src/
├── main.rs           # Entry point, CLI args, .env auto-load
├── api/              # Anthropic + Ollama + OpenAI-compat backends (streaming SSE)
├── sdk/              # Headless NDJSON server (--headless mode)
├── tui/              # ratatui UI (inline viewport, no alt screen)
├── tools/            # 30+ tools (Bash, Read, Write, Edit, Glob, Grep, ...)
├── commands/         # 60+ slash commands
├── rag/              # tree-sitter AST + SQLite FTS5 indexing
├── mcp/              # MCP plugin client
├── session/          # Save/resume/search/export sessions
├── voice.rs          # Recording + Whisper STT + Piper/XTTS TTS
├── router.rs         # Smart model routing by complexity
├── cost.rs           # Token/cost tracking + budget enforcement
├── sandbox.rs        # bwrap / firejail / strict
└── config.rs         # Settings, CLAUDE.md/AGENTS.md injection
```

Built with `tokio`, `ratatui`, `reqwest` (rustls), `clap`, `serde_json`, `tree-sitter`.
