# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.3.x   | Yes       |
| < 0.3   | No        |

## Reporting a Vulnerability

If you discover a security vulnerability in OxideClaw, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please email the maintainers or use [GitHub's private vulnerability reporting](https://github.com/ForkedInTime/OxideClaw/security/advisories/new).

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

### Response Timeline

- **Acknowledgment** — within 48 hours
- **Assessment** — within 1 week
- **Fix or mitigation** — as soon as practical, depending on severity

## Security Considerations

OxideClaw executes shell commands and modifies files as part of its core functionality. Users should be aware of:

- **API keys** — stored in `.env` files. Never commit these to version control.
- **Tool execution** — the AI agent can run Bash commands. Use sandboxing (`bwrap`, `firejail`, `strict`) for untrusted workloads.
- **MCP plugins** — third-party plugins execute with the same permissions as OxideClaw. Project-scoped plugins (`.claude/settings.json`, `.mcp.json`), project hooks and a project `apiKeyHelper` are ignored until you run `/trust` in that folder — a cloned repository cannot run commands on your machine by itself.
- **HTTP MCP servers use static headers.** The bearer token in `mcpServers.<name>.headers` is sent as-is; OxideClaw has no OAuth refresh flow. When a token expires the server returns 401, the failure is reported, and you replace the token and restart.
- **SDK / Headless mode** — the NDJSON server accepts commands on stdin. Secure the transport layer in production deployments.

## Sandboxing

OxideClaw supports multiple sandbox backends to limit tool execution:

```
bwrap      — bubblewrap, lightweight Linux sandboxing
firejail   — security sandbox with predefined profiles
strict     — most restrictive, minimal filesystem access
```
