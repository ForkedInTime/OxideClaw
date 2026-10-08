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
- **HTTP MCP servers use static headers.** The bearer token in `mcpServers.<name>.headers` (or in the `headers` an ACP client passes with an `http` or `sse` server) is sent as-is; OxideClaw has no OAuth refresh flow. When a token expires the server returns 401, the failure is reported, and you replace the token and restart. The headers go only to the server's own origin: a redirect to another host, port or scheme (https to http included) is not followed, and the error names its target.
- **SDK / Headless mode** — the NDJSON server accepts commands on stdin. Secure the transport layer in production deployments.

## Network access from WebFetch and WebBrowser

WebFetch and WebBrowser run without an approval prompt, so a prompt-injected turn could aim them at the cloud metadata service or a service on your network. Every destination, including each redirect hop, is checked against the addresses its hostname resolves to before anything is sent:

- **Always refused:** link-local (`169.254.0.0/16`, `fe80::/10`, where `169.254.169.254` lives), unspecified, multicast, broadcast and IPv4 documentation ranges, and the metadata endpoints outside link-local (`100.100.100.200`, `fd00:ec2::254`).
- **Refused unless `allowPrivateNetworkFetch: true`:** loopback, RFC 1918, CGNAT (`100.64.0.0/10`) and ULA (`fc00::/7`).
- A direct connection is pinned to the checked addresses, so a second DNS answer cannot redirect it.

**Behind a proxy** (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`, honouring `NO_PROXY`) the hostname is still resolved and checked locally first; the proxy never sees a request the policy refuses. Public destinations are then sent to the proxy by name, private ones (when allowed) and `NO_PROXY` hosts connect directly with the same pinning. The proxy resolves the name again itself, so the address pin cannot extend through it.

Credentials in the proxy URL (`http://user:pass@proxy:3128`) are added for WebBrowser and the CDP browser by their local policy proxy, which listens on `127.0.0.1` where any local user can connect. On Linux it adds them only for clients running as your user, judged from the kernel's socket table; other clients are still checked and chained, without your credentials. macOS and Windows have no such check yet, so on a shared machine use an unauthenticated egress proxy there.

A name that does not resolve locally (split-horizon DNS where only the proxy can resolve it) can only be judged by its shape. It goes to the proxy only if it looks public: cloud metadata names (`metadata.google.internal`, `metadata`, `instance-data`, and `metadata.` or `instance-data.` under a local-network domain such as `instance-data.eu-west-1.compute.internal`) are always refused, and local-network names (single-label names, `localhost`, `.local`, `.internal`, `.lan`, `.corp`, `.home.arpa`, ...) only pass with `allowPrivateNetworkFetch: true`. For such names, and for what a public name resolves to at the proxy, **the proxy is treated as trusted egress**: if it can reach your internal network or a metadata service, restrict it there.

### The `browser_*` tools (CDP browser)

A Chrome that OxideClaw launches sends every connection, loopback included, through a local proxy that applies the same checks to each one: redirects, link clicks, script navigation and subresources, not only the URL given to `browser_navigate`. The rules are the ones above, with one addition for local development: without `allowPrivateNetworkFetch`, `browser_navigate` to a loopback `host:port` asks you once (in the TUI, as a `/browse` approval, or as a `tool/approval_needed` for `browser_loopback` to an SDK host), and that service alone stays reachable for the rest of the session. The grant is for the name you approved (the prompt shows what it resolves to), not for any name that resolves to the same address, so a page cannot rebind its own name onto your dev server. With `allowPrivateNetworkFetch: true` the pages Chrome visits can reach loopback and LAN services too; the proxy then remembers whether each name first resolved to a public or a private address for the session and refuses an answer that switches class or mixes both, so a public page cannot rebind its own name onto them. Requests a page makes to a loopback service you have not approved are refused and named in the tool result. Non-interactive runs (`-p`, `/spawn`) are refused instead. A Chrome attached through `browserCdpEndpoint` has no proxy; OxideClaw re-checks its page's address before reading it. A page on a link-local or metadata address is blanked; a loopback or LAN page in your own tab is left alone and only not read.

Snapshot and `browser_get_text` results, JavaScript dialog messages and `browser_console` output put the page's text inside a fence labelled as untrusted page data, so instructions printed on a page are presented to the model as content, not commands. The approval gate's visible-price check reads the page at the moment it decides, never text the model supplies: prices in element names always count, prices in the page's text only when the action activates a control that commits something (Continue, Pay, Subscribe).

## Sandboxing

OxideClaw supports multiple sandbox backends to limit tool execution:

```
bwrap      — bubblewrap, lightweight Linux sandboxing
firejail   — security sandbox with predefined profiles
strict     — best-effort command denylist only; no filesystem or network isolation
```
