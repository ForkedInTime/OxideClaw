<p align="center">
  <img src="assets/banner.png" alt="RustyClaw — Claude Code, carved in Rust" width="100%">
</p>

<p align="center">
  <a href="https://github.com/ForkedInTime/RustyClaw/releases"><img src="https://img.shields.io/github/v/release/ForkedInTime/RustyClaw?style=flat-square&color=B23616" alt="Release"></a>
  <a href="https://github.com/ForkedInTime/RustyClaw/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ForkedInTime/RustyClaw/ci.yml?style=flat-square&label=CI&color=B23616" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-B23616?style=flat-square" alt="License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-2024_edition-F08A3E?style=flat-square&logo=rust" alt="Rust"></a>
  <a href="https://github.com/ForkedInTime/RustyClaw/stargazers"><img src="https://img.shields.io/github/stars/ForkedInTime/RustyClaw?style=flat-square&color=F08A3E" alt="Stars"></a>
</p>

<h3 align="center">The Claude Code experience — native, offline-capable, and in a single 19 MB binary.</h3>

<p align="center">
  No Node. No Python. No 80 MB of <code>node_modules</code>. No flickering TUI.<br>
  Indexes your codebase, routes each task to the cheapest capable model, and runs agents in parallel.
</p>

<p align="center">
  <img src="assets/demo.gif" alt="RustyClaw demo" width="100%">
</p>

---

## Install

**Linux / macOS:**
```bash
curl -fsSL https://raw.githubusercontent.com/ForkedInTime/RustyClaw/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
Invoke-WebRequest https://github.com/ForkedInTime/RustyClaw/releases/latest/download/rustyclaw-windows-x64.exe -OutFile rustyclaw.exe
```
Then move `rustyclaw.exe` somewhere on your `PATH` (e.g. `%USERPROFILE%\bin`).

<details>
<summary>Other install methods</summary>

**From source (Rust 2024 edition):**
```bash
git clone https://github.com/ForkedInTime/RustyClaw.git
cd RustyClaw && cargo build --release
./target/release/rustyclaw
```

**Specific version (Linux/macOS):**
```bash
curl -fsSL https://raw.githubusercontent.com/ForkedInTime/RustyClaw/main/install.sh | bash -s v0.2.0
```

Pre-built binaries attached to every [release](https://github.com/ForkedInTime/RustyClaw/releases):
- Linux: `x86_64-linux-gnu`, `aarch64-linux-gnu`, `x86_64-linux-musl`
- macOS: `x86_64-apple-darwin` (Intel), `aarch64-apple-darwin` (Apple Silicon)
- Windows: `rustyclaw-windows-x64.exe`
</details>

**Linux / macOS:**
```bash
echo 'ANTHROPIC_API_KEY=sk-ant-...' >> ~/.env
rustyclaw
```

**Windows (PowerShell):**
```powershell
"ANTHROPIC_API_KEY=sk-ant-..." | Out-File -FilePath $HOME\.env -Encoding utf8 -Append
rustyclaw
```

---

## Why RustyClaw?

> **Every other "Rust port" of Claude Code re-implements the CLI and stops there.**
> RustyClaw takes the Rust advantage and builds the features a native binary makes possible — an on-disk codebase index, a smart router that keeps your bill down, parallel agents in git worktrees, voice I/O, and a `/undo` that actually works.

|  | Claude Code (npm) | Other Rust ports | **RustyClaw** |
|---|---|---|---|
| Runtime | Node.js / Bun | Rust | **Rust** |
| Binary | ~50 MB + `node_modules` | ~15 MB | **19 MB static, zero deps** |
| Cold start | ~300 ms | ~50 ms | **sub-50 ms** |
| Memory idle | ~150 MB | ~40 MB | **~10 MB** |
| Ollama tool-use | No | Broken / partial | **Working** |
| Codebase RAG | No | No | **tree-sitter + FTS5, 8 langs** |
| Model router | No | No | **Auto-route by task complexity** |
| Parallel agents | No | No | **Git-worktree isolation** |
| Voice I/O | No | No | **Whisper + XTTS v2 cloning** |
| Browser automation | External MCP server | No | **9 CDP tools, in the binary** |
| Autonomous browser agent | No | No | **Goal-driven, 50-step cap, safety-gated** |
| Auto-fix loop | No | No | **Post-edit lint + tests + retry** |
| `/undo` · `/redo` | No | Partial (pollutes git log) | **Invisible shadow refs** |
| OpenAI-compat providers | No | Partial | **9 providers, working tools** |
| Sandbox | No | No | **bwrap / firejail / strict** |
| CLAUDE.md + AGENTS.md | Partial | No | **Both, with `/reload`** |

---

## Feature tour

### 🧠 &nbsp; Local codebase RAG — zero setup

tree-sitter AST parsing, SQLite FTS5 semantic search. Index your whole repo in seconds. Indexes stay on disk and update incrementally.

```
> /rag search "TOCTOU"
HAS match "search TOCTOU" — 10 results
  src/tools/read.rs:12 (module `search`, rust)
  src/session/mod.rs:17 (comment, rust)
  ...
```

### 💰 &nbsp; Smart model router + live cost dashboard

Simple edits go to Haiku or Ollama. Architecture questions go to Opus. Every token is priced in real time. Cap the bill with `/budget $5` — RustyClaw warns at 80% and stops the loop when the budget is exceeded.

### 🎭 &nbsp; Parallel agents in git worktrees

```bash
rustyclaw spawn "refactor the auth middleware"
# runs in an isolated git worktree while you keep working in the main tree
```

### 🎤 &nbsp; Voice I/O with XTTS v2 cloning

Push-to-talk speech input (Whisper). TTS responses in any voice, including a clone of your own after a 6-second sample. **No competitor ships this.**

### ♻️ &nbsp; Auto-fix loop — anti-cheat protected

Every `Write`/`Edit` kicks off a lint + test cycle. Failures feed back into the next turn for up to three retries. The old rollback-on-fail behaviour is gone — RustyClaw fixes forward.

### ↩️ &nbsp; `/undo` and `/redo` on shadow refs

Every assistant turn silently snapshots the working tree to `refs/rustyclaw/sessions/<id>/<n>`. Invisible to `git log`, `git branch`, `git status`. Never pushed. Use the `/undo` picker or skip straight to a turn with `/undo 3`. **Other tools with undo pollute your history. RustyClaw doesn't.**

### 🔌 &nbsp; Works offline via Ollama — with working tool use

Full tool use over Ollama's native format. Other Rust ports have had this broken or partial for months — ours just works. Auto-falls back to prompt-injected JSON on models that don't support native tools.

### 🌐 &nbsp; Built-in browser automation — no extra server

Eight CDP-driven tools — `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_fill`, `browser_screenshot`, `browser_get_text`, `browser_press_key`, `browser_wait` — shipped in the binary and enabled by default. Snapshots return a text tree with stable `@eN` element refs you can pass to click/fill. Works against any Chromium-based browser (Chrome, Chromium, Brave, Edge) you already have installed. No external automation server, no separate install.

### 🤖 &nbsp; Autonomous browser mode — `/browse <goal>`

Give it a goal, it drives. `/browse find the cheapest flight SF to Tokyo on July 7` navigates, fills forms, scrolls, reads results, and speaks the answer. 50-step hard cap (configurable), destructive-action approval gate (pauses at payment / delete / OAuth / free-trial-autobill), stagnation detector (escalating nudges when the model is stuck). `rustyclaw browse "<goal>" --json` runs the same loop headless from scripts or CI. `/voice` with prefixes `browse | browser | web | go to | open | shop for | book | order` drives it hands-free with milestone TTS at start, gate trip, and end.

### 🦀 &nbsp; Single 19 MB static binary

No runtime. No dependencies. No post-install scripts. `scp` it to a server and run. Cross-compiled for `x86_64-linux-gnu`, `aarch64-linux-gnu`, and `x86_64-linux-musl` on every release.

### 🛡️ &nbsp; Sandbox-first execution

Shell commands can run under `bwrap`, `firejail`, or a `strict` mode (no network, read-only FS). Approvals are per-session, per-command-family.

### 📁 &nbsp; Respects your config like a native tool

XDG Base Directory compliant (`$XDG_CONFIG_HOME/rustyclaw`, `$XDG_DATA_HOME`, `$XDG_CACHE_HOME`). Reads **both** `CLAUDE.md` and `AGENTS.md` (3,518 upvotes on the Claude Code repo). Hot-reload with `/reload` — no restart.

See **[FEATURES.md](FEATURES.md)** for the complete reference (30+ tools, 60+ slash commands, every config knob).

---

## Quick start

```bash
# First-run setup
rustyclaw /init         # generates CLAUDE.md from your repo
rustyclaw /doctor       # verifies API keys, models, and sandbox

# Day-to-day
rustyclaw               # interactive TUI
rustyclaw --headless    # NDJSON stdio for editor/CI embedding (see sdk/)

# Inside the TUI
/help                   # interactive command menu
/model                  # pick a model (Claude + Ollama + 9 OpenAI-compat providers)
/rag search <query>     # semantic codebase search
/budget $5              # cap the bill
/voice                  # voice I/O + TTS picker
/spawn <task>           # parallel agent in a git worktree
/undo                   # step back to any previous turn
```

`.env` files auto-load from `$CWD/.env`, `~/.env`, or `~/.config/rustyclaw/.env`.

---

## What it looks like

<details open>
<summary>Screenshots</summary>
<br>
<table>
<tr>
<td width="50%">

**Streaming codebase conversation**
![Streaming](assets/conversation-streaming.png)

</td>
<td width="50%">

**Completed response with cost tracking**
![Complete](assets/conversation-complete.png)

</td>
</tr>
<tr>
<td width="50%">

**Codebase RAG search**
![RAG search](assets/rag-search.png)

</td>
<td width="50%">

**Model picker — Claude + Ollama + OpenAI-compat**
![Model picker](assets/ollama-models.png)

</td>
</tr>
<tr>
<td width="50%">

**Session manager**
![Session picker](assets/session-picker.png)

</td>
<td width="50%">

**Interactive help**
![Help menu](assets/help-menu.png)

</td>
</tr>
<tr>
<td width="50%">

**Doctor diagnostics**
![Doctor](assets/doctor.png)

</td>
<td width="50%">

**Cost dashboard**
![Cost](assets/cost-dashboard.png)

</td>
</tr>
<tr>
<td width="50%">

**Voice I/O — XTTS v2**
![Voice](assets/voice-status.png)

</td>
<td width="50%">

**Keybindings overlay**
![Keybindings](assets/keybindings.png)

</td>
</tr>
</table>
</details>

---

## Documentation

| Document | Description |
|----------|-------------|
| [FEATURES.md](FEATURES.md) | Complete feature reference — every command, shortcut, and config option |
| [sdk/](sdk/) | SDK / headless mode — protocol, examples, integration guide |
| [CHANGELOG.md](CHANGELOG.md) | Release history |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to contribute |
| [SECURITY.md](SECURITY.md) | Security policy and vulnerability reporting |

---

## License

Apache 2.0 — see [LICENSE](LICENSE).

---

<p align="center">
  <img src="assets/logo-128.png" alt="" width="48" height="48"><br>
  <sub>Built on Arch Linux. No rust was harmed in the making of this binary.</sub>
</p>
