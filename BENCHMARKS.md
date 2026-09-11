# Benchmarks

Startup and footprint of terminal coding agents, measured with
`scripts/bench.py` (no dependencies; run it yourself). Every number below is
from one machine on one day; the point is the method and the order of
magnitude, not the last millisecond.

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
