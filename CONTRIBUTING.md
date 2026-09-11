# Contributing to OxideClaw

Thank you for your interest in contributing to OxideClaw! This document provides guidelines and information for contributors.

## Getting Started

### Prerequisites

- Rust (2024 edition) — install via [rustup](https://rustup.rs/)
- Linux (primary target)
- An Anthropic API key (for testing API features)
- Ollama (optional, for local model testing)

### Building from Source

```bash
git clone https://github.com/ForkedInTime/OxideClaw.git
cd OxideClaw
cargo build --release
./target/release/oxideclaw
```

### Running Tests

```bash
cargo test
```

## How to Contribute

### Reporting Bugs

1. Check [existing issues](https://github.com/ForkedInTime/OxideClaw/issues) first.
2. Open a new issue using the **Bug Report** template.
3. Include: steps to reproduce, expected behavior, actual behavior, and your environment (OS, Rust version, terminal).

### Suggesting Features

1. Check [existing issues](https://github.com/ForkedInTime/OxideClaw/issues) for similar requests.
2. Open a new issue using the **Feature Request** template.
3. Describe the use case, not just the solution.

### Submitting Code

1. Fork the repository.
2. Create a feature branch from `main`.
3. Make your changes.
4. Run `cargo test` and `cargo clippy` — ensure both pass.
5. Submit a pull request against `main`.

## Code Style

- Follow existing patterns in the codebase.
- No unnecessary abstractions or premature generics.
- Quality over breadth — a smaller change that works is better than a large one that doesn't.
- Don't add features beyond what's needed. A bug fix is just a bug fix.
- Use `cargo fmt` before committing.
- Address `cargo clippy` warnings.

## Architecture Overview

See [FEATURES.md](FEATURES.md#architecture) for the full source tree. Key patterns:

- **CommandAction enum** — slash commands return `CommandAction` variants, matched in the main event loop.
- **Overlay system** — `Overlay::with_items()` for interactive pickers.
- **Streaming SSE** — all API backends stream responses via server-sent events.
- **pending_* fields** — async dispatch pattern for overlay selections.

## Pull Request Guidelines

- Keep PRs focused — one feature or fix per PR.
- Include tests for new functionality.
- Update `CHANGELOG.md` under `[Unreleased]` for user-facing changes.
- Write clear commit messages that explain *why*, not just *what*.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](LICENSE).
