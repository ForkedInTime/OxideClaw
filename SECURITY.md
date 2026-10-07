# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.4.x   | Yes       |
| < 0.4   | No        |

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
- **Tool execution** — the AI agent can run Bash commands. Use an isolating sandbox (`bwrap` or `firejail`, Linux only) for untrusted workloads; `strict` mode is a pattern denylist and provides no containment.
- **MCP plugins** — third-party plugins execute with the same permissions as OxideClaw. Project-scoped plugins (`.claude/settings.json`, `.mcp.json`), project hooks and a project `apiKeyHelper` are ignored until you run `/trust` in that folder — a cloned repository cannot run commands on your machine by itself.
- **HTTP MCP servers use static headers.** The bearer token in `mcpServers.<name>.headers` is sent as-is; OxideClaw has no OAuth refresh flow. When a token expires the server returns 401, the failure is reported, and you replace the token and restart.
- **SDK / Headless mode** — the NDJSON server accepts commands on stdin. Secure the transport layer in production deployments.

## Network access from WebFetch and WebBrowser

WebFetch and WebBrowser run without an approval prompt, so a prompt-injected turn could aim them at the cloud metadata service or a service on your network. Every destination, including each redirect hop, is checked against the addresses its hostname resolves to before anything is sent:

- **Always refused:** link-local (`169.254.0.0/16`, `fe80::/10`, where `169.254.169.254` lives), unspecified, multicast, broadcast and IPv4 documentation ranges, and the metadata endpoints outside link-local (`100.100.100.200`, `fd00:ec2::254`).
- **Refused unless `allowPrivateNetworkFetch: true`:** loopback, RFC 1918, CGNAT (`100.64.0.0/10`) and ULA (`fc00::/7`).
- A direct connection is pinned to the checked addresses, so a second DNS answer cannot redirect it.

**Behind a proxy** (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, honouring `NO_PROXY`) the hostname is still resolved and checked locally first; the proxy never sees a request the policy refuses. Public destinations are then sent to the proxy by name, private ones (when allowed) and `NO_PROXY` hosts connect directly with the same pinning. The proxy resolves the name again itself, so the address pin cannot extend through it.

A name that does not resolve locally (split-horizon DNS where only the proxy can resolve it) can only be judged by its shape. It goes to the proxy only if it looks public: cloud metadata names (`metadata.google.internal`, `metadata`, `instance-data`, and `metadata.` or `instance-data.` under a local-network domain such as `instance-data.eu-west-1.compute.internal`) are always refused, and local-network names (single-label names, `localhost`, `.local`, `.internal`, `.lan`, `.corp`, `.home.arpa`, ...) only pass with `allowPrivateNetworkFetch: true`. For such names, and for what a public name resolves to at the proxy, **the proxy is treated as trusted egress**: if it can reach your internal network or a metadata service, restrict it there.

## Sandboxing

OxideClaw supports multiple sandbox backends to limit tool execution:

```
bwrap      — bubblewrap, lightweight Linux sandboxing
firejail   — security sandbox with predefined profiles
strict     — best-effort command denylist only; no filesystem or network isolation
```
