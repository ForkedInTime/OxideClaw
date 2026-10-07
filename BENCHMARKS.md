# Benchmarks

Startup and footprint of terminal coding agents, measured with
`scripts/bench.py` (no dependencies; run it yourself). Every number below is
from one machine on one day; the point is the method and the order of
magnitude, not the last millisecond. Two measurements: [`--version` cold
start](#results) for every tool, and [time to first frame](#time-to-first-frame)
of the interactive UI.

**What is measured.** Executable size on disk (following symlinks; script
wrappers are flagged rather than sized), wall-clock time of `<tool> --version`
(the runtime's cold start, before any network or model work), and the peak
resident set of that process. `--version` is the fairest common denominator:
every tool has it and it exercises the same thing, loading the runtime.
It says nothing about model quality or how fast a turn completes.

**Reproduce:**

```bash
cargo build --release
PATH="$PWD/target/release:$PATH" scripts/bench.py --runs 20 oxideclaw claude codex gemini goose opencode jcode codewhale
```

## Results

Machine: dolphin · x86_64 · Linux 6.18.49-3-lts
Date: 2026-09-11 · runs per tool: 20 · command: `<tool> --version`

| Tool | Version | Executable | Cold start (median) | p95 | Peak RSS |
|---|---|---|---|---|---|
| oxideclaw | oxideclaw 0.3.2 | 19.4 MB | 3 ms | 8 ms | 19 MB |
| claude | 2.1.268 (Claude Code) | 208.5 MB | 14 ms | 24 ms | 40 MB |
| codex | codex-cli 0.153.4 | 246.7 MB | 15 ms | 20 ms | 22 MB |
| gemini | 0.37.1 | script wrapper | 456 ms | 463 ms | 218 MB |
| goose | 1.32.0 | 259.0 MB | 8 ms | 18 ms | 26 MB |

Notes on this run: `claude`, `codex`, and `goose` are native executables
(Claude Code 2.1.x ships a Bun-bundled binary; Codex and Goose are Rust).
`gemini` is a JavaScript entry point run by Bun/Node, so its executable size
is not comparable and its start time includes the runtime boot. Tools not on
this machine (OpenCode, jcode, Codewhale, claurst, ante) are omitted rather
than copied from their own READMEs; add them to the command above to measure
them locally.

## Time to first frame

`--version` only loads the runtime. Time to first frame is how long from
launch until the interactive UI is on screen, which is what a user waits
for.

**Method.** `scripts/bench.py --first-frame` starts each tool with no
arguments in a pseudo-terminal of 120x40 with `TERM=xterm-256color`, in an
empty temporary git repo, with a temporary `HOME` and XDG directories and no
inherited environment except `PATH`, so no user config or credentials are
read. Each launch is fresh. The clock runs from spawn to the first frame:
the last output before 50 ms of quiet, once at least 20 visible characters
(escape sequences do not count) are on screen; with `--marker TOOL=TEXT`,
the first time that text appears instead. The 50 ms of quiet is not
included. The harness answers what a terminal answers (cursor position,
device attributes, colours), because a TUI that asks and gets no reply waits
out a timeout no real terminal would make it wait. Each launch is then
stopped with SIGTERM, then SIGKILL. A tool that shows a sign-in screen or
exits asking for a key is reported as `needs login` rather than timed; one
that draws nothing within `--timeout` (10 s) is reported as `timeout`.
Median and p95 over `--runs`.

**Reproduce:**

```bash
cargo build --release
PATH="$PWD/target/release:$PATH" scripts/bench.py --first-frame --runs 20 oxideclaw
# no Ollama on the machine: answer its model listing so the keyless path runs
PATH="$PWD/target/release:$PATH" scripts/bench.py --first-frame --ollama-stub --runs 20 oxideclaw
# check the harness itself against dummy TUIs
scripts/bench.py --self-test
```

Other tools take the same command; they are measured only where installed,
never copied from their own READMEs.

### Results

Machine: vm · x86_64 · Linux 6.18.44-fc-v77 (4 vCPUs)
Date: 2026-10-07 · runs: 20 · terminal: 120x40 xterm-256color · first frame:
20 visible characters then 50 ms quiet

| Tool | Version | Path | First frame (median) | p95 | Frame begins |
|---|---|---|---|---|---|
| oxideclaw | oxideclaw 0.4.0 | keyless Ollama (`--ollama-stub`) | 29 ms | 36 ms | `No Anthropic key found; using local Ollama model bench-stub:` |
| oxideclaw | oxideclaw 0.4.0 | no key, no Ollama | needs login | | `Error: No Anthropic credential found. OxideClaw checks, in o` |

Notes on this run: with no key, OxideClaw starts on a local Ollama model
when one answers on `OLLAMA_HOST`, and otherwise exits with the
missing-credential error, which is not a frame. This machine has no Ollama,
so the first row answers Ollama's `/api/tags` and `/api/show` from a stub on
a local port: the time includes OxideClaw's real probe of it, and the frame
is OxideClaw's full first screen (the keyless notice, the prompt and the
status line); no model runs before the first frame. A real Ollama reads
each pulled model's metadata to answer `/api/show`, so the probe can take
longer there (it gives up after 800 ms). Measured on a shared VM, so read
the order of magnitude.
