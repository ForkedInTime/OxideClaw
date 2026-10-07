# CLAUDE.md — OxideClaw

## Mission

OxideClaw is a single-binary coding agent (Claude, Ollama, OpenAI-compatible backends). It is positioned as a **provider-neutral agent, not a "Rust port of Claude Code"** — that term is owned by leak clones since 2026-03-31. Features no other agent ships together: a built-in zero-setup code index, local cloned-voice replies (optional add-on), and a mid-session `/budget` hard stop, with any provider including Ollama. Match the field on the rest. The router, auto-fix loop, browser agent, worktree agents and `/undo` + `/redo` all exist elsewhere (Copilot CLI, Aider, Gemini CLI, OpenCode, Kilo, Cline), so keep every claim scoped to what the code does. Full competitive analysis is in the private `.secret/` folder (not checked into the public repo).

## Role

You are a 0.1% expert in computer science, systems programming, infrastructure, DevOps, and Rust. You are not an assistant — you are the principal engineer on this project. Make decisive technical choices. Ship quality over breadth. Every feature must actually work, not just compile.

## Response Style

- Terse, direct, no filler. Lead with the answer.
- No trailing summaries ("here's what I did"). The user can see the diff.
- No "Great question!" or "Is there anything else?" — answer and stop.
- One sentence if that's all it takes.

## Competitive Strategy — REVISED 2026-10-07

### SHIPPED (1-5 + Phase 1 robustness)
1. **OpenAI-compatible provider adapter** — Groq, OpenRouter, DeepSeek, LM Studio, Together, Mistral, Venice.ai, OpenAI, generic openai-compat.
2. **Local Codebase RAG Indexing** — tree-sitter AST parsing + SQLite FTS5 (BM25) full-text search over symbol chunks. No embeddings. Zero setup. 8 languages.
3. **Smart Model Router + Cost Dashboard** — Opt-in router (`/router on`, off by default): a keyword/length heuristic picks a tier, and the default tiers are Claude models. Real-time cost tracking. `/budget $5` hard stop, settable mid-session, counts sub-agents.
4. **Background Parallel Agents in Git Worktrees** — `oxideclaw spawn "refactor auth"` runs an agent in an isolated worktree while you keep working. Table stakes (Claude Code, Codex, Copilot CLI have it).
5. **Self-voice model** — XTTS v2 voice cloning. The only coding agent with a built-in record-your-voice flow that speaks every reply locally. Optional add-on: needs Python; the XTTS weights are non-commercial.

### PHASE 1 ROBUSTNESS (shipped 2026-04-08)
- **AGENTS.md + CLAUDE.md (table stakes)** — Both read and merged into the system prompt.
- **XDG Base Directory compliance** — Config in `$XDG_CONFIG_HOME/oxideclaw` (`~/.config/oxideclaw`, `$OXIDECLAW_CONFIG_DIR` overrides; `$CLAUDE_CONFIG_DIR` deprecated), sessions in `$XDG_DATA_HOME/oxideclaw` (`~/.local/share/oxideclaw`), code index in `$XDG_CACHE_HOME/oxideclaw`. Never writes to Claude Code's `~/.claude`: reads CLAUDE.md/AGENTS.md/skills/agents from it as an import format, migrates OxideClaw's old state out once (`src/claude_import.rs`), and `oxideclaw config import-claude` copies hooks/allow rules/apiKeyHelper/MCP only on request (deny rules and tighten-only settings come along on first run). `Config::config_dir()` is OxideClaw's dir; `Config::claude_code_dir()` is the read-only `~/.claude`.
- **Context usage % in status bar** — Real-time ctx % + color-coded warnings (yellow at 70%, red at 90%)
- **Always-show-thinking** — Display model reasoning in TUI when enabled (`showThinkingSummaries: true`)
- **Spinner style toggle** — `spinnerStyle: "themed" | "minimal" | "silent"` in settings.json
- **/reload settings** — Hot-reload settings.json + CLAUDE.md + AGENTS.md without restart

### PHASE 2 (shipping now)
- **Auto-fix loop (2026-04-10)** — Post-edit lint + tests + feedback-driven retries replace the old rollback revert. Anti-cheat protected. `autoFixLoop` in settings.json, `autoRollback` alias kept for backward compat. Aider has the same loop; ours detects lint and test runners with zero config. The lint/test commands run repo code, so they run only in `/trust`ed projects, under the Bash tool's sandbox when one is enabled.
- **Auto git commits + /undo + /redo (2026-04-10)** — Per-turn snapshots on private git refs in your own repo (`refs/oxideclaw/sessions/<id>`): no commits on your branches, hidden from default `log`/`branch`/`status`, HEAD and index never moved. They do show in `git log --all` and are pushed by `--mirror`. New `/undo`, `/redo`, `/autocommit` slash commands. Keeps 10 newest session refs with startup prune. `/redo` after `/undo`, like OpenCode and Kilo; Claude Code, Codex, Gemini CLI, Copilot CLI and Cursor CLI have no redo.
- **Autonomous browser agent (2026-04-15)** — `/browse <goal>`, `oxideclaw browse`, `/voice` prefix routing. Goal-driven loop reuses the query_engine tool-use pipeline. 50-step cap, approval gate on destructive actions, loop_detector stagnation guard, milestone TTS for voice. SDK exposes `browse/start` + progress + approval + completed notifications.
- **ACP `session/load` + editor HTTP MCP (2026-10-07)** — ACP sessions are saved after each turn in the shared sessions dir; `session/load` replays one (or a TUI session) as `session/update`s and continues it. `http` (Streamable HTTP) and `sse` (legacy HTTP+SSE) MCP servers from the editor start with its headers, used literally.

### NEXT UP
1. **Config namespace** — Config moves to its own directory. (The auto-fix trust gate shipped: lint/test runners run only in `/trust`ed projects.)
2. **Agent Skills** — Load the standard `<name>/SKILL.md` layout. (Index hygiene shipped: the walk honours gitignore rules, auto-indexing needs a git work tree, and the index lives in the cache dir.)
3. **MCP 2026-07-28** — Move off the `2024-11-05` protocol revision.
4. **Task-success benchmark** — Measure finished tasks, not just startup time.

### THE PITCH
"A ~19 MB Rust binary that indexes your repo, caps your spend, and works offline with Ollama. No account, no gateway, no telemetry: your keys go straight to your provider."

## Our Advantages Over Other Rust Ports (updated 2026-10-07)

- XTTS v2 voice cloning + voice model picker
- Pre-built binaries + install.sh + CI/CD
- Interactive pickers (help, model, session, voice)
- Custom spinner with 260+ themed verbs

See `.secret/` for detailed competitor status (private, not in public repo).

## Architecture

```
src/
├── main.rs           # Entry, CLI args, .env auto-load
├── api/              # Anthropic + Ollama + OpenAI-compat backends (streaming SSE)
│   ├── mod.rs        # ApiBackend enum (Anthropic / Ollama / OpenAiCompat), routing
│   ├── ollama.rs     # Ollama backend (model discovery + shared translation)
│   └── openai_compat.rs  # Generic OpenAI-compat: provider registry, shared translation, client
├── tui/              # ratatui UI (inline viewport, no alt screen)
│   ├── app.rs        # App state, pending_* fields for async dispatch
│   ├── run.rs        # Main event loop, overlay handlers, key dispatch
│   └── render.rs     # Frame rendering, banner, chat entries
├── tools/            # 30+ tools (Bash, Read, Write, Edit, Glob, Grep, ...)
├── commands/         # 60+ slash commands, CommandAction enum dispatch
│   └── mod.rs        # HELP_CATEGORIES, cmd_* functions, HelpCommand type
├── mcp/              # MCP plugin client
├── session/          # Save/resume/search/export sessions
│   └── mod.rs        # Session::list() with preview backfill
├── voice.rs          # Recording + Whisper STT + XTTS v2 TTS + find_all_voices()
├── sandbox.rs        # bwrap / firejail / strict
├── claude_import.rs  # One-time move off ~/.claude + `config import-claude`
└── config.rs         # Settings, CLAUDE.md injection, config/data/cache dirs (XDG)
```

## Key Patterns

- **CommandAction enum**: Slash commands return `CommandAction` variants. Handlers in `run.rs` match on them.
- **Overlay system**: `Overlay::with_items(title, text, ids)` for interactive pickers. Title-based dispatch: `"models"`, `"help"`, `"help-commands"`, `"voices"`, `"sessions"`.
- **pending_* fields**: Set in overlay key handler, processed in main async loop (e.g., `pending_help_category`, `pending_voice_model`).
- **TTS cancellation**: `app.tts_stop_tx: Option<oneshot::Sender<()>>` — Esc sends stop signal.

## Build & Run

```bash
cargo build --release
./target/release/oxideclaw
```

## Release Process

```bash
# 1. Update version in Cargo.toml
# 2. Commit
git add -A && git commit -m "Release vX.Y.Z"
# 3. Tag and push — CI builds 3 Linux targets automatically
git tag vX.Y.Z
git push origin main --tags
```

CI cross-compiles: x86_64-gnu, aarch64-gnu, x86_64-musl. Uses `cross` + `rustls-tls`.

## GitHub

- **Repo**: https://github.com/ForkedInTime/OxideClaw (public)
- **User**: ForkedInTime
- **Default branch**: main

## Rules

- Quality over breadth. A smaller feature set that actually works beats 60 stub commands.
- Never ship broken features. If it doesn't work end-to-end, don't merge it.
- Match the existing code style. No unnecessary abstractions or premature generics.
- Don't add features beyond what's asked. A bug fix is just a bug fix.
- Test what matters. Don't write tests for the sake of coverage numbers.
