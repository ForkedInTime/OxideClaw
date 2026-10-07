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
- [Auto-fix Loop](#auto-fix-loop)
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
| Generic | `openai-compat:` | `OPENAI_API_KEY` (optional: unset sends no `Authorization` header), plus `OPENAI_BASE_URL` (required) |

Keys come from the environment, never from `settings.json`. Export the variable or put it in `~/.env` or `~/.config/oxideclaw/.env`, then pick the model. `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are not read from `.env` files; export them in your shell.

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
| `/effort [low\|medium\|high\|xhigh\|max\|off]` | Set the API effort level (`output_config.effort`); prompt nudge on models without it |

### Session Management

| Command | Description |
|---------|-------------|
| `/session` | Browse and resume sessions |
| `/session list` | List saved sessions |
| `/export` | Export current session to markdown (sessions save automatically) |
| `/session delete <id-prefix>` | Print the `rm` command that deletes a session's files |

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
| `/index` (or `/rag index`) | Index current directory |
| `/rag search <query>` | Search the index |
| `/rag status` | Show index stats |
| `/rag clear` | Clear the index |

### Cost & Budget

| Command | Description |
|---------|-------------|
| `/cost` | Show session cost breakdown |
| `/budget <amount>` | Set budget limit (e.g., `/budget $5`) |
| `/budget clear` | Remove budget limit |
| `/router [on\|off\|status]` | Enable, disable or inspect the [smart model router](#smart-model-router) (off by default) |
| `/router <low\|medium\|high\|super-high> <model>` | Set the model for a router tier |

### Settings

| Command | Description |
|---------|-------------|
| `/reload` | Hot-reload settings, CLAUDE.md, AGENTS.md |
| `/config` | Show current configuration |

### Tools & MCP

| Command | Description |
|---------|-------------|
| `/mcp` | List MCP plugins |
| `/mcp add <name> <command\|url> [args...]` | Add MCP plugin (stdio command or HTTP URL) |
| `/mcp tools` | List tools per MCP server |

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
| `WebSearch` | Search the web, with sources (Anthropic server-side search: needs an Anthropic key on any provider) |

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

Rust, Python, JavaScript, TypeScript, Go, Java, C, Bash

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

The index auto-updates when files change between queries. It lives in `.claude/rag.db` (with the project's `/memory` entries); the interactive TUI builds it on start, while `-p`, SDK and ACP sessions only use an index that already exists and never create one. OxideClaw adds `**/.claude/rag.db*` to the repo's private `.git/info/exclude` so `/checkpoint`, `/commit` and `/spawn merge` never commit it.

---

## Smart Model Router

Routes each interactive (TUI) turn to the most cost-effective model based on complexity analysis. The router is **off by default**: until you enable it, every turn uses your configured model. Enable it with `/router on` for the session, or `"routerEnabled": true` in settings.json.

| Complexity | Routed To (default) | Example |
|-----------|-----------|---------|
| Low | `claude-haiku-4-5` | "What does this function do?" |
| Medium | `claude-sonnet-5` | "Refactor this module" |
| High | your configured model | "Debug this race condition" |
| Super-high | `claude-opus-5` | "Audit the entire codebase" |

```
/router              # show status and tier models (also /router status)
/router on | off     # enable / disable routing for this session
/router low ollama:llama3      # set the model for a tier
/router medium <model>
/router high <model>
/router super-high <model>
```

The router analyzes prompt length, keyword signals (debug, refactor, audit), and context to classify complexity. Cost savings are tracked and shown in `/cost`. The `router*` keys in the [settings table](#settings-file) set the same options at startup.

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

All Claude model pricing is built in. Ollama and LM Studio models are always free. Other OpenAI-compatible providers use built-in rough estimates, flagged as estimated in `/cost`.

---

## Session Management

Sessions save automatically; resume, search, and export them.

```
/session             # interactive session browser with previews
/session list        # list saved sessions
/export              # export the current session to markdown
```

Sessions are stored in the data directory's `sessions/` folder: `~/.claude/sessions/` by default, or `$XDG_DATA_HOME/oxideclaw/sessions/` when `XDG_DATA_HOME` is set (see [XDG Base Directories](#xdg-base-directories)). `/status` prints the resolved path.

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

## Auto-fix Loop

In trusted projects, every edit triggers a lint and test cycle. Untrusted projects skip it until you run /trust.

After a turn's `Write`, `Edit` or `MultiEdit` calls in the interactive TUI (`-p`, the SDK and ACP do not run auto-fix), OxideClaw runs the project's linter and then its tests: an auto-detected runner (clippy / `cargo test`, ESLint / `npm test`, ruff / pytest, `go vet` / `go test`) when it is installed, or `autoFixLoop.lintCommand` / `autoFixLoop.testCommand` from your settings. Failures go back to the model for up to `autoFixLoop.maxRetries` (default 3) retries. `autoFixLoop.trigger` is `autonomous` (default: only in `auto-edit` and `full-auto` autonomy), `always` or `off`.

Lint and test commands run the project's own code (`build.rs`, `conftest.py`, npm scripts, Makefiles), so nothing runs in a folder that is not in your `trustedProjects` list; the first skipped edit of a session says so once. `/trust` takes effect from the next edit, including the project's own `autoFixLoop` settings. In a trusted folder the commands run under the same sandbox as the Bash tool when one is enabled (`/sandbox enable`), and a command the sandbox refuses is skipped, never run unsandboxed. Under bwrap or firejail, a check that fails because of the sandbox itself (no network for a download, a tool it does not expose) is skipped with a note instead of being sent to the model as a broken edit.

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

**JSON on stdout (optional):** `{ "decision": "block", "reason": "...", "continue": false, "stopReason": "...", "systemMessage": "...", "additionalContext": "..." }`. A hook can only block: `"decision": "approve"` is ignored, and the call still goes through the normal permission check.

Each hook has a 60-second timeout and runs in its own process group, so a timed-out hook cannot leave children behind. `"disableAllHooks": true` in settings or `--bare` on the command line skips every hook.

---

## Sandboxing

OxideClaw supports multiple sandbox backends for tool isolation:

| Backend | Description |
|---------|-------------|
| `bwrap` | bubblewrap — lightweight Linux sandboxing. System dirs and per-user toolchains (`~/.cargo/bin`, `~/.rustup`, `~/.local/bin`, `~/.nvm`) are read-only, the rest of `$HOME` is hidden, `/tmp` is private |
| `firejail` | Firejail — security sandbox with profiles |
| `strict` | Best-effort denylist of catastrophic command literals — NOT isolation; no filesystem or network restriction (the only option on macOS/Windows) |

The project directory stays writable inside the sandbox, `.git/` included, so OxideClaw's own git snapshots (auto-commit, `/undo`, `/redo`) never run repository hooks or `core.fsmonitor`, and they stop with a warning if a repo-local `filter.*` driver appears or changes mid-session instead of running it outside the sandbox. Restart after reviewing `.git/config` to resume them.

---

## Configuration

### Settings File

`settings.json` in the config directory: `~/.claude/settings.json` by default, `$XDG_CONFIG_HOME/oxideclaw/settings.json` when `XDG_CONFIG_HOME` is set (see [XDG Base Directories](#xdg-base-directories)), or `$CLAUDE_CONFIG_DIR/settings.json`. `/status` prints the config directory in use:

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
| `maxTokens` | number | `32000` on Claude 4.5+, `8192` elsewhere | Output cap per turn, shared with thinking. `maxTokensByModel` sets it per model |
| `showThinkingSummaries` | `true` / `false` | `false` | Show model reasoning. On Claude models that return empty thinking by default (Opus 4.7+, Claude 5, Fable), requests `display: "summarized"` |
| `thinkingBudgetTokens` | `0` or ≥ `1024` | unset | Extended thinking. Sent as `{"type":"adaptive"}` on Claude 4.6+ / Claude 5 and as `budget_tokens` on older models; `0` disables (sent as `{"type":"between_tools"}` on Sonnet 5.5; ignored on Fable and Opus 5.5, where the API does not allow thinking to be turned off, and on Opus 5 and Sonnet 5.5 at `xhigh`/`max` effort, where it only allows it at `high` or below). CLI: `--thinking enabled\|disabled`, `--max-thinking-tokens N` |
| `effort` | `low` / `medium` / `high` / `xhigh` / `max` | unset | Sent as `output_config.effort` on Claude 4.6+ / Claude 5 (`xhigh` becomes `high` on Opus/Sonnet 4.6, which lack it); older and non-Claude models get a prompt nudge. Set with `/effort` |
| `spinnerStyle` | `themed` / `minimal` / `silent` | `themed` | Spinner animation style |
| `routerEnabled` | `true` / `false` | `false` | Start with the [smart model router](#smart-model-router) on (same as `/router on`) |
| `routerBudget` | USD amount | unset | Session spend limit applied at startup (same as `/budget`) |
| `routerLowModel` | any model name | `claude-haiku-4-5` | Model for low-complexity turns |
| `routerMediumModel` | any model name | `claude-sonnet-5` | Model for medium-complexity turns |
| `routerHighModel` | any model name | your `model` | Model for high-complexity turns |
| `routerSuperHighModel` | any model name | `claude-opus-5` | Model for super-high-complexity turns |
| `updateCheck` | `true` / `false` | `true` | Once every 24 h the TUI looks up the latest GitHub release (the one `oxideclaw update` installs) in the background, with a 3 s timeout, and shows one dim line when it is newer than yours. `false` in any settings file turns it off; `-p`, `--headless`, `acp` and `browse` never check. The last answer is cached in `$XDG_CACHE_HOME/oxideclaw/update-check.json` (default `~/.cache/oxideclaw/`). Uses `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` |
| `env` | `{ "NAME": "value" }` | `{}` | Environment variables set on every Bash and PowerShell tool command. A project's `.claude/settings.json` may set them only in a folder you have `/trust`ed |

### CLAUDE.md / AGENTS.md

Drop a `CLAUDE.md` or `AGENTS.md` in your project root to give the agent project-specific context. These files are automatically injected into the system prompt.

### .env Files

Auto-loaded from (in order):
1. `$CWD/.env`
2. `~/.env`
3. `~/.config/oxideclaw/.env`

Only oxideclaw's own keys (provider API keys, `ANTHROPIC_MODEL`, `OLLAMA_HOST`, ...) are read; `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are not, so export those in your shell. `OLLAMA_HOST` and `ANTHROPIC_MODEL` decide where your prompts are sent, so `$CWD/.env` may set them only in a folder you have `/trust`ed; otherwise they are ignored with a note.

### XDG Base Directories

XDG config and data paths are opt-in: they are used only when the matching variable is set. Otherwise config and data live in `~/.claude/`, shared with Claude Code. The cache follows `$XDG_CACHE_HOME` and defaults to `~/.cache/oxideclaw/`.

| Purpose | When the variable is set | Default (variable unset) |
|---------|--------------------------|--------------------------|
| Config (`settings.json`, MCP, output styles) | `$XDG_CONFIG_HOME/oxideclaw/`, if it exists or `~/.claude/` does not | `~/.claude/` |
| Data (sessions, XTTS server files) | `$XDG_DATA_HOME/oxideclaw/`, if it exists or `<config dir>/sessions/` does not | the config directory |
| Cache (update check) | `$XDG_CACHE_HOME/oxideclaw/` | `~/.cache/oxideclaw/` |

`$CLAUDE_CONFIG_DIR` overrides the config directory outright. To move an existing `~/.claude/` setup to XDG paths, create `$XDG_CONFIG_HOME/oxideclaw/` (and `$XDG_DATA_HOME/oxideclaw/`) and copy your files in; `/status` shows which directories are in use.

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
| `OPENAI_API_KEY` | API key for `oai:` models, and for `openai-compat:` endpoints that need one |
| `OPENAI_BASE_URL` | Endpoint for `openai-compat:` models (required for that prefix; shell only, not `.env`) |
| `LM_STUDIO_HOST` | LM Studio server URL (default: `http://localhost:1234/v1`; shell only, not `.env`) |
| `OXIDECLAW_NO_UPDATE_CHECK` | `1` turns off the TUI's daily update check (same as `"updateCheck": false`). Shell only. |
| `OXIDECLAW_BROWSER_NO_SANDBOX` | `1` lets `/browse` run Chrome without its sandbox when OxideClaw runs as root (Docker, CI); pages then run unsandboxed as root. Shell only. |
| `CLAUDE_CONFIG_DIR` | Config directory, overriding `~/.claude` and `XDG_CONFIG_HOME` |
| `XDG_CONFIG_HOME` | Opt-in config directory base (`$XDG_CONFIG_HOME/oxideclaw`) |
| `XDG_DATA_HOME` | Opt-in data directory base (`$XDG_DATA_HOME/oxideclaw`) |
| `XDG_CACHE_HOME` | Cache directory base (`$XDG_CACHE_HOME/oxideclaw`, default `~/.cache/oxideclaw`); holds the update-check cache |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | CA certificates to trust in place of the OS certificate store (e.g. a TLS-inspecting corporate proxy's root). HTTPS trusts the bundled Mozilla roots plus the OS store by default |

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
