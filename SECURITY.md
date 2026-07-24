# Security Policy

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately through GitHub's
[private vulnerability reporting](https://github.com/hromadkom/calendar-ics-mcp/security/advisories/new)
(Security → Report a vulnerability). If that is unavailable to you, email
`hromadko.m@gmail.com` with `calendar-ics-mcp` in the subject.

Please include the version or image tag, the configuration involved (stdio or
HTTP mode), and reproduction steps. **Redact your feed URL** — see below.

This is a personal side project, not a commercial product: expect an
acknowledgement within about a week, and a fix timeline that depends on
severity. There is no bug bounty.

## Supported versions

Only the latest release receives fixes. Older tags are not patched.

## Your feed URL is a credential

The single most important thing to understand about this server: a published
Outlook/Exchange ICS URL is an **unauthenticated bearer credential**. Anyone
who obtains that URL can read the entire calendar — there is no OAuth, no
token expiry, and no per-request authorization. Microsoft's only revocation
mechanism is unpublishing the calendar, which invalidates the URL for
everyone.

Consequences:

- **Never paste your real `ICS_URL` into an issue, pull request, log excerpt,
  screenshot, or stack trace.** If you already have, unpublish and republish
  the calendar in Outlook to rotate the URL.
- Keep it in `.env` (already gitignored) or your MCP client's `env` block —
  never in a Dockerfile, a compose `command:`, a shell history, or a git
  commit.

## How the server protects it

These are properties the code maintains deliberately; a regression in any of
them is a security bug worth reporting:

- The URL is read from the environment in exactly one place (`src/config.rs`)
  and is never written to stdout, stderr, tool results, or error messages.
  Fetch failures are reduced to an error enum before any formatting, so a
  transport error can never `Display` the URL. Config errors never echo the
  offending value.
- Tool-facing text passes through `sanitize_text`. The test suite asserts that
  the fixture's `SECRETPATH123` marker never appears in any output.
- It is never baked into the image and never placed on a command line — the
  documented invocation uses a value-less `-e ICS_URL`, so it does not appear
  in `docker ps` or the container's argv.
- `HTTP_BEARER_TOKEN` is handled the same way.

## Security model

- **Read-only, single destination.** The only network request the server ever
  makes is an HTTPS `GET` on the one operator-supplied `ICS_URL`. It writes
  nothing to disk, calls no Microsoft 365 API, uses no OAuth, and needs no
  tenant permissions or app registration.
- **No persistence, no telemetry.** Calendar data lives only in an in-memory
  cache (`CACHE_TTL_SECONDS`, default 5 minutes) inside the container.
  Responses go only to the connected MCP client. Nothing is reported anywhere.
- **Hardened container.** The image is `FROM scratch` with a single static
  binary and runs as `USER 65534:65534`. It exposes no ports in stdio mode.
  The documented invocation adds `--read-only`, `--cap-drop=ALL`, and
  `--security-opt=no-new-privileges`; the server needs no writable filesystem
  and no capabilities, so these cost nothing.
- **Fails safe.** A feed that does not end in `END:VCALENDAR` is treated as
  truncated: event tools return an error rather than silently serving partial
  data, and a truncated snapshot is never cached as fresh. Expired cache
  entries are never served stale.

## HTTP mode

Setting `HTTP_BIND` turns on the MCP Streamable HTTP transport. Note its
intended threat model:

- It is designed for a **trusted network** — a compose network, a loopback
  bind, or a VM-local port. Do not expose it directly to the internet.
- `HTTP_BEARER_TOKEN` is optional and unset by default. **With it unset,
  anyone who can reach the port can read the calendar.** Set it for anything
  beyond loopback, and put a TLS-terminating reverse proxy in front if the
  port is reachable off-host — the server speaks plain HTTP and a bearer token
  over plain HTTP is interceptable.
- `GET /healthz` is intentionally unauthenticated so a container healthcheck
  can reach it. It returns only `{"status":"ok","version":…}` and never
  touches the feed.
- A browser DNS-rebinding guard rejects a request whose `Origin` header is
  present but matches neither a localhost variant nor the request `Host`.
  Non-browser clients send no `Origin` and are unaffected.
- Request bodies are capped at 1 MiB.
