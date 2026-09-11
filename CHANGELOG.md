# Changelog

All notable changes to RustyClaw will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`rustyclaw acp` — Agent Client Protocol.** RustyClaw can now be the
  agent behind Zed, JetBrains, and any ACP client: `initialize`,
  `session/new`, `session/prompt` with streamed `session/update`s, tool-call
  status, `session/request_permission` for tools the policy marks *ask*, and
  `session/cancel` that stops a running model stream mid-turn. Built on the
  SDK sidecar's session engine; verified live for a full turn and for a
  mid-turn cancel.
- SDK sessions can be cancelled (`CancelSignal`) and report why a turn ended
  (`TurnEnd`).

### Fixed

- **Extended thinking works on Claude 5 again.** The request sent
  `{"type":"enabled","budget_tokens":N}` to every model; Sonnet 5, Opus 5
  and Fable 5.1 reject that with a 400, and Haiku 4.5 rejects the newer
  `adaptive` form. The shape is now chosen per model generation, the budget
  is clamped to the API's `1024 ≤ budget < max_tokens` window, and
  `--thinking disabled` sends `{"type":"disabled"}` instead of an illegal
  zero budget. Verified against the live API on all three generations.
- **`/effort` is a real parameter.** It used to inject a sentence into the
  prompt. It now sets `output_config.effort` (`low|medium|high|max`) on
  Claude 4.6+ / Claude 5, persists to settings, and falls back to the prompt
  nudge only on models without the parameter (Haiku 4.5, Ollama,
  OpenAI-compatible). `/effort off` clears it.

### Changed

- **README repositioned.** RustyClaw is presented as a provider-neutral
  coding agent rather than a "Rust port of Claude Code". The comparison
  table now names Claude Code, Codewhale, jcode, and claurst and only claims
  what each project documents (checked 2026-09-11).
  Second verification pass: Codewhale's cloud TTS tool (MiMo, voice clone
  on request) and `/restore` are now credited; the voice claim is scoped to
  local, every-reply, own-voice TTS.
- **Hooks are documented.** README and FEATURES.md describe the eight
  lifecycle events, environment variables, exit codes, and JSON output.
- README browser tool count is 9 (`browser_console` was missing from the
  tour); readme-lint no longer counts the `browse_done` loop terminator.

### Fixed

- **LSP queries no longer pay a full language-server start per call.** One
  server per (command, project root) is kept for the session. File URIs are
  percent-encoded, so paths with spaces resolve.
- **The browser agent's visible-price signal is live.** The last snapshot's
  text is kept on the session, so a click on a page showing a price prompts
  even when the button text is innocuous. The stagnation detector now keys
  on the element acted on, so clicks on different elements that return the
  same text are no longer mistaken for a loop.
- The code index missed an edit made within the same second as the previous
  index (whole-second mtimes); nanosecond mtimes now.
- `Config` tool reported the startup model after `/model` changed it.
- SDK: a `session/start` with a `cwd` that is not a directory is refused up
  front (`invalid_cwd`) instead of failing every tool call.
- Startup lists spawn worktrees a previous crash left behind, with the merge
  or remove command for each.
- `bwrap`/`firejail` availability is probed once per process, not on every
  Bash call. The package-manager probe during plugin install no longer
  freezes the TUI.

## [0.3.2] - 2026-09-10

Phases 7–15 of the code review. **Upgrade from 0.3.1**: it still has the
clone-and-own settings hole, the headless deep-link execution and the sub-agent
plan-mode bypass fixed here.

### Added

- **SDK: `health/check` now reports `protocol_version`** (currently 1). It
  changes only on an incompatible wire change; hosts should gate on it
  rather than on the crate version.

### Changed

- **Default model is now `claude-sonnet-5`** (was `claude-sonnet-4-6`): the
  current generation, and cheaper per token. `/model` aliases follow suit:
  `opus` → `claude-opus-5`, `sonnet` → `claude-sonnet-5`, `haiku` →
  `claude-haiku-4-5`, new `fable` → `claude-fable-5-1`; the 4.6 ids remain
  available by name. The `/model` picker and the smart router's defaults
  list the current generation. Nothing changes for users who set a model
  explicitly in settings or on the command line.
- Settings writes (`/trust`, `/config set`, MCP server registration, banner
  label) are atomic: a crash mid-write cannot truncate `settings.json`.
- Three slash-command actions that nothing could trigger (`PersistModel`,
  `PreviewVoiceModel`, `ShowCostDashboard`) were removed along with their
  dead handlers; `/model`, `/voice` and `/cost` are unaffected.

### Security

- **The browser agent's form-field protections were never wired.** The
  approval gate had patterns for password and card fields, but production
  never passed it a signal, so a `browser_fill` into "Card number" or
  "Password" was never gated. Fields are now recognised by their accessible
  name. And after such a fill is approved, pressing **Enter** to submit the
  form is gated too — previously it submitted with no button for the
  button-text patterns to see.
- **The browser agent echoed what it typed.** A fill result read
  `Filled @e3 with "<value>"`, so passwords and card numbers went into the
  transcript on disk. It now reports the length only. (The byte slice at 50
  also panicked on a multi-byte character.)

- **A cloned repository could run commands on your machine.** Its
  `.claude/settings.json` and `.mcp.json` could define hooks (run around
  every tool call), an `apiKeyHelper` shell command (run at startup) and MCP
  servers (spawned at startup), and RustyClaw honoured them. Those three are
  now ignored from project settings until you run **`/trust`** in that
  folder, which records it in the global `trustedProjects` list. Startup
  says what was ignored. Everything non-executable in project settings still
  applies.
- **Deep links no longer run anything.** `rustyclaw-cli://open?q=…` used to
  start a headless agent session with your credentials, triggerable from any
  web page once the handler was registered. It now opens the interactive TUI
  with the prompt in the input box for you to review, and refuses to run at
  all without a terminal. Re-run `rustyclaw --register-protocol` so the
  handler opens a terminal. Non-ASCII queries were also mangled by the
  percent-decoder.
- **A symlinked `CLAUDE.md` / `AGENTS.md` is refused.** A repository could
  point one at `~/.ssh/id_rsa` and have the key read into the system prompt
  sent to the API.

- **MCP resource reads were uncapped.** Tool results were already limited to
  25K characters; `resources/read` was not, so one large or hostile resource
  flooded the context window. Same cap now. Server-supplied tool descriptions
  (untrusted text that goes into the model's prompt) are capped at 2,000
  characters and always carry the `[MCP: server]` provenance prefix.
- **HTTP MCP responses are refused past 32 MiB** instead of being buffered.

- **`TeamDelete` took an unvalidated name** and used it as a path component:
  `../../.claude/settings` deleted an arbitrary `.json` under home and could
  `remove_dir_all` a directory. `SendMessage`'s recipient had the same hole.
  Both now accept `[A-Za-z0-9_-]` only (experimental agent-teams feature,
  off by default).
- **Notebook tools bypassed the sensitive-path deny-list** that Read/Write/
  Edit honour, and `NotebookEdit` needed no approval. Both now go through the
  same checks; `NotebookEdit` prompts like `Edit`, is blocked in plan mode,
  and writes atomically.
- `Skill` accepted a path as the skill name and read markdown from anywhere.

### Fixed

- **Text-to-speech went silent on multi-line replies.** The XTTS request
  body was hand-escaped (quotes and backslashes only), so any newline or tab
  in the model's text produced invalid JSON the server rejected. Built with
  serde now.
- **Voice temp files were shared by every RustyClaw on the machine**
  (`rustyclaw-voice.wav` and three XTTS files under the temp dir): two
  sessions clobbered each other's audio, and a fixed name in a world-writable
  temp dir is a pre-created-symlink target. Per-process names now.
- The Whisper transcription request had no timeout.
- **Plan mode now applies to sub-agents.** It was enforced only in the
  session's own tool loop; an `Agent` launched during plan mode could write
  and run commands. The block list rides on the permission gate, which
  children inherit.
- The session picker and `/sessions` could panic on a session id shorter
  than 8 characters (a hand-edited or foreign `.meta` file).
- **SDK: a late approval reply for an earlier prompt denied the current tool
  and left the real answer queued**, cascading down every following prompt.
  Replies for other approval ids are now skipped.
- **SDK: a malformed request line with a multi-byte character at byte 200
  crashed the sidecar** (byte-slice preview). Char-safe now. Over-long
  lines are drained in bounded chunks instead of being buffered whole
  before the 4 MB check.
- **The cost dashboard used the wrong prices for every current Claude
  model.** Opus was charged at 3× the published rate and Haiku at ¼, so
  `/budget` stopped a session far too early or far too late. Rates now match
  Anthropic's list prices per generation (Fable $10/$50, Opus 4.6+ $5/$25,
  Sonnet 5 $2/$10, Sonnet 4.6 $3/$15, Haiku 4.5 $1/$5). Third-party rows the
  code itself calls "rough" are now flagged as estimates in the dashboard.
- **A hung MCP server blocked startup for a minute, and several hung servers
  blocked it for a minute each.** Servers now connect concurrently under a
  20-second per-server budget; a server that does not answer is skipped with
  a warning.
- **HTTP MCP servers never received `notifications/initialized`**; spec-strict
  servers reject every request until they do.
- **The code index kept chunks for deleted or renamed files** until a forced
  re-index, so search returned code that no longer existed. Incremental
  re-index now prunes them.
- A skill file with an empty frontmatter block (`---` directly followed by
  `---`) was rejected as malformed.
- **The `LSP` tool never worked**: the language server was killed the instant
  it was spawned (the child handle was dropped with kill-on-drop), so every
  query timed out after 15 s. The server now lives as long as the client, runs
  in the project directory, and a server that dies fails in-flight requests
  immediately instead of after the timeout.
- `CronList` panicked on a prompt with a multi-byte character at the 80-byte
  cut. Step values beyond the field range (`*/99`) are rejected. The job store
  is written atomically.
- `NotebookRead`/`NotebookEdit` and `LSP` panicked on a bare `~` path.

### Removed

- **The cron tools** (`CronCreate`, `CronList`, `CronDelete`). They recorded
  jobs in `~/.claude/cron_jobs.json` that nothing in RustyClaw ever ran, so a
  user asking for a recurring reminder got a confident "job created" and then
  nothing. Rather than build a scheduler for a feature RustyClaw does not
  sell, the tools are gone. The file, if you have one, is left untouched.

## [0.3.1] - 2026-09-10

Every item below comes from the Phase 5 and Phase 6 code reviews. **Upgrade from
0.3.0**: it still has the SSRF and the sub-agent permission bypass fixed here.

### Security

- **Sub-agents ran with no permission check at all.** The approval gate lived
  only in the TUI loop; the engine behind the `Agent` tool, `/spawn`, and
  `-p` print mode had none. A model could call `Agent { prompt: "…" }` and
  the child ran Bash/Write/Edit unprompted. There is now one
  `PermissionGate`: the TUI plugs in its prompt, `Agent` children inherit the
  parent's gate (their prompts reach the same user), and a headless engine
  fails closed on anything that would have needed a prompt.
- **`Agent` nesting was unbounded.** Capped at 2 levels below the session.
- **`permissions.deny` now holds under `--dangerously-skip-permissions`.**
  Bypass skips prompts; it no longer overrides a rule the user wrote down.

- **WebFetch / WebBrowser could reach the cloud metadata service, loopback
  and private networks** with only a scheme check, and followed redirects
  unchecked. Destinations are now resolved and classified before any
  connection (link-local always refused; loopback/private refused unless
  `allowPrivateNetworkFetch: true`), the connection is pinned to the checked
  addresses, and every redirect hop is re-checked. Responses are refused past
  5 MiB instead of being buffered whole. `browser_navigate` refuses
  link-local/metadata addresses too.
- **WebBrowser passed the model's URL straight to `chromium --dump-dom`**, so
  `file:///etc/passwd` printed the file and a flag-shaped string became a
  Chromium switch. Validated before spawn; the browser is killed on timeout
  instead of orphaned.

### Changed

- **`rustyclaw -p` fails closed on sensitive tools.** Print mode used to run
  Bash/Write/Edit with no check. It now applies `permissions.allow`/`deny`,
  `--allowedTools` and `--dangerously-skip-permissions`, and refuses anything
  else with a message saying how to allow it. This matches Claude Code's
  print-mode semantics.
- `/spawn` says up front that the background agent runs without approval
  prompts (settings deny rules still apply), and refuses a 9th concurrent
  agent.

### Fixed

- **`/merge` of a conflicting spawn left your checkout mid-merge and deleted
  the worktree** you would have needed to resolve it. It now aborts the merge
  and keeps the worktree, branch and registry entry.
- **Quitting the TUI mid-spawn leaked the agent's worktree** next to your
  repo. Shutdown now cancels running agents, removes their worktrees and
  branches, and prints where completed unmerged work is.
- `/kill` showed the agent as *failed* instead of *cancelled*. Two spawns with
  the same description collided on the branch name. Worktree paths are
  passed as OS strings (non-UTF-8 paths no longer fall back to `/tmp`).
- The in-memory task registry is capped at 1000 entries.
- **WebSearch returned 401 for every OAuth user** — it always sent
  `x-api-key`, but the credential chain puts a bearer token there. It also
  had no request timeout.

## [0.3.0] - 2026-09-10

This release contains every fix from the enterprise security audit (PRs #11–#20).
**Users on 0.2.0 or earlier should upgrade** — several of the items under
*Security* are exploitable by a hostile repository or a prompt-injected model turn.

### Security

- **PowerShell bypassed both the approval gate and the sandbox.** The tool was
  absent from the sensitive-tool list (no prompt, ever) and was never wrapped by
  `apply_sandbox` — arbitrary unprompted, unsandboxed execution on any machine
  with `pwsh`. Now gated and sandboxed identically to Bash. (#12)
- **Symlinks bypassed the sensitive-path deny-list.** A repository could ship
  `notes.md -> ~/.ssh/id_rsa` and have it read, or overwrite credentials through
  a link. Paths are now canonicalised before the check. `Grep` also returned the
  contents of files `Read` refuses; it now honours the same guard. (#16)
- **Prefix allow-rules were bypassable by command chaining.** An allow rule for
  `git status` also allowed `git status\nrm -rf /` and `git status & …`.
  Compound commands are now split on every shell separator and each part is
  checked. PowerShell had no compound splitting at all. (#15)
- **Sandbox failed open on an unknown mode.** An unrecognised `sandboxMode` in
  `settings.json` ran commands unsandboxed while the UI said "enabled". Unknown
  modes are now rejected. `firejail` also ignored `allow_network`. (#12)
- **`strict` mode no longer claims to be isolation.** Modes are split into
  real isolation (bwrap, firejail) and best-effort denylist; the platform
  ceiling is stated and enabling a non-enforcing mode warns. (#13)
- **Hooks failed open and had no timeout.** A hook that failed to spawn, or was
  signal-killed, was treated as "allow". A hanging hook blocked the agent
  forever. PreToolUse hooks now fail closed, run under a 60 s timeout in their
  own process group, and have bounded output. (#12)
- Browser navigation rejects `javascript:`, `data:`, `file:` and other
  non-http(s) schemes. Sandbox `cwd` is shell-quoted. RAG indexer skips
  symlinked files. `bwrap` runs with `--new-session` (TIOCSTI class). Path
  traversal in file tools. (#11, #14, a8e99b0)
- **Dependency advisories cleared** — `cargo audit` reported 8 vulnerabilities
  in the 0.2.0 lockfile: `rustls-webpki` (RUSTSEC-2026-0098/0099/0104, name
  constraints + CRL panic), `h2` (2026-0258), `quinn-proto` (2026-0185),
  `crossbeam-epoch` (2026-0204) and `quick-xml` (2026-0194/0195, via
  `self_update`). All patched. The unused `tokio-fs` 0.1 dependency (and the
  tokio-0.1-era tree behind it, incl. an unsound `memoffset`) is removed.
- **`rustyclaw update` verifies what it installs.** `self_update` 0.44 → 1.3:
  the GitHub-published SHA-256 digest of the downloaded asset is checked before
  install. The asset is also now selected by exact name — the old substring
  match could pick `rustyclaw-linux-x64.sha256` or the musl build for a glibc
  x64 host depending on listing order.

### Added

- **Autonomous browser agent** — `/browse <goal>`, `rustyclaw browse <goal>`,
  and `/voice` prefix routing. Goal-driven loop with a 50-step cap, approval
  gate on destructive actions, stagnation detection, and milestone TTS. SDK
  exposes `browse/start` with progress, approval and completed notifications.
  (#6–#9)
- **Browser automation tools** — 8 CDP tools (navigate, snapshot, click, fill,
  screenshot, text, key, wait) plus `browser_console`; `/browser`, `/screenshot`
  slash commands; `browserCdpEndpoint` and `browserTimeoutMs` settings.
- **`/watch`** — file watcher with AI-marker scanning and debounce, bounded to
  the working directory. **`/diff`** — unified diff viewer.
- **Skills** — YAML frontmatter, named parameters with defaults, categories.
- **OAuth credential chain** compatible with the official SDK:
  `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` → active OAuth profile → default
  profile, first match wins. (#12)
- **API retry with backoff** — 408/429/500/502/503/504 and transport errors
  retried with `retry-after` honoured, exponential backoff with jitter, capped
  at 5 attempts. Wired into all three provider backends. (#20)
- **CI gates** — clippy with `-D warnings` on all three platforms, README-claim
  lint, perf smoke test, and a standing tool-schema contract test. (#5, #13, #14)

### Fixed

- **A crash mid-append could lose the whole conversation.** Session appends are
  now fsynced and the JSONL loader tolerates a torn final line. (#18)
- **A recovered session could be permanently unusable.** Torn-line recovery
  could leave a `tool_use` with no `tool_result`, 400-ing every subsequent
  request; the pair is now repaired on load. (#19)
- **Streamed responses were replayed on mid-stream retry.** The user saw a
  truncated answer followed by the full one, and the doubled text was saved.
  (#20)
- **SSE streams hung forever** on a silent connection; both the Anthropic and
  OpenAI-compatible (incl. Ollama) paths now enforce a 120 s inter-event
  timeout. (#12)
- **Writes were not atomic.** `Write`, `Edit` and session metadata now write to
  a sibling temp file, fsync, and rename, preserving file mode. `MultiEdit` is
  now actually all-or-nothing. `Glob` results are bounded. (#11, #17)
- **Auto-commit shadow refs could be clobbered by a second RustyClaw process**
  in the same repo; updates now use compare-and-swap and surface conflicts.
  Prune tie-break fixed. (#14)
- **Unbounded memory growth** — TUI scrollback, streaming channel, Bash output
  buffering, browser console buffer and various histories are now capped.
  Newline-free Bash output no longer OOMs. (#12, #15, 9abcffa)
- Bash and PowerShell no longer inherit the TUI's stdin (interactive commands
  fought crossterm for keystrokes). (#12)
- Chrome processes and temp dirs leaked on browser session close. CDP client
  now detects connection death and cleans its pending map.
- Cost dashboard: unknown-model pricing is flagged as an estimate instead of
  silently assumed; summary no longer aborts on NaN. (#14)
- `NO_COLOR` and `TERM=dumb` are respected. `/tmp` fallbacks replaced with the
  platform temp dir. TOCTOU race in session load. Windows CI. (#11)
- Web tools send a user agent carrying the real crate version.

### Removed

- The Agent tool's unimplemented `run_in_background` parameter, and ~200 LOC of
  unwired browser scaffolding and dead diff-review state. (#3, #11)

## [0.2.0] - 2026-04-10

### Added

- **SDK / Headless mode** — `--headless` flag starts an NDJSON stdio server for embedding in editors, CI/CD, scripts, and custom UIs. Full protocol reference in [`sdk/`](sdk/).
- **Phase 1 robustness** — AGENTS.md support, XDG Base Directory compliance, context usage % in status bar, always-show-thinking, spinner style toggle, `/reload` hot-reload.
- **Auto-commit loop** (Phase 2 robustness #1) — every assistant turn now takes a full-tree snapshot on a private shadow ref at `refs/rustyclaw/sessions/<id>`. New `/undo`, `/redo`, and `/autocommit` slash commands. `autoCommit.{enabled,keepSessions,messagePrefix}` settings with startup prune. Zero impact on the user's real git index.
- **Auto-fix loop** (Phase 2) — After the model edits code, RustyClaw runs
  project-appropriate lint + test commands and, on failure, injects the
  output back as a synthetic user turn for up to `maxRetries` rounds
  (default 3, cap 10). Supports Rust (`cargo clippy` + `cargo test`),
  Node (`npx eslint` + `npm test`), Python (`ruff check` + `pytest`),
  and Go (`go vet` + `go test`). Anti-cheat clause in the feedback
  prompt blocks `#[allow(dead_code)]`-style escapes.
  Configure via `autoFixLoop` in `settings.json`; `autoRollback` still
  works as an alias.

### Changed

- The `auto_rollback` module has been renamed to `autofix` and no longer
  reverts files on failure. On retry-cap, the working tree is left
  as-is; use `/undo` or `git checkout` to revert manually.

## [0.1.0] - 2026-04-07

### Added

- Initial release.
- **Anthropic API backend** — streaming SSE, all Claude models.
- **Ollama backend** — local model discovery, tool-use fallback, model picker.
- **OpenAI-compatible providers** — Groq, OpenRouter, DeepSeek, LM Studio, Together, Mistral, Venice.ai, OpenAI, generic endpoints.
- **30+ tools** — Bash, Read, Write, Edit, Glob, Grep, WebFetch, WebSearch, Agent, LSP, Jupyter, MCP plugins, and more.
- **60+ slash commands** — `/help`, `/model`, `/session`, `/voice`, `/doctor`, `/rag`, `/budget`, and more.
- **RAG indexing** — tree-sitter AST parsing + SQLite FTS5 search across 8 languages.
- **Smart model router** — auto-detect task complexity, route to cheapest capable model.
- **Cost tracking** — real-time token/cost dashboard with budget limits.
- **Voice I/O** — Whisper STT + Piper TTS + XTTS v2 voice cloning.
- **Session management** — save, resume, search, export conversations.
- **Interactive pickers** — model, session, help, voice model selection with previews.
- **Custom spinner** — 260+ themed verbs with animated glyphs and completion stats.
- **Sandboxing** — bwrap / firejail / strict isolation.
- **Inline TUI** — ratatui-based, no alt screen, zero flicker.
- **Cross-compilation** — CI builds x86_64-gnu, aarch64-gnu, x86_64-musl via `cross`.
- **Install script** — one-liner install with version pinning.

[Unreleased]: https://github.com/ForkedInTime/RustyClaw/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/ForkedInTime/RustyClaw/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/ForkedInTime/RustyClaw/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/ForkedInTime/RustyClaw/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ForkedInTime/RustyClaw/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ForkedInTime/RustyClaw/releases/tag/v0.1.0
