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
- [Autonomy Modes](#autonomy-modes)
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
| Gemini | `gemini:` | `GEMINI_API_KEY`, else `GOOGLE_API_KEY` (Google's OpenAI-compatible endpoint, `generativelanguage.googleapis.com/v1beta/openai`) |
| LM Studio | `lmstudio:` | none (`LM_STUDIO_HOST` overrides `http://localhost:1234/v1`) |
| Together | `together:` | `TOGETHER_API_KEY` |
| Mistral | `mistral:` | `MISTRAL_API_KEY` |
| Venice.ai | `venice:` | `VENICE_API_KEY` |
| OpenAI | `oai:` | `OPENAI_API_KEY` (Responses API; see below) |
| Generic | `openai-compat:` | `OPENAI_API_KEY` (optional: unset sends no `Authorization` header), plus `OPENAI_BASE_URL` (required) |

Keys come from the environment, never from `settings.json`. Export the variable or put it in `~/.env` or `.env` in the config dir (`~/.config/oxideclaw/.env`), then pick the model. `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are not read from `.env` files; export them in your shell.

```bash
echo 'GROQ_API_KEY=gsk_...' >> ~/.config/oxideclaw/.env
oxideclaw --model groq:llama-3.3-70b-versatile
```

Or switch at runtime:

```
/model groq:llama-3.3-70b-versatile
/model gemini:gemini-2.5-flash
/model oai:gpt-4o
```

Each provider reads only its own variable, so your OpenAI key is never sent to Groq or OpenRouter.

`oai:` models use OpenAI's Responses API (`/v1/responses`); every other provider uses Chat Completions. On reasoning models (o-series, GPT-5 and later, Codex) the reasoning behind a tool call comes back encrypted (`store: false`, nothing is kept on OpenAI's side) and is sent back with the next request of the tool loop, so the model does not lose its train of thought between calls; after `/model` or a resumed session those turns go out without it. `effort` is sent as `reasoning.effort`, and `showThinkingSummaries` shows the reasoning summaries (OpenAI generates them only for verified organizations; when it refuses, OxideClaw says so once and carries on without them). Set `"openaiApi": "chat"` in settings.json (or `OXIDECLAW_OPENAI_API=chat`) to go back to Chat Completions, or `"responses"` to use the Responses API for an `openai-compat:` endpoint or LM Studio (`lmstudio:`) that serves it; the named cloud presets keep Chat Completions.

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
| `/undo [N]` | Take back the last N turns (default 1): files and conversation |
| `/redo [N]` | Put back the last N undone turns (default 1) |
| `/rewind [N]` | Pick a turn to go back to; `/rewind N` is `/undo N` |
| `/autocommit` | Show the per-turn snapshot state |

Claude Code sessions import with `oxideclaw config import-claude --sessions`; see [Importing Claude Code sessions](#importing-claude-code-sessions).

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
| `/router [on\|off\|status]` | Enable, disable or inspect the [smart model router](#smart-model-router) (on when two tiers are configured) |
| `/router <low\|medium\|high\|super-high> <model>` | Set the model for a router tier |

### Settings

| Command | Description |
|---------|-------------|
| `/reload` | Hot-reload settings (permission rules, hooks, sandbox and `autonomy` included; an `/autonomy` choice stays unless the file's `autonomy` changed), CLAUDE.md, AGENTS.md, GEMINI.md |
| `/config` | Show current configuration |
| `/autonomy [mode]` | Show or switch the [autonomy mode](#autonomy-modes) for this session |

### Tools & MCP

| Command | Description |
|---------|-------------|
| `/mcp` | List connected MCP servers, their scope and negotiated protocol revision, and project servers waiting for `/trust` |
| `/mcp add [--scope local\|project\|user] <name> <command\|url> [args...]` | Add an MCP server (stdio command or HTTP URL), local scope by default |
| `/mcp remove [--scope <s>] <name>` | Remove a server; `--scope` is needed when the name is in more than one scope |
| `/mcp get <name>` | Show a server's config in every scope that defines it |
| `/mcp enable\|disable <name>` | Toggle the entry that starts; a project entry is refused, since `.mcp.json` is shared (override it with a local one) |
| `/mcp tools` | List tools per MCP server |

**MCP scopes.** `oxideclaw mcp add` and `/mcp add` keep a server in one of three scopes; `mcp list`, `mcp get` and `mcp remove` show and take the scope.

| Scope | Where | Who sees it | Starts |
|-------|-------|-------------|--------|
| `local` (default) | `local-mcp/<project path>.json` in the config dir, keyed by the project's canonical path, mode 0600 | You, in this project only | Always: it is your own config |
| `project` | `.mcp.json` in the repo (`mcpServers` in `.claude/settings.json` count too) | Everyone with the repo; usually committed | Only after `/trust` |
| `user` | `mcpServers` in the config dir's `settings.json` | You, in every project | Always |

A name in several scopes starts from the highest one that loads: local, then project, then user. `--scope project` refuses literal `-e` values (and `add-json` headers), since the file is shared and committed: keep the secret in the local scope, or write a reference such as `-e GITHUB_TOKEN='${GITHUB_TOKEN}'` (or `"Authorization": "Bearer ${TOKEN}"`) that each user's environment fills in at startup. `--force` writes a literal value anyway.

**MCP protocol.** OxideClaw speaks the stateless `2026-07-28` revision: no `initialize` handshake and no session, every request carries the protocol version, client name and (empty) client capabilities in `_meta`, and over Streamable HTTP each POST also sends `MCP-Protocol-Version`, `Mcp-Method`, `Mcp-Name` and the `Mcp-Param-*` headers a tool's `x-mcp-header` arguments ask for (a tool with an invalid `x-mcp-header` is left out, as the revision requires). Each server is probed once per connection with `server/discover`: a discovery result or one of the revision's own errors means a `2026-07-28` server; any other error, an HTTP 4xx without such an error, or no answer within 5 seconds means an older server, which gets the `initialize` handshake offering `2025-06-18` and keeps the version it answers (`2025-03-26` and `2024-11-05` servers work as before). A stdio server that exits on the probe is started again without it. Servers that ask for input mid-request (`input_required`) get their elicitations declined; a sampling or roots request ends that call with an error, since OxideClaw serves neither. `/mcp` and `oxideclaw mcp list` show each server's negotiated revision; `mcp list` starts the servers a session in the current directory would start (trusted, enabled) to find out, and reports the ones that fail to connect.

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
| `LSP` | Language Server Protocol integration; starts servers only in a `/trust`ed project, under the Bash sandbox when one is enabled |
| `NotebookEdit` | Edit Jupyter notebooks |
| `MCP` | Model Context Protocol plugins |

---

## Voice & TTS

### Voice Input (STT)

Uses Whisper for speech-to-text. Press `Ctrl+R` to start/stop recording.

### Text-to-Speech

Powered by XTTS v2. Supports voice cloning — speak in your own voice. GPU-accelerated when CUDA is available, falls back to CPU. Spoken replies are trimmed to 200 words.

Voice is an optional add-on that needs Python + Coqui. XTTS v2 weights are licensed under CPML (non-commercial use only).

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

Local codebase search: SQLite FTS5 (BM25) full-text search over tree-sitter symbol chunks.

### Supported Languages

Rust, Python, JavaScript, TypeScript, Go, Java, C, Bash

### How It Works

1. The project is walked with git's ignore rules: `.gitignore` (at every level), `.git/info/exclude`, the global excludes file and `.ignore`. Ignored files (a `config.local.js` with keys, `.env.*` you keep out of git) are never indexed, so their contents never reach a model. Hidden directories and build/vendor directories (`target`, `node_modules`, `dist`, ...) are skipped too.
2. tree-sitter parses source files into AST nodes (functions, structs, classes, methods)
3. Each symbol becomes a chunk in a SQLite database with an FTS5 full-text index
4. Queries match symbol names and code content, ranked by BM25, and the best chunks are injected into the AI context

### Usage

```
/rag index           # index the current directory
/rag search "auth"   # search for symbols/code matching "auth"
/rag status          # show index statistics
/rag clear           # clear the index
```

The index auto-updates when files change between queries. The interactive TUI builds it on start, while `-p`, SDK and ACP sessions only use an index that already exists and never create one. When `.claude/memory.db` is opened, OxideClaw adds `**/.claude/memory.db*` to the repo's private `.git/info/exclude` so `/checkpoint`, `/commit` and `/spawn merge` never commit your memories; `/undo` snapshots skip it too.

**Where it runs.** The index builds on its own (at TUI startup and before each prompt, and is refreshed for `-p`, SDK and ACP turns once one exists) only when the working directory is inside a git repository. Elsewhere the TUI shows `Code index off: not inside a git repository` once and `-p`/SDK stay silent; `/index` still works there on request. Your home directory and `/` are never indexed, not even by `/index`.

**What it covers.** The whole git work tree, wherever in it you start: launching from `repo/src` uses and refreshes the same index as launching from `repo`, and the walk starts at the work-tree root so every `.gitignore` rule applies (starting inside an ignored directory indexes nothing from it). Outside git, `/index` covers the directory it runs in.

**Where it lives.** `$XDG_CACHE_HOME/oxideclaw/rag/<hash>.db` (default `~/.cache/oxideclaw/rag/`), where `<hash>` is the first 16 hex digits of the SHA-256 of the canonical work-tree root (the directory itself outside git). With neither `$XDG_CACHE_HOME` nor a home directory set, the index is off. The code index writes nothing into the project; `/memory` notes live in `.claude/memory.db`, which is only created once memories are used (a `/memory` command or auto-capture), not by every session. The cache can be deleted at any time. `/rag search`, `/rag status` and `/rag clear` only read an existing index, and `/rag status` prints its path.

Older versions kept the index in `<project>/.claude/rag.db`. On first use that file is deleted without being searched again, once its tables confirm it is an OxideClaw index; any memories in it move to `.claude/memory.db` first. A `.claude/rag.db` belonging to anything else is left alone.

---

## Smart Model Router

Optional. Each prompt goes to one of four tiers, and a tier can be any model `/model` accepts: a Claude model, `ollama:<name>` or an OpenAI-compatible preset (`groq:`, `oai:`, `openrouter:`, ...). A local model can take the simple turns and Claude the hard ones.

```json
"router": {
  "low": "ollama:qwen3-coder",
  "mid": "claude-sonnet-5",
  "high": "claude-opus-5"
}
```

The router starts on once two or more tiers are set in the `router` block. `"enabled": false` keeps it off; `"enabled": true` turns it on with fewer, and unset tiers keep their defaults. With one tier or none and no `enabled`, it stays off. `/router on` and `/router off` switch it for the session. `/router` (or `/router status`) shows the tiers, the classifier, the model the last turn ran on, any skipped tiers and the estimated savings. Routing applies in the TUI, in `-p` (unless `--model` names the model for the run) and in SDK sessions whose `session/start` names no `model`. ACP sessions are not routed.

| Tier | Default on a Claude session | Default on an Ollama or OpenAI-compatible session | Example |
|-----------|-----------|-----------|---------|
| Low | `claude-haiku-4-5` | your current model | "What does this function do?" |
| Medium | `claude-sonnet-5` | your current model | "Add a test for parse_config" |
| High | your current model | your current model | "Debug this race condition" |
| Super-high | `claude-opus-5` | your current model | Whole-codebase analysis |

On a local or OpenAI-compatible session, only the tiers you name go to another provider, so switching the router on never sends prompts somewhere you did not choose. Unset tiers follow `/model`; tiers set in settings.json or with `/router <tier>` stay.

Tiers, `enabled: true` and the classifier in a project's `.claude/settings.json` apply only once the project is `/trust`ed. An untrusted project can still switch the router off, and its `routerBudget` may lower your budget but not raise it. `/trust`, `/trust revoke` and `/reload` (which also picks up edits to the router settings) apply from the next prompt; only the settings that moved change, so a `/router off` or `/router <tier>` from this session stays unless that same setting changed, and a router that settings switch on says so with its tiers. The same goes for where an untrusted project can send prompts otherwise: its `model` applies only on the backend of your own `model` setting (Anthropic when you set none), its `phaseRouter` and the `<!-- phase-routing: ... -->` directive in its CLAUDE.md files may pick only Anthropic models, and either switches phase routing on only when every phase stays on Anthropic. Your own CLAUDE.md in the config dir applies as written. What was ignored is listed at startup and by `/trust status`; `/trust` applies the phase routing at once and a project `model` on `/reload`.

Compaction measures the history against the largest window among the tiers a turn can still go to, and the summary (automatic, `/compact`, or for a prompt too long for its model) goes to that tier when its window beats the session model's; tiers skipped for the session or without a credential do not count, and with none left the session model's window applies. Between tool rounds inside a turn, once the tier is picked, it measures against that tier's window: the tier is picked once per prompt, and Ollama truncates an overflow silently rather than failing over. A turn's final response is measured against the largest tier again, since the next prompt is routed afresh. In the TUI a response at 90% of the tier's window hands the rest of the turn to the next tier up with a larger window, when there is one; otherwise old tool results are stripped from 85%.

**Picking the tier.** By default a keyword and length heuristic scores the prompt: signal words such as debug, refactor or audit, prompt length, code blocks and file paths. With `"classifier": "model"` the low tier is asked for a one-word label (`low`, `mid`, `high` or `super-high`) under a strict prompt, with 16 output tokens and 3 seconds. A non-Claude low tier gets 1024 output tokens, since a model that reasons first (OpenAI's GPT-5 and o-series, Gemini 2.5, qwen3) spends its reasoning against that cap, and an OpenAI reasoning model is asked for its lowest effort (`minimal`, `none` or `low`). A Claude low tier that thinks by default has thinking switched off (`disabled`, or `between_tools` on Sonnet 5.5); one that cannot turn it off (Opus 5.5, Fable, Mythos) is asked for `low` effort and gets 1024 output tokens. The first word of the answer is the label. On a timeout, an error or any other answer the heuristic decides. The classifier's call is billed like any other turn and counts in `/cost` and `/budget`. When the history is too large for the chosen tier's window, the turn goes to the next tier up that holds it.

**Escalation.** Sometimes a turn on a lower tier fails in a way that suggests the model is out of its depth:

- an API error that is not about authentication or rate limits
- malformed tool calls (an unknown tool, or a required parameter missing) in two responses in a row
- the loop detector firing (TUI)
- a context larger than the model's window

The turn then continues once on the next tier up, from where it stopped, and one dim line says so. It does not escalate if resending the history at the next tier's input price could pass what is left of `/budget`. It also does not escalate once text from the failed response has been shown.

**Unavailable tiers.** Some tiers can't be used: their backend has no credential (no Anthropic key, no `GROQ_API_KEY`, no `OPENAI_BASE_URL`, ...), or their self-hosted server (Ollama, LM Studio, `openai-compat:`) does not accept a connection within 1.5 seconds. A server that requests reach through `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` (and not `NO_PROXY`) is not probed; its errors show up on the turn instead. The router skips such a tier for the rest of the session and says so once. Its turns go to the next usable tier up (or down when there is none). `/router on` checks again.

**Seeing where a turn went.** In the TUI, the status bar shows `ROUTER → <model>` and the chat gets one dim line whenever the routed model changes. In `-p`, `--verbose` prints a `[router] ...` line on stderr. SDK hosts get a [`model/routed`](sdk/protocol.md#modelrouted) notification, and `turn/completed` carries the model that finished the turn.

```
/router              # show status, tiers and skipped tiers (also /router status)
/router on | off     # enable / disable routing for this session
/router low ollama:llama3      # set the model for a tier
/router mid <model>            # also: medium
/router high <model>
/router super-high <model>
```

Earlier versions used flat keys (`routerEnabled`, `routerLowModel`, `routerMediumModel`, `routerHighModel`, `routerSuperHighModel`). They still work, and the `router` block wins where both set a tier. Tiers set only with the flat keys do not switch the router on: that takes `routerEnabled: true`, `"enabled": true` or `/router on`, as before.

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
/undo 2              # take back the last two turns, files and conversation
```

### Undo timeline

`/undo`, `/redo` and `/rewind` move one timeline of turns, a turn being a prompt and everything the agent did for it. `/undo N` reverts the files of the last N turns and removes those turns from the conversation and the saved session, so the model no longer sees them; `/redo N` re-applies both, oldest first, and any new prompt or `/compact` clears what could be redone. `/rewind` lists the turns and undoes everything after the one you pick. Files come from per-turn snapshots, one commit per turn on a private ref (`refs/oxideclaw/sessions/<id>`), which hold the repository's non-ignored files only: edits to gitignored files and to files outside the repository are never reverted; an undo rewrites only the paths the undone turns changed, and one that would overwrite such a file changed since the last snapshot is refused with the file names and changes nothing. Every turn is snapshotted however it ends: done, Esc, a failed request, the `/budget` stop or quit. Changes made between turns (hand edits, new files, a pull, work done before a resume) are snapshotted before the next turn starts, so undoing that turn keeps them; `/undo` past them takes them back too, and `/redo` returns them. Outside a git repository, with `autoCommit.enabled: false`, or in a session whose snapshots are not in this repository (resumed elsewhere, or pruned), only the conversation moves and a one-line notice says so; the next turn there starts a new chain of snapshots in this repository, and the earlier turns stay undoable for the conversation only. A TUI session continued with `-p --resume`/`--continue` or over ACP `session/load` gets turns its timeline never saw, so that ends its timeline and `/redo` turns the same way. Turns from before the timeline (a session saved by an older version, or the summary `/compact` leaves) can be undone too, conversation only. Undo and redo state is saved with the session, so it survives `/resume`; the turns `/redo` can bring back go in `<id>.redo`, never the `.meta`, and stay in memory only with `--no-session-persistence`.

### Importing Claude Code sessions

```bash
oxideclaw config import-claude --sessions --list   # this directory's Claude Code sessions
oxideclaw config import-claude --sessions          # import every one not imported yet
oxideclaw config import-claude --sessions 0f3e9a1c # import one, by id or id prefix
```

Claude Code keeps a JSONL transcript per session in `~/.claude/projects/<directory>/<id>.jsonl` (under `$CLAUDE_CONFIG_DIR` instead when that is set to an absolute path). Run from a project directory, `--list` shows that directory's sessions (id, start time, message count, first prompt) and which are imported; it changes nothing. `--sessions` turns each into an ordinary OxideClaw session that `/session` lists and `--resume <id>` reopens: titled with Claude Code's title (or the first prompt), with its working directory, model, start time and last activity, and the user and assistant text, tool calls and tool results. The history is the one Claude Code's model last saw: the transcript is a tree, so it is rebuilt by walking back from the last message, which leaves out branches abandoned by a rewind, and a compacted session starts at its compaction summary instead of replaying everything before it. Subagent (sidechain) records, injected meta records, queue bookkeeping and API error stand-ins are left out, and reasoning (thinking) blocks are not replayed. Tool results are paired with their calls by id, since Claude Code does not always write them in order, and a call or result without its other half is dropped, so the model gets a history the API accepts. Blocks OxideClaw has no type for (server tool calls, images inside tool results, ...) and malformed lines are skipped and counted in the summary. Each imported session records the Claude Code session id, so a re-run imports only what is new, and when it was imported, which counts as activity for `cleanupPeriodDays` (the list still shows the session's own age). Transcripts are read one at a time, and those over 50 MiB are skipped with a warning. Imported files are owner-only (0600, in a 0700 directory when the import creates it), as Claude Code keeps them. `~/.claude` is only read; an import into a sessions directory inside it is refused, as is one inside `$CLAUDE_CONFIG_DIR` unless OxideClaw's own (deprecated) `$CLAUDE_CONFIG_DIR` profile is that directory.

Sessions are stored in `~/.local/share/oxideclaw/sessions/` by default (`$XDG_DATA_HOME/oxideclaw/sessions/` when `$XDG_DATA_HOME` is set). See [Where files live](#where-files-live).

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
| `initialize` | Protocol version 1. Advertises `embeddedContext`, `loadSession` and `mcpCapabilities` `http` and `sse`; no image/audio prompts. With `oxideclaw --no-session-persistence acp`, `loadSession` is `false`. |
| `authenticate` | No-op. Credentials come from the normal chain (`ANTHROPIC_API_KEY`, `ant` profile, settings). |
| `session/new` | Requires an existing `cwd`; the session reads that directory's CLAUDE.md, AGENTS.md, GEMINI.md and project settings. `mcpServers` entries of the stdio, `http` (Streamable HTTP) and `sse` (MCP's older HTTP+SSE transport; the announced endpoint must be on the server's own origin) kinds are started for the session, with the client's `env` or `headers`; their tools ask for permission like any other. The client's values are used as sent, with no `${VAR}` expansion. The conversation is saved after every turn, in the same sessions directory as the TUI's (so `/resume` lists it); `oxideclaw --no-session-persistence acp` saves nothing. At most 8 sessions stay live: past that, the least recently used idle session that is already saved is dropped with its MCP servers, and its next `session/prompt` reloads it from disk with its history. |
| `session/load` | Loads a saved session by id (one made over ACP, or a TUI or `-p` session), replays it as `session/update`s in order (`user_message_chunk`, `agent_message_chunk`, `agent_thought_chunk` when `showThinkingSummaries` is on, `tool_call` then its `tool_call_update` with the result), answers `{}`, and then takes prompts with the full history. A history already past the model's summarise threshold (90% of its window) is summarised before the first prompt, and a prompt the model rejects as too long summarises the earlier history and is retried once. Takes `cwd` and `mcpServers` like `session/new`. An unknown id is `-32002` (resource not found). |
| `session/prompt` | Text, `resource_link`, and embedded text resources are flattened into one prompt. Answers with `stopReason`: `end_turn`, `max_tokens`, `max_turn_requests` (the turn's request cap or the `/budget` cap was reached), `refusal` (the model declined the request), or `cancelled`. |
| `session/update` | `agent_message_chunk`, `agent_thought_chunk`, `tool_call` (kind + title + raw input), `tool_call_update` (status + output summary). |
| `session/request_permission` | Sent for every tool the SDK policy marks *ask* (the default for tools not on an allow list). Options: allow once / reject once. A `cancelled` outcome denies the tool. The request waits for an answer with no timeout; `session/cancel` withdraws it. |
| `session/cancel` | Stops the in-flight model stream and skips queued tools; the prompt is answered with `cancelled`. |
| `session/close` | Drops a live session and stops its MCP servers; answers `{}`. The saved conversation stays for `session/load`. `-32000` while a prompt runs, `-32002` for an unknown id. |
| `session/set_mode` | Not supported (`-32601`). |

Errors use JSON-RPC codes: `-32602` invalid params (bad `cwd`, prompt for an unknown session, media prompt), `-32002` when `session/load` names no saved session, `-32000` when a prompt is already running (including a `session/load` of a session mid-prompt), `-32601` unsupported method. A message line is limited to 4 MB, embedded resources included: a longer one (a prompt that inlines a large @-mentioned file) is answered with `-32700` and the request's `id`, read from the start of the line, so the prompt ends with an error instead of waiting.

---

## Auto-fix Loop

In trusted projects, every edit triggers a lint and test cycle. Untrusted projects skip it until you run /trust.

After a turn's `Write`, `Edit` or `MultiEdit` calls in the interactive TUI (`-p`, the SDK and ACP do not run auto-fix), OxideClaw runs the project's linter and then its tests: an auto-detected runner (clippy / `cargo test`, ESLint / `npm test`, ruff / pytest, `go vet` / `go test`) when it is installed, or `autoFixLoop.lintCommand` / `autoFixLoop.testCommand` from your settings. Failures go back to the model for up to `autoFixLoop.maxRetries` (default 3) retries. `autoFixLoop.trigger` is `autonomous` (default: in every [autonomy mode](#autonomy-modes) but `suggest`), `always` or `off`.

Lint and test commands run the project's own code (`build.rs`, `conftest.py`, npm scripts, Makefiles), so nothing runs in a folder that is not in your `trustedProjects` list; the first skipped edit of a session says so once. `/trust` takes effect from the next edit, including the project's own `autoFixLoop` settings. In a trusted folder the commands run under the same sandbox as the Bash tool when one is enabled (`/sandbox enable`), and a command the sandbox refuses is skipped, never run unsandboxed. Under bwrap or firejail, a check that fails because of the sandbox itself (no network for a download, a tool it does not expose) is skipped with a note instead of being sent to the model as a broken edit.

**Language-server diagnostics.** While lint and tests run, each edited file also goes to its language server, the one the `LSP` tool uses (rust-analyzer, typescript-language-server, pyright-langserver or pylsp, gopls, clangd, jdtls, solargraph, lua-language-server), when one is on your `PATH`. OxideClaw waits for the server's diagnostics until 2 s after its first report (`autoFixLoop.lspSettleMs`) and never longer than 10 s in all, start-up included (`autoFixLoop.lspTimeoutMs`, at most 60000). New errors go to the model with the lint and test output as `file:line:col message`, at most 30 lines; warnings only with `autoFixLoop.lspWarnings: true`. Errors the file already had do not count: the check compares with what the server reported for the file before the turn's first edit to it; when it was not running yet, the server first checks the file as it was before that edit. Only if that gets no report in time does the check keep just the errors on lines the turn changed, and then that file does not count toward "checks passed". Language servers execute project code (build scripts, proc macros, plugins), so the trust and sandbox rules above apply to them: nothing starts in an untrusted folder, and in a trusted one a server auto-fix or the `LSP` tool starts runs under the Bash sandbox when one is enabled (the `LSP` tool refuses in an untrusted folder, headless and over ACP too: add the folder to `trustedProjects` there). A server the `LSP` tool already started is reused when no sandbox is enabled; with one, a server not started under the current sandbox (started before `/sandbox enable`, or with other sandbox settings) is stopped and started again inside it. Losing trust (`/trust revoke`, or `/reload` after the trust entry is gone) stops the running servers. Servers start on first use and get a clean `shutdown` when you quit. Only files inside the project go to its servers. Before each check and each `LSP` query, a file a server has open that changed on disk since it was sent (`/undo`, a shell edit, your editor) is sent again, and one deleted since is closed, so the server never works from an old copy. rust-analyzer builds into `target/rust-analyzer`, so it does not wait on the lock the lint and test cargo commands hold. A server that crashes, is refused by the sandbox, or does not answer `initialize` or take the files within the cap is dropped from the check for the rest of the session, with a note. A server that publishes only when diagnostics change (rust-analyzer) and has said nothing new for `lspSettleMs` is taken to have nothing new: its last report is read against the text it was published for, so a server still busy with the newest edit never moves an old error onto a new line, what that report said about lines the edit has since changed (an error the model just fixed) is not fed back, and a report for a text older than the one before is not used; one that has not reported on a file by the cap is kept and asked again next time. `autoFixLoop.lsp: false` turns the step off; `--bare`, which has no `LSP` tool, never runs it.

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

Every hook also receives `CLAUDE_HOOK_EVENT`, `CLAUDE_SESSION_ID`, `CLAUDE_CWD`, and `CLAUDE_PROJECT_DIR` (also as `OXIDECLAW_PROJECT_DIR`; the project root, which is the working directory), so a Claude Code hook written as `"$CLAUDE_PROJECT_DIR"/.claude/hooks/guard.sh` finds its script. Environment values over 64 KiB are cut (the OS limits one variable to about 128 KiB), and `TOOL_INPUT_TRUNCATED=1` (or `TOOL_RESULT_TRUNCATED`, `CLAUDE_MESSAGE_TRUNCATED`) is set when that happens. The full, uncapped event is always written to the hook's stdin as one JSON object: `hook_event_name`, `session_id`, `cwd`, and when present `tool_name`, `tool_input` (the parsed tool arguments), `tool_response`, and `prompt`. A guard that pattern-matches tool input should read stdin, for example `jq -r .tool_input.command`, so padding cannot push the dangerous part out of view.

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

**Exit codes:** `0` allow and continue · `2` block (the tool is not run, or the turn stops; `stopReason`, stdout or stderr is shown) · anything else is logged and ignored.

**JSON on stdout (optional):** `{ "decision": "block", "reason": "...", "continue": false, "stopReason": "...", "systemMessage": "...", "additionalContext": "..." }`. Claude Code's `{ "hookSpecificOutput": { "permissionDecision": "deny", "permissionDecisionReason": "...", "additionalContext": "..." } }` works too: `deny` blocks, and `ask` blocks as well, since a hook cannot force a prompt here. A hook can only block: `"decision": "approve"` and `"permissionDecision": "allow"` are ignored, and the call still goes through the normal permission check.

Each hook has a 60-second timeout and runs in its own process group, so a timed-out hook cannot leave children behind. `"disableAllHooks": true` in settings or `--bare` on the command line skips every hook.

---

## Sandboxing

OxideClaw supports multiple sandbox backends for tool isolation:

| Backend | Description |
|---------|-------------|
| `bwrap` | bubblewrap — lightweight Linux sandboxing. System dirs and per-user toolchains (`~/.cargo/bin`, `~/.rustup`, `~/.local/bin`, `~/.nvm`) are read-only, the rest of `$HOME` is hidden, `/tmp` is private |
| `firejail` | Firejail — security sandbox with profiles |
| `strict` | Best-effort denylist of catastrophic command literals — NOT isolation; no filesystem or network restriction (the only option on macOS/Windows) |

With any sandbox enabled, shell commands (and the auto-fix checks and language servers that run under it) do not inherit OxideClaw's provider credentials (`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY` and the other providers' key variables, `WHISPER_API_KEY`); a variable you set in settings.json `env` still reaches them. Outbound network stays on in `bwrap` and `firejail` until `/sandbox network off` (`"sandboxAllowNetwork": false`).

The project directory stays writable inside the sandbox, `.git/` included, except for what code outside the sandbox runs later: under `bwrap`, `.git/hooks`, `.git/config`, `.git/modules`, `.hg/hgrc`, `.sl/config`, `.claude/`, `.oxideclaw/`, `.agents/`, `.husky/`, `.githooks/`, `.mcp.json`, `.pre-commit-config.yaml` and `lefthook.yml` are read-only when they exist, and `.git` cannot be renamed or replaced, so a sandboxed command cannot plant a hook or `core.fsmonitor` that your next `git commit` runs unsandboxed (`git config`, `git remote add` and `git push -u` fail inside it; run them yourself). A missing `.git/hooks` is created empty first. With any sandbox, the git commands OxideClaw runs itself (snapshots, `/undo`, `/redo`, `/checkpoint`, `/diff`, `/branch`, `EnterWorktree`, `oxideclaw spawn`'s worktrees) never run repository hooks or `core.fsmonitor`; `/spawn merge`'s commit and merge run your hooks as git normally does. Snapshots stop with a warning if a repo-local `filter.*` driver appears or changes mid-session instead of running it outside the sandbox. Restart after reviewing `.git/config` to resume them.

---

## Autonomy Modes

The autonomy mode decides what runs without a permission prompt. Set it for the session with `/autonomy <mode>`, or for good with `"autonomy"` in your settings.json.

| Mode | Edits (`Write`, `Edit`, `MultiEdit`, `NotebookEdit`) | Commands, MCP tools |
|------|------|------|
| `suggest` | Always prompt, even when `permissions.allow` or `[a]lways` covers them; the auto-fix loop does not run | Prompt unless allowed |
| `ask` (default) | Prompt unless `permissions.allow` or `[a]lways` covers them | Prompt unless allowed |
| `auto-edit` | No prompt inside the project directory, except for the files below | Prompt unless allowed (the auto-fix lint/test run in a trusted project does not prompt) |
| `full-auto` | As `auto-edit`: no prompt inside the project, except for the files below; other edits follow the rules | Commands: no prompt (under bwrap, network off). MCP tools: prompt unless allowed |

`auto-edit` still prompts for edits outside the project (symlinks are resolved first) and for the files that change what the auto-fix, git or CI commands run, or that persist past the session: `.git/` (and Mercurial, Sapling and Jujutsu state: `.hg/`, `.sl/`, `.jj/`), `.claude/`, `.oxideclaw/`, `.agents/`, `.mcp.json`, `.env*`, hook config (`.husky/`, `.githooks/`, `.pre-commit-config.yaml`, `lefthook.yml`), CI config (`.github/`, `.gitlab/`, `.gitlab-ci.yml`, `.circleci/`, `.buildkite/`, `.travis.yml`, `Jenkinsfile`, ...) and build/test-runner config (`package.json`, `.npmrc`, ESLint configs, `jest.config.*`, `vitest.config.*`, `babel.config.*`, `karma.conf.*`, `Cargo.toml`, `build.rs`, `.cargo/`, `rust-toolchain.toml`, `conftest.py`, `pytest.ini`, `setup.py`, `setup.cfg`, `pyproject.toml`, `tox.ini`, `noxfile.py`, `.venv/`, `node_modules/`, `Makefile`, `justfile`, `*.csproj`, `*.props`, `*.targets`), wherever they sit in the tree. Names are matched as Windows resolves them, so `Makefile.` or `package.json:stream` prompt too. After `EnterWorktree`, "the project" is the worktree the tools write in. Started from your home directory (or above it), it pre-approves no edit at all. In a `/trust`ed project, `auto-edit` with the auto-fix loop runs the tests and sources the model just wrote (`cargo test`, `pytest`, `npm test`, ...) without a prompt, under the Bash sandbox only when one is enabled. Use `/sandbox enable bwrap`, or set `autoFixLoop.trigger` to `off`, to avoid that.

`full-auto` needs real isolation: the Bash sandbox enabled in `bwrap` mode (`/sandbox enable bwrap`), with bwrap installed and its network off (`/sandbox network off`, `"sandboxAllowNetwork": false`). bwrap confines only shell commands, leaving just the project directory writable to them; `firejail`'s default profile leaves all of `$HOME` writable, so it does not qualify. With the network on, a command injected by a file the model read could send the project anywhere without a prompt, so that does not qualify either. Without it `/autonomy full-auto` is refused and the mode stays as it was; a `full-auto` in settings.json starts as `ask` with a notice, and turning the sandbox off or its network on later drops back to `ask`. WebFetch and WebSearch run in-process, outside the sandbox, and need no prompt in any mode; add `deny` rules for them if a session must not reach the web. macOS and Windows have no such sandbox, so `full-auto` is unavailable there until a native one ships. Started from your home directory (or above it), where the sandbox would leave every dotfile writable, it pre-approves nothing and acts as `ask`, with a notice. File tools run in-process and MCP servers run as unsandboxed processes, so bwrap does not contain them: under `full-auto` edits get `auto-edit`'s rule (unprotected files inside the project only), and MCP tools, plan approval (`ExitPlanMode`), discarding a worktree's changes (`ExitWorktree` with `discard_changes`) and the browser's loopback question still ask unless a rule allows them.

In every mode `permissions.deny` rules refuse a call outright, without a prompt. OxideClaw has no `permissions.ask` list, but a settings file's Claude Code `ask` rules are read: an allow rule one of them narrows (`allow: ["Bash(git:*)"]` beside `ask: ["Bash(git push:*)"]`) is ignored with a notice, so the call prompts as it would in Claude Code instead of running unprompted. A command rule such as `Bash(git push:*)` also covers the command inside a subshell, `{ ...; }` group, `$(...)` or backtick substitution, `bash -c '...'` or `eval`, and after `VAR=value`, `env`, `command`, `nohup`, `time` or `sudo` prefixes and git's global options (`git -C dir push`, `git -c k=v push`); it matches the command's text, so a command that builds the name at run time (`g=git; $g push`, a script) is not caught, and only a sandbox confines that; a command with a quote, bracket or heredoc that does not close cannot be checked, so it prompts where a prompt is shown and is refused under `full-auto` and `--dangerously-skip-permissions`. The mode applies to the TUI, sub-agents, `-p` (where a call that would prompt is refused), `--headless` SDK sessions and ACP: there it fills in for tools the host's policy does not list, and never overrides the host's `deny` or `ask` lists; `suggest` does override the host's `auto_approve` and `allow` lists for edit tools, which still prompt. A project's `.claude/settings.json` may only make the mode stricter than yours as it runs (a `full-auto` that falls back to `ask` counts as `ask`); a looser value is ignored with a notice.

`--allowed-tools` and `--disallowed-tools` take tool names and permission rules for one run, separated by commas or spaces (spaces and commas inside parentheses stay in the rule), as separate arguments (`--disallowed-tools Bash WebFetch`) or by repeating the flag. Each flag reads every argument up to the next flag, so put the prompt first, end the list with `--` or write `--allowed-tools=<list>`; a prompt word read as a tool stops startup as an unknown tool. A bare name filters the tool list: `--allowed-tools Read,Grep` offers the model only those tools, `--disallowed-tools WebFetch` removes one, and `mcp__<server>` covers every tool of that server. A name only offers a tool; it pre-approves nothing. A rule with a specifier keeps its tool and acts like a settings rule: `--allowed-tools 'Bash(git status:*)'` runs `git status` without a prompt and asks for (in `-p`, refuses) every other command, and `--disallowed-tools 'Bash(git push:*)'` refuses `git push` in every mode, `--dangerously-skip-permissions` included. With bare names in `--allowed-tools` or a `--tools` list as well, a rule's tool is kept beside them; bare `--allowed-tools` names beside `--tools`, or `--allowed-tools` beside `--tools ""`, stop startup, since one list would have to be ignored. The rules join `permissions.allow` / `permissions.deny` in the TUI, sub-agents and `-p`; in `--headless` SDK sessions and ACP the host's policy decides what runs without asking, as it does for `permissions.allow`, while deny rules still hold. An unknown tool (in `--tools` too), unbalanced parentheses or a rule OxideClaw cannot parse stops startup with an error instead of being dropped.

Earlier versions accepted `auto-edit` (then the default) and `full-auto` but prompted for every edit in both. On the first run after upgrading, a stored `"autonomy": "auto-edit"` or `"full-auto"` in your OxideClaw settings.json is changed to `"ask"` once, with a message, so nobody is switched to unprompted edits by upgrading. A `--settings` file or JSON is reported instead of changed, on every run that applies it.

---

## Configuration

### Settings File

`~/.config/oxideclaw/settings.json` by default (see [Where files live](#where-files-live)), plus `<project>/.claude/settings.json` per project. `/status` prints the config directory in use:

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
| `effort` | `low` / `medium` / `high` / `xhigh` / `max` | unset | Sent as `output_config.effort` on Claude 4.6+ / Claude 5 (`xhigh` becomes `high` on Opus/Sonnet 4.6, which lack it), and as `reasoning.effort` on OpenAI reasoning models over the Responses API (`xhigh` and `max` become `xhigh` on GPT-5.2+ and gpt-5.1-codex-max, `high` elsewhere; GPT-5 `-pro` models get at least `high`); other models get a prompt nudge. Set with `/effort` |
| `openaiApi` | `auto` / `chat` / `responses` | `auto` | `auto`: the Responses API for `oai:`, Chat Completions for every other provider. `chat`: Chat Completions for `oai:` too. `responses`: also for `openai-compat:` and `lmstudio:`. `OXIDECLAW_OPENAI_API` overrides it |
| `spinnerStyle` | `themed` / `minimal` / `silent` | `themed` | Spinner animation style |
| `autonomy` | `suggest` / `ask` / `auto-edit` / `full-auto` | `ask` | What runs without a permission prompt; see [Autonomy Modes](#autonomy-modes) |
| `router` | `{ "low", "mid", "high", "superHigh", "enabled", "classifier" }` | unset | The [smart model router](#smart-model-router): a model per tier (any provider), `enabled` (unset: on once two tiers are set) and `classifier` (`heuristic` or `model`) |
| `routerEnabled` | `true` / `false` | unset | Older form of `router.enabled` |
| `routerBudget` | USD amount | unset | Session spend limit applied at startup (same as `/budget`), in the TUI, `-p` and SDK sessions; `--max-budget-usd` or a `session/start` `max_budget_usd` may lower it, not raise it, and an untrusted project's may only lower yours |
| `routerLowModel` | any model name | `claude-haiku-4-5` (your `model` on a non-Claude session) | Older form of `router.low` |
| `routerMediumModel` | any model name | `claude-sonnet-5` (your `model` on a non-Claude session) | Older form of `router.mid` |
| `routerHighModel` | any model name | your `model` | Older form of `router.high` |
| `routerSuperHighModel` | any model name | `claude-opus-5` (your `model` on a non-Claude session) | Older form of `router.superHigh` |
| `allowPrivateNetworkFetch` | `true` / `false` | `false` | Let WebFetch, WebBrowser and the `browser_*` tools reach loopback, RFC 1918, CGNAT and ULA addresses (e.g. a dev server on `localhost:3000`). Without it, `browser_navigate` asks once per loopback `host:port` in an interactive session (the TUI, `/browse`, an SDK host) and refuses elsewhere; LAN addresses are refused. Link-local and cloud metadata endpoints stay refused either way. Behind an `HTTP(S)_PROXY` the same check runs on the locally resolved address before the proxy is used; see [SECURITY.md](SECURITY.md#network-access-from-webfetch-and-webbrowser) |
| `updateCheck` | `true` / `false` | `true` | Once every 24 h the TUI looks up the latest GitHub release (the one `oxideclaw update` installs) in the background, with a 3 s timeout, and shows one dim line when it is newer than yours. `false` in any settings file turns it off; `-p`, `--headless`, `acp` and `browse` never check. The last answer is cached in `$XDG_CACHE_HOME/oxideclaw/update-check.json` (default `~/.cache/oxideclaw/`). Uses `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` |
| `env` | `{ "NAME": "value" }` | `{}` | Environment variables set on every Bash and PowerShell tool command. A project's `.claude/settings.json` may set them only in a folder you have `/trust`ed |

### CLAUDE.md / AGENTS.md / GEMINI.md

Drop a `CLAUDE.md` or `AGENTS.md` in your project root to give the agent project-specific context. These files are automatically injected into the system prompt. The global ones are read from the config dir (`~/.config/oxideclaw/CLAUDE.md`, `AGENTS.md`, `GEMINI.md`), falling back to Claude Code's `~/.claude/` copies when OxideClaw has none. A `GEMINI.md` (Gemini CLI's name for the same file) is read too, by the same rules: the global copy, then one in each directory from the filesystem root (or your home) down to the working directory. It goes after CLAUDE.md and AGENTS.md, as the lowest-priority source, and only when one exists. As with the other two, a symlinked file inside the project is ignored.

### Skills

A skill is a reusable prompt you run as `/<name> [args]`, or the agent runs with the `Skill` tool after finding it with `DiscoverSkills`. `/skills` lists what is loaded. OxideClaw reads the [Agent Skills](https://agentskills.io) layout, a folder per skill:

```
.agents/skills/release/
├── SKILL.md          # YAML frontmatter + instructions
└── scripts/bump.sh   # supporting files, referenced from SKILL.md
```

```markdown
---
name: release
description: Cut a release, bump the version and tag it
---
Run scripts/bump.sh with the new version, then ...
```

As in Claude Code, every field is optional: `name` defaults to the folder's name and `description` to the first paragraph of the body, and a `SKILL.md` with no frontmatter is all body. Other fields (`license`, `allowed-tools`, `metadata`, ...) are accepted and ignored. Only the name and description are loaded at startup. The body of `SKILL.md` is read when the skill runs, and the model is given the skill's folder so it reads the supporting files only when the instructions need them. A `SKILL.md` with malformed frontmatter, a name that is not one command word, or nothing to describe it (no `description` and an empty body) is skipped, and a startup notice (and the `DiscoverSkills` output) lists each skipped path with the reason.

Skills are looked up in this order; on a name collision the first one wins, and the built-in skills (`commit`, `review`, `explain`, `fix`, `test`) only fill names nothing else uses:

| # | Directory | Holds |
|---|-----------|-------|
| 1 | `<project>/.agents/skills/` | `<name>/SKILL.md` only (shared with other agents) |
| 2 | `<project>/.oxideclaw/skills/` | `<name>/SKILL.md` and flat `<name>.md` |
| 3 | `<project>/.claude/skills/` | both; read-only import from Claude Code |
| 4 | `<config dir>/skills/` | both |
| 5 | `~/.claude/skills/` | both; read-only import from Claude Code |

`<project>` is the working directory and then each parent up to the repo root (the nearest directory with a `.git`), nearer first, so a monorepo's root skills work from `packages/web`. Outside a git repo only the working directory is used.

Flat `<name>.md` skills from earlier versions still load: plain markdown, `# Title` / description / `---` / prompt, or YAML frontmatter with `params`. `{{ARGS}}` and `{{param}}` placeholders are filled from the arguments; arguments with no placeholder to go in are appended to the prompt.

A skill is only a prompt: frontmatter grants no tools or permissions, so a cloned repository's skills go through the same permission prompts, sandbox and `/trust` rules as anything you type, and `disableSkillShellExecution` removes the shell tools from every skill turn. A skill file that is, or links to, a file your `Read(...)` deny rules cover is skipped with a notice, so a repository cannot ship `notes.md -> ../.env` as a skill.

### .env Files

Auto-loaded from (in order):
1. `$CWD/.env`
2. `~/.env`
3. `.env` in the config dir (`~/.config/oxideclaw/.env`; also that path when `$XDG_CONFIG_HOME` moves the config dir)

Only oxideclaw's own keys (provider API keys, `ANTHROPIC_MODEL`, `OLLAMA_HOST`, ...) are read; `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are not, so export those in your shell. `OLLAMA_HOST` and `ANTHROPIC_MODEL` decide where your prompts are sent, so `$CWD/.env` may set them only in a folder you have `/trust`ed; otherwise they are ignored with a note.

### Where files live

| Purpose | Default | Override |
|---------|---------|----------|
| Config (`settings.json`, global `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`, skills, `memory.md`, plugins, `local-mcp/`) | `~/.config/oxideclaw/` | `$OXIDECLAW_CONFIG_DIR`; else `$XDG_CONFIG_HOME/oxideclaw/` (an absolute path). `$CLAUDE_CONFIG_DIR` still works for one more release, with a warning, unless it names `~/.claude` or a directory Claude Code has used (one holding `projects/`, `todos/`, `statsig/`, `.claude.json` or `.credentials.json`); the first run then migrates from that directory |
| Sessions | `~/.local/share/oxideclaw/sessions/` | `$XDG_DATA_HOME/oxideclaw/sessions/`. With `$OXIDECLAW_CONFIG_DIR` (or `$CLAUDE_CONFIG_DIR`) and no `$XDG_DATA_HOME`, `<config dir>/sessions/` |
| Cache: code index (`rag/`) and the update-check answer | `~/.cache/oxideclaw/` | `$XDG_CACHE_HOME/oxideclaw/` (an absolute path; a relative one is ignored) |
| Skills | `<config dir>/skills/`, plus the project and `~/.claude/` directories in [Skills](#skills) | — |
| Project memories (`/memory`) | `<project>/.claude/memory.db`, created on first use | — |

OxideClaw is XDG Base Directory compliant and never writes to Claude Code's `~/.claude`. It reads from it, as an import format, the global `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` (when the config dir has none), skills, agents, output styles and workflows; OxideClaw's own copies win.

**Upgrading from a version that used `~/.claude`.** On the first run, when the config dir does not exist yet (or holds only a `.env`), OxideClaw copies its own state out of `~/.claude` and prints what it did: sessions (only OxideClaw's `<id>.meta` / `.jsonl` files and snapshots), `memory.md`, `plugins.json`, `local-mcp/`, `bannerOrgDisplay` from `config.json`, and from `settings.json` the `model`, the `/trust` list, plain preferences (spinner, router, TTS, auto-commit, ...), `env` entries from the `.env` allowlist, the MCP servers of installed plugins, `permissions.deny`, and the safety settings whose value only tightens (`sandboxEnabled: true` with its `sandboxMode`, `sandboxAllowNetwork: false`, `allowPrivateNetworkFetch: false`, `autonomy: "suggest"`, `browseDefaultPolicy: "ask"`, `browseApprovalPatterns`, and an `autoFixLoop` / `autoRollback` that turns the loop off), so upgrading never turns on what you had turned off. Hooks, allow rules, `apiKeyHelper` and other MCP servers run code or change permissions, so they are listed, not copied; `oxideclaw config import-claude` shows them and `--hooks`, `--permissions`, `--api-key-helper`, `--mcp` copy them (Claude Code's hook format is converted, allow rules that one of its `ask` rules narrows are skipped, an `apiKeyHelper` is refused when Claude Code's `env` points it at a gateway, Bedrock or Vertex (`ANTHROPIC_BASE_URL`, `CLAUDE_CODE_USE_BEDROCK`, ...), since OxideClaw would send those keys to api.anthropic.com, and `--mcp` also reads the servers `claude mcp add` keeps in `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`: user-scope ones go to `settings.json`, the current project's local-scope ones to its private local MCP file); `--sessions` imports the current directory's Claude Code sessions ([Importing Claude Code sessions](#importing-claude-code-sessions)). Settings that loosen something or pick commands or endpoints (an enabled `autoFixLoop`, `ollamaHost`, `defaultShell`, `sandboxEnabled: false`, ...) are named so you can set them again; `voiceEnabled` is left behind with a custom `voiceApiUrl`, so voice never falls back to OpenAI's endpoint with your recording and key. `~/.claude` itself is never modified. Sessions older versions kept in `$XDG_CONFIG_HOME/oxideclaw/sessions/` move to the data dir. `/status` and `oxideclaw doctor` show the directories in use.

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
| `GROQ_API_KEY`, `OPENROUTER_API_KEY`, `DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, `TOGETHER_API_KEY`, `MISTRAL_API_KEY`, `VENICE_API_KEY` | API key for the matching OpenAI-compatible provider |
| `GOOGLE_API_KEY` | API key for `gemini:` models when `GEMINI_API_KEY` is unset |
| `OPENAI_API_KEY` | API key for `oai:` models, and for `openai-compat:` endpoints that need one |
| `OPENAI_BASE_URL` | Endpoint for `openai-compat:` models (required for that prefix; shell only, not `.env`) |
| `OXIDECLAW_OPENAI_API` | `auto`, `chat` or `responses`; overrides `openaiApi` in settings.json. Shell only, not `.env` |
| `LM_STUDIO_HOST` | LM Studio server URL (default: `http://localhost:1234/v1`; shell only, not `.env`) |
| `OXIDECLAW_NO_UPDATE_CHECK` | `1` turns off the TUI's daily update check (same as `"updateCheck": false`). Shell only. |
| `OXIDECLAW_BROWSER_NO_SANDBOX` | `1` lets `/browse` run Chrome without its sandbox when OxideClaw runs as root (Docker, CI); pages then run unsandboxed as root. Shell only. |
| `OXIDECLAW_CONFIG_DIR` | Config directory (default `~/.config/oxideclaw`). Shell only, not `.env` |
| `CLAUDE_CONFIG_DIR` | Deprecated alias for `OXIDECLAW_CONFIG_DIR`, honoured with a warning for one more release; ignored when it names `~/.claude` or another directory Claude Code has used |
| `XDG_CONFIG_HOME` | Config directory base (`$XDG_CONFIG_HOME/oxideclaw`) |
| `XDG_DATA_HOME` | Sessions directory base (`$XDG_DATA_HOME/oxideclaw`), under the rule in [Where files live](#where-files-live) |
| `XDG_CACHE_HOME` | Cache directory base (code index, update-check answer) |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY` (either case) | Egress proxy for WebFetch and WebBrowser (per scheme for WebFetch; WebBrowser chains `http://` proxies only). Hosts are resolved and checked locally before the request goes to the proxy, private and `NO_PROXY` hosts connect directly; see [SECURITY.md](SECURITY.md#network-access-from-webfetch-and-webbrowser) |
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
├── voice.rs          # Recording + Whisper STT + XTTS v2 TTS
├── router.rs         # Optional model routing across providers, with escalation
├── cost.rs           # Token/cost tracking + budget enforcement
├── sandbox.rs        # bwrap / firejail / strict
└── config.rs         # Settings, CLAUDE.md/AGENTS.md/GEMINI.md injection
```

Built with `tokio`, `ratatui`, `reqwest` (rustls), `clap`, `serde_json`, `tree-sitter`.
