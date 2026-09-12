# `/login`: native Anthropic OAuth and a provider keystore

Date: 2026-09-11
Status: approved design, awaiting implementation plan

## Problem

OxideClaw resolves Anthropic credentials the same way the official SDKs do,
including the OAuth profile written by `ant auth login`. But it obtains that
profile by shelling out to the `ant` binary. On a machine without `ant`, the
only path is an API key in the environment, and the model's own guidance sends
users to a CLI they do not have. For the nine OpenAI-compatible providers the
only mechanism is an environment variable, with no in-app way to add a key, no
indication of which variable is needed, and no feedback about whether a key is
present or where it came from.

## Goals

- Sign in to Anthropic from inside the TUI with no external CLI, storing a
  profile that the `ant` CLI, the official SDKs, and Claude Code all read.
- Keep OAuth tokens fresh across a long session without user action.
- Add, validate, and remove provider API keys from inside the TUI, with the
  key never appearing in the chat log or shell history.
- One status board that shows every provider, whether it is configured, and
  where the credential came from.

## Non-goals

- Claude Pro or Max subscription login. Consumer OAuth is licensed to Claude
  Code and Anthropic's own apps only. Third-party use is against the consumer
  terms and has led to account bans. Not now, not later.
- Reading any other tool's configuration or credential files, including Claude
  Code's. Never.
- OpenRouter's PKCE flow. A legitimate follow-up that fits this architecture.
- A `!` shell prefix in the TUI. Separate feature.
- Billing changes. Console OAuth is billed as API usage to the org the token
  is bound to. This design removes key management, not cost.

## Section 1: native `/login` for Anthropic

### Flow

1. Generate a PKCE verifier (32 random bytes, base64url, no padding), its S256
   challenge, and a random `state` (32 random bytes, base64url).
2. Bind a TCP listener on `127.0.0.1:0`. The redirect URI is
   `http://localhost:<port>/callback`.
3. Build the authorize URL and open it with the existing browser-open action
   (`xdg-open`, `open`, `cmd /C start`). If none succeed, print the URL as a
   system message and keep listening.
4. Wait up to five minutes for a single HTTP request on the listener. Parse
   the query string. `error` present: fail with `error_description`. `state`
   mismatch: fail, and the callback page says not to retry in that browser
   session. `code` missing: fail. Otherwise reply with a small HTML page
   ("Signed in. You can close this tab.") and close the listener.
5. Exchange the code at the token endpoint.
6. Persist the profile (below), then emit a credential-changed event so the
   running session uses the new token immediately.

The whole flow runs in a spawned task, the same pattern the RAG indexer uses.
Progress and outcome arrive as system messages. The TUI never blocks.

### Parameters

These mirror the open-source `ant` CLI (`pkg/cmd/cmd_auth.go`) so the resulting
profile is interchangeable.

| Item | Value |
|------|-------|
| Client ID | `41077d10-94b8-4194-be48-d251e9eb21b4`, overridable with `ANTHROPIC_OAUTH_CLIENT_ID` |
| Console URL | `https://platform.claude.com` |
| Authorize | `<console>/oauth/authorize` |
| Token | `https://api.anthropic.com/v1/oauth/token` |
| Scope | `user:profile user:inference user:developer` |
| Beta header | `anthropic-beta: oauth-2025-04-20` on the token exchange, the refresh, and every authenticated request |

Authorize query parameters: `client_id`, `redirect_uri`, `response_type=code`,
`scope`, `state`, `code_challenge`, `code_challenge_method=S256`. When the
profile already has an `organization_id`, add `orgUUID` so the Console skips
the org switcher. When a workspace is requested, add `workspace_id`.

Token exchange: `POST` with `application/x-www-form-urlencoded` body
`grant_type=authorization_code`, `code`, `code_verifier`, `client_id`,
`redirect_uri`, `state`. The response carries `access_token`, `refresh_token`,
`expires_in`, `scope`, plus `organization {uuid, name}`, `account {uuid,
email_address}`, and `workspace {id, name}` when bound.

Refresh: `POST` with a JSON body `{"grant_type": "refresh_token",
"refresh_token": ..., "client_id": ...}` and the beta header. Same response
shape.

**Risk.** The default client ID is registered to the `ant` CLI. It is a public
PKCE client published in an Anthropic tool, and the profile we write is the one
`ant` itself refreshes, but Anthropic could restrict it. The env override makes
a future OxideClaw-specific ID a one-line change.

### Manual flow

`/login anthropic manual` is for SSH sessions, containers, and any host without
a usable localhost. The redirect URI is the Console's code-display page,
`<console>/oauth/code/callback?app=anthropic-cli`. The authorize URL is printed.
The user opens it anywhere, signs in, and pastes the displayed code into the
existing ask-user dialog. The exchange then proceeds exactly as above, with the
manual redirect URI in the token request.

### Storage

Root: `$ANTHROPIC_CONFIG_DIR`, else `~/.config/anthropic` on Unix and
`%APPDATA%\Anthropic` on Windows. Existing helper: `anthropic_config_dir()`.

```
<root>/
  active_config                 "<profile>\n"           0644 in 0755
  configs/<profile>.json                                0600 in 0700
  credentials/<profile>.json                            0600 in 0700
```

`configs/<profile>.json`:

```json
{
  "version": "1.0",
  "authentication": { "type": "user_oauth", "client_id": "…", "scope": "…" },
  "organization_id": "…",
  "workspace_id": "…"
}
```

`scope` is written only when the user overrode it. `workspace_id` is omitted
when empty. `console_url` and `base_url` are written only when non-default.

`credentials/<profile>.json`:

```json
{
  "version": "1.0",
  "type": "oauth_token",
  "access_token": "…",
  "expires_at": 1757600000,
  "refresh_token": "…",
  "scope": "…",
  "organization_uuid": "…",
  "organization_name": "…",
  "account_email": "…",
  "workspace_id": "…",
  "workspace_name": "…"
}
```

`expires_at` is Unix seconds. Empty optional fields are omitted. Unknown fields
on read are ignored. A `type` that is present but not `oauth_token` is an error.

All writes are atomic: temp file in the target directory, chmod, rename.

Profile selection: `ANTHROPIC_PROFILE` if set, else the content of
`active_config`, else `default`. `/login anthropic <name>` writes profile
`<name>` and sets it active. A plain `/login anthropic` sets `active_config`
only when no profile is active yet, matching `ant`.

### Reading and refresh without `ant`

The shell-out to `ant auth print-credentials` is removed, along with the
"credentials directory exists" gate that protected against the Apache Ant name
collision. OxideClaw reads the profile itself.

Because tokens are short-lived, refresh must work mid-session. The Anthropic
client gains a credential provider consulted per request:

- `Static(key)` returns immediately (API key or `ANTHROPIC_AUTH_TOKEN`).
- `Profile(state)` holds the loaded credentials behind a mutex. On each
  request: if fewer than 120 seconds remain, refresh, rewrite the credentials
  file, and update the in-memory copy, all under the lock so concurrent
  requests share one refresh. Then hand back the current access token.

A 401 on a profile-backed request forces one refresh and one retry. If the
refresh fails, or there is no refresh token, the error says to run `/login`.

Precedence is unchanged: `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` →
OxideClaw's own explicit mechanisms (key file descriptor, `apiKeyHelper`) →
profile. The existing "stale env var shadows your profile" warning stays.

### `/logout`

Removes `configs/<profile>.json` and `credentials/<profile>.json` for the
active profile and clears `active_config` if it pointed there. Emits a
credential-changed event so the session falls back to whatever the environment
provides, or to "no credential".

### Testing

Pure units: PKCE challenge against the RFC 7636 test vector; authorize URL
construction with and without org and workspace; callback parsing for success,
`error`, state mismatch, missing code; credentials and config round-trip
against fixtures copied from the Go SDK wire shape; refresh-threshold decision;
active-profile resolution order.

Integration: a local mock token endpoint on `127.0.0.1` exercising exchange,
refresh, the 120-second threshold, and the 401-then-refresh-then-retry path.

No new dependencies. The callback listener is a single-request HTTP/1.1 read
on a tokio socket. `sha2`, `base64`, `rand`, `url`, `reqwest` are present.

## Section 2: provider keystore and masked key entry

### Where keys live

The user-level `~/.config/oxideclaw/.env` (via `app_dir`), which startup already
loads. Created 0600 inside a 0700 directory, rewritten atomically. A save adds
or replaces exactly the `<KEY_ENV>=<value>` line and preserves every other
line and comment. Nothing is ever written to the project tree.

Only variables in the existing `SAFE_ENV_KEYS` allowlist are stored, which
covers every provider `key_env`. `OPENAI_BASE_URL` and `LM_STUDIO_HOST` are
deliberately excluded from .env loading because a file must not be able to
redirect API traffic; the keystore honours that exclusion.

### In-memory keystore

A `Keystore` is built once at startup from the process environment (which
already includes the loaded .env files) and stored on `Config`. It records,
per variable, the value and its source: shell environment, project `.env`,
`~/.env`, or the user-level file. The configured-provider check, the
OpenAI-compatible client constructor, the model picker, and `/login` all read
the keystore. Nothing mutates the process environment at runtime.

`/login <provider>` updates the file and the keystore in the same step, so the
key is usable immediately with no restart.

### Precedence and the shadow warning

Shell environment beats the stored file on every launch, as today. If a key is
saved while a different value for the same variable is exported in the shell,
the save succeeds and a warning states that the exported value will win next
launch. The board shows the source per provider so the situation is visible.

### Masked entry

`PendingUserQuestion` gains `secret: bool`. When set, the dialog renders one
bullet per character, supports paste, Enter submits, Esc cancels. The value
goes only to the keystore. All feedback uses the existing redaction helper
(first eight characters, then an ellipsis).

### Validation

Before saving, `GET <base_url>/models` with `Authorization: Bearer <key>` and a
ten-second timeout.

| Result | Action |
|--------|--------|
| 200 | Save. Picker gains the provider immediately. |
| 401 or 403 | Reject. Do not save. Reopen the dialog with the error. |
| Anything else (network error, 404, 5xx) | Save, with a note that the key could not be verified. |

### Get-a-key URLs

`ProviderDef` gains `key_url: &'static str`:

| Provider | URL |
|----------|-----|
| Groq | https://console.groq.com/keys |
| OpenRouter | https://openrouter.ai/keys |
| DeepSeek | https://platform.deepseek.com/api_keys |
| Together | https://api.together.ai/settings/api-keys |
| Mistral | https://console.mistral.ai/api-keys |
| Venice | https://venice.ai/settings/api |
| OpenAI | https://platform.openai.com/api-keys |
| LM Studio | empty |
| Generic | empty |

The board row shows it. `/login <provider> open` opens it with the existing
browser-open action before showing the dialog.

### URL-based providers

`/login lmstudio` stores nothing and prints the `LM_STUDIO_HOST` export to run
in the shell. `/login openai-compat` stores `OPENAI_API_KEY` and prints the
`OPENAI_BASE_URL` export. Validation for the generic provider runs only when a
base URL is present.

### `/logout <provider>`

Removes the line from the file and the entry from the keystore. Warns if the
shell still exports the variable, since the provider will remain configured.

### Testing

.env line replace and insert with comments preserved; file and directory
permissions; keystore source attribution; the shadow warning; status-code
mapping against a local mock server; masked rendering; the URL-based provider
paths store nothing.

## Section 3: command surface, board, and integration

### Commands

```
/login                        status board (interactive overlay)
/login anthropic [profile]    Console OAuth, optional named profile
/login anthropic manual       paste-the-code flow for SSH or headless
/login <provider>             masked key entry, e.g. /login groq
/login <provider> open        open the key page first, then the dialog
/logout                       remove the active Anthropic profile
/logout <provider>            remove the stored key
```

A bare word after `/login` must be a provider prefix or `anthropic`. Anything
else is an error listing the valid words, so a profile name is never mistaken
for a provider.

### The board

Reuses the interactive overlay used by the model picker (arrows, Enter,
number quick-pick, Esc). Rows, in order: Anthropic, the nine providers in
registry order, Ollama. One line per row: name, status, source. Example:

```
1. Anthropic      signed in as a…@kubereva.com · org Kubereva · expires in 41 min
2. Groq           key via shell env
3. OpenRouter     key via ~/.config/oxideclaw/.env
4. DeepSeek       not configured · keys at platform.deepseek.com/api_keys
5. LM Studio      needs LM_STUDIO_HOST in your shell
…
11. Ollama        reachable at http://localhost:11434 · 3 models
```

Enter on a row starts that row's flow. The footer hint reads
"↑↓ select · Enter login · 1-9 quick · Esc close" via the per-overlay hint
table.

### Wiring

New `AppEvent::CredentialChanged { kind }` where `kind` is `Anthropic` or
`Provider(prefix)`. On receipt the run loop:

- For `Anthropic`: re-resolves the credential, updates `config.api_key`,
  `auth_is_oauth`, `auth_source`, and swaps the client's credential provider.
- For `Provider`: nothing beyond the keystore update already done; the picker
  and client read it live.
- Rebuilds `ApiBackend` only if the current model is served by the changed
  credential.

Sub-agents receive the current Anthropic token the same way they receive the
key today, read at spawn time from the provider.

### Doctor, help, system prompt

`/doctor` shows profile name, org, email, and time to expiry for a profile
credential, and one line per configured provider with its source. `/help`
lists the login entries. The system prompt's auth note changes from "auth is
env-driven, no login state" to "use `/login`", so the model directs users to
the in-app flow rather than an external CLI.

### Docs

README quickstart gains a `/login` line. FEATURES.md gains an Authentication
section: the profile files and their compatibility with `ant`, the SDKs, and
Claude Code; precedence; the keystore file; the manual flow; the shadow
warning.

### Module layout

```
src/auth/mod.rs        Credential, CredentialSource, resolution order (moved from auth.rs)
src/auth/profile.rs    config dir, active profile, config/credentials files, atomic writes
src/auth/oauth.rs      PKCE, authorize URL, loopback callback, exchange, refresh, provider
src/auth/keystore.rs   Keystore, .env line editing, validation call
src/commands/login.rs  /login and /logout parsing, board construction
```

### Delivery

Four mergeable increments, each with its tests:

1. Native profile reading and refresh, replacing the `ant` shell-out.
   Behaviour-neutral for anyone already logged in via `ant`.
2. OAuth login and `/logout` for Anthropic, including the manual flow and the
   credential-changed wiring.
3. Keystore, masked entry, validation, provider login and logout.
4. The board, doctor, help, system prompt, and docs.

## Security notes

- PKCE S256 and a random state on every authorize request.
- Callback listener bound to loopback only, accepts one request, five-minute
  timeout, then closes.
- Tokens and keys are never logged, never placed in chat entries, never
  echoed. Display uses the redaction helper only.
- Credential files 0600 in 0700 directories, atomic writes.
- No file under a project directory is ever written.
- Base URLs are never read from any .env file.
