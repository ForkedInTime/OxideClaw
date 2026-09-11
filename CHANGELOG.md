# Changelog

All notable changes to RustyClaw will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security

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

### Fixed

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

[Unreleased]: https://github.com/ForkedInTime/RustyClaw/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/ForkedInTime/RustyClaw/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/ForkedInTime/RustyClaw/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/ForkedInTime/RustyClaw/releases/tag/v0.1.0
