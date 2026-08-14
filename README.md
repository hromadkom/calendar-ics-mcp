# calendar-ics-mcp

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Docker Image](https://img.shields.io/docker/v/hromadkom/calendar-ics-mcp?label=docker&sort=semver)](https://hub.docker.com/r/hromadkom/calendar-ics-mcp)
[![Image Size](https://img.shields.io/docker/image-size/hromadkom/calendar-ics-mcp/latest)](https://hub.docker.com/r/hromadkom/calendar-ics-mcp)

A read-only [MCP](https://modelcontextprotocol.io) (Model Context Protocol)
server that turns an Outlook/Exchange **published ICS calendar feed** into three
small JSON tools an AI assistant can actually use — "what's on my calendar
today?", answered from the real feed with recurrence, overrides, timezones, and
DST handled correctly.

Written in Rust for a minimal footprint: the whole image is ~4 MB (`FROM
scratch`, musl-static, no async runtime) and the server idles at ~3 MB RSS.

## Why this exists

A published ICS URL is the simplest possible way to give a tool read access to a
calendar: **no OAuth, no tenant permissions, no app registration** — the URL
itself is the only credential. That makes it a good fit where the official
Microsoft 365 connector isn't available or isn't permitted.

But you can't just point a generic web-fetch tool at the feed:

- **Feeds get truncated.** Assistant fetch tools cap response size (Claude's cuts
  off around 66 kB), silently dropping every event past the cut — and a calendar
  feed is mostly boilerplate, so the cut comes fast. This server downloads the
  feed in full and *verifies* it arrived complete.
- **ICS is not a list of events.** A weekly meeting is one `VEVENT` plus an
  `RRULE`; the cancelled instance is an `EXDATE`; the one that moved to Thursday
  is a separate `RECURRENCE-ID` component that must override the slot it came
  from. Getting "today's meetings" right means expanding all of that, in the
  right timezone, across DST boundaries.

This server does that expansion and hands back small, ready-to-use JSON.

## Tools

| Tool | Input | Output |
| --- | --- | --- |
| `get_events` | `{ date?: "YYYY-MM-DD" }` — default: today in `TZ_DEFAULT` | Events overlapping that local day, sorted by start |
| `get_events_range` | `{ from, to }` (inclusive, max 31 days) | Same format over a date range |
| `feed_info` | — | Feed diagnostics — see below |

Every tool returns its payload both as `structuredContent` and as the exact same
JSON serialized into `content[0].text`. Domain failures (feed unreachable, feed
truncated, bad arguments) come back as a normal tool result with
`isError: true`, never as a JSON-RPC error.

### `get_events` / `get_events_range`

```json
{
  "date": "2026-07-14",
  "timezone": "Europe/Prague",
  "events": [
    {
      "summary": "Design review",
      "start": "2026-07-14T14:15:00+02:00",
      "end": "2026-07-14T14:45:00+02:00",
      "all_day": false,
      "location": "Microsoft Teams Meeting",
      "busy_status": "TENTATIVE",
      "is_recurring": true,
      "meeting_url": "https://teams.microsoft.com/l/meetup-join/…",
      "description": "Agenda without the Teams dial-in boilerplate"
    }
  ]
}
```

Timed events use ISO 8601 with the `TZ_DEFAULT` offset; all-day events use
date-only strings with an **exclusive** end date. `get_events_range` returns
the same envelope with `from` and `to` in place of `date`.

- `busy_status` comes from `X-MICROSOFT-CDO-BUSYSTATUS` (`BUSY` / `TENTATIVE` /
  `FREE` / `OOF`; defaults to `BUSY` when absent).
- `meeting_url` is the first Teams/Zoom/Meet link found in the
  `X-MICROSOFT-SKYPETEAMSMEETINGURL` property, the description, or the location.
- `description` is stripped of the Teams boilerplate (everything from the first
  `____…` separator) and capped at ~2000 characters.
- A day query returns every event **overlapping** that local day: a meeting
  running past midnight shows on both days, a multi-day OOF on each of its days.

### `feed_info`

Diagnostics — call this first when something looks wrong.

```json
{
  "feed_bytes": 148213,
  "vevent_count": 312,
  "ends_with_end_vcalendar": true,
  "dtstart_min": "2024-01-08T07:00:00.000Z",
  "dtstart_max": "2027-06-30T15:30:00.000Z",
  "last_fetch_at": "2026-07-14T09:12:03.000Z",
  "cache_age_seconds": 12,
  "source_host": "https://outlook.office365.com/…"
}
```

`source_host` is the feed URL masked to its origin — the secret path is never
included. `ends_with_end_vcalendar: false` means the snapshot arrived truncated;
`feed_info` still answers (that *is* the diagnosis), but the event tools will
refuse rather than serve partial data.

## Getting your ICS URL

In Outlook on the web: **Settings → Calendar → Shared calendars → Publish a
calendar**. Pick the calendar, choose **"Can view all details"** (lesser
permissions strip the fields these tools return), click **Publish**, and copy
the **ICS** link — not the HTML one.

> [!WARNING]
> Treat that URL like a password. It is an unauthenticated bearer credential:
> anyone who has it can read the whole calendar, it does not expire, and the
> only way to revoke it is to unpublish the calendar — which invalidates it for
> everyone. Never commit it, log it, or paste it into an issue. See
> [SECURITY.md](SECURITY.md).

## Quick start

Prerequisite: Docker. Images are published for `linux/amd64` and `linux/arm64`.

```bash
docker pull hromadkom/calendar-ics-mcp:latest
```

Then add it to your MCP client. For Claude Desktop, in
`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "calendar-ics": {
      "command": "docker",
      "args": ["run", "-i", "--rm", "--init", "--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges", "-e", "ICS_URL", "-e", "TZ_DEFAULT", "hromadkom/calendar-ics-mcp:latest"],
      "env": {
        "ICS_URL": "https://outlook.office365.com/owa/calendar/…/reachcalendar.ics",
        "TZ_DEFAULT": "Europe/Prague"
      }
    }
  }
}
```

Notes:

- The value-less `-e ICS_URL` copies the variable from the `docker` CLI
  environment (populated by your client from the `env` block) into the
  container — the URL never appears in the process arguments or in `docker ps`.
- Claude Desktop spawns commands with a minimal `PATH`; if `docker` is not
  found, use an absolute path (e.g. `/usr/local/bin/docker`).
- Prefer pinning a version tag (`:0.3.0`) over `:latest`.

### Without Docker

There are no prebuilt binaries, and the dockerized build produces a **Linux
musl** binary — it will not run on macOS or Windows. For a native binary you
need a host Rust toolchain (see `rust-toolchain.toml` for the pinned version):

```bash
cargo build --release       # target/release/calendar-ics-mcp
```

Point your client's `command` at that path, with the same `env` block. If you
*are* on Linux and want the static musl binary from the container build:

```bash
docker compose run --rm dev cargo build --release
docker compose run --rm dev cp target/release/calendar-ics-mcp ./calendar-ics-mcp
```

(`/app/target` is a named volume, so the binary has to be copied into the
bind-mounted working directory to reach the host. It is gitignored.)

## HTTP mode

The same binary serves **MCP Streamable HTTP** when `HTTP_BIND` is set —
stateless `POST /mcp`, plus `GET /healthz` for the container healthcheck. This
is the mode for a long-running agent rather than a desktop client. There are no
sessions and no SSE stream; `GET /mcp` returns 405.

```bash
cp .env.example .env                       # fill in ICS_URL
docker compose up -d calendar-ics-http     # listens on 127.0.0.1:8590
curl -s http://127.0.0.1:8590/healthz
```

Because the scratch image has no shell, the binary probes itself for the
healthcheck: `/calendar-ics-mcp --health`.

> [!IMPORTANT]
> This mode is meant for a trusted network — a compose network, a loopback
> bind, or a VM-local port. `HTTP_BEARER_TOKEN` is **unset by default**, and
> with it unset anyone who can reach the port can read your calendar. Set it,
> and put a TLS-terminating reverse proxy in front if the port is reachable
> off-host — the server speaks plain HTTP.

## Configuration

All configuration is via environment variables.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `ICS_URL` | yes | — | Published ICS feed URL (`http`/`https`; use `https`). Works as a bearer credential — never log or commit it. |
| `TZ_DEFAULT` | no | `Europe/Prague` | IANA zone for interpreting date queries and formatting output. |
| `CACHE_TTL_SECONDS` | no | `300` | In-memory cache TTL for the downloaded feed. |
| `FETCH_TIMEOUT_MS` | no | `15000` | Feed download timeout. |
| `HTTP_BIND` | no | — (stdio) | `<ip>:<port>` — serve MCP Streamable HTTP instead of stdio, with `GET /healthz`. |
| `HTTP_BEARER_TOKEN` | no | — | Require `Authorization: Bearer …` on `POST /mcp` (treated as a secret). |

The host's own `TZ` is deliberately ignored — results depend only on
`TZ_DEFAULT` and the feed, so the same query gives the same answer on any
machine.

## Architecture

```
src/
├── main.rs           # entrypoint: config, signals, transport select, --health probe
├── lib.rs            # module wiring (the test target's entry point)
├── mcp.rs            # hand-rolled JSON-RPC 2.0 / MCP protocol layer (no SDK, no tokio)
├── http.rs           # Streamable HTTP transport: POST /mcp, GET /healthz
├── server.rs         # tool declarations + handlers (get_events / get_events_range / feed_info)
├── config.rs         # env parsing + validation (single place reading the environment)
├── logger.rs         # stderr-only JSON logger (stdout is reserved for JSON-RPC)
├── errors.rs         # typed errors + URL masking/sanitizing helpers
├── feed.rs           # ureq fetch + single-slot TTL cache + END:VCALENDAR integrity check
└── ics/
    ├── mod.rs        # submodule wiring
    ├── model.rs      # parsed-calendar types
    ├── tzids.rs      # pre-parse Windows→IANA TZID rewrite (deterministic timezones)
    ├── parse.rs      # hand-rolled ICS content-line parser
    ├── rrule_slots.rs# rrule crate as a bare slot generator (EXDATE stays ours)
    ├── expand.rs     # RRULE/EXDATE/override expansion — the correctness core
    └── format.rs     # output shaping: offsets, busy status, meeting URL, description
```

Recurrence slots come from the [rrule] crate; all other ICS handling (parsing,
EXDATE, RECURRENCE-ID overrides, timezones via chrono-tz) is first-party and
pinned by a fixture suite of hand-verified Exchange-style calendar data.

**Scope:** this targets Outlook/Exchange published feeds. `RRULE`, `EXDATE`,
`RECURRENCE-ID` overrides, `DURATION`, `SEQUENCE`, and Windows TZIDs are
handled; `RDATE`, `STATUS:CANCELLED`, `TRANSP`, and `RANGE=THISANDFUTURE` are
not — Exchange does not emit them, but other calendar sources do. Issues and
patches for wider feed support are welcome.

[AGENTS.md](AGENTS.md) has the deeper design notes — invariants, deliberate
deviations, and the reasoning behind the odd-looking parts.

## Development

Everything runs in the dockerized toolchain (`dev` compose service) — **no host
Rust toolchain is needed**, and a fresh clone can run the tests immediately
(they use a fixture, not your real feed, so no `.env` is required):

```bash
git clone https://github.com/hromadkom/calendar-ics-mcp.git
cd calendar-ics-mcp
docker compose run --rm dev cargo test
```

The rest of the loop:

```bash
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test   # host-TZ independence proof
docker compose run --rm dev cargo clippy --all-targets -- -D warnings
docker compose run --rm dev cargo fmt --check
docker build --target test .                                      # hermetic gate: fmt + clippy + test
npx @modelcontextprotocol/inspector docker compose run --rm -T calendar-ics
```

`docker build --target test .` is the gate. Run it yourself before opening a
pull request (see [CONTRIBUTING.md](CONTRIBUTING.md)); CI then runs the same
gate — plus the host-TZ proof and a musl release build — on the pull request,
and the release workflow runs it once more and refuses to publish an image if
it fails.

### Running the image locally

`compose.yaml` holds the same hardened run configuration as the Claude Desktop
block above and reads `.env`, so you don't have to retype the flags:

```bash
cp .env.example .env                        # fill in ICS_URL
docker compose build                        # local image build
docker compose run --rm -T calendar-ics     # stdio JSON-RPC session
```

Two things to know: this is a stdio server, so use `compose run`, not `compose
up` (an `up`'d container would just wait on stdin forever); and keep `-T` when
piping — without it Compose allocates a pseudo-TTY, which merges stderr logs
into the stdout JSON-RPC stream. Example smoke test:

```bash
{ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"feed_info","arguments":{}}}'; \
  sleep 5; } | docker compose run --rm -T calendar-ics
```

(The `sleep` keeps stdin open until the response arrives — the server
intentionally shuts down on stdin EOF, i.e. when the MCP client disconnects.)

To check against your real feed without putting the URL in your shell history:

```bash
read -rs ICS_URL && export ICS_URL   # paste the URL without echoing it
npx @modelcontextprotocol/inspector \
  docker run -i --rm -e ICS_URL hromadkom/calendar-ics-mcp:latest   # call feed_info first
```

`feed_info` should report `ends_with_end_vcalendar: true`, a `feed_bytes`
matching the full size of the feed, and a plausible `vevent_count`. If your
feed is larger than a generic fetch tool's response cap, that gap is the whole
point of this server.

## Releasing

Releases are driven by GitHub Releases — there is nothing to run locally:

1. Bump `version` in `Cargo.toml`, refresh `Cargo.lock`
   (`docker compose run --rm dev cargo check`), and move the `[Unreleased]`
   entries in [CHANGELOG.md](CHANGELOG.md) under the new version. Commit.
2. Create a GitHub Release with tag `vX.Y.Z`.

[`.github/workflows/release.yml`](.github/workflows/release.yml) then verifies
the tag matches `Cargo.toml`, runs the hermetic gate (fmt + clippy + the full
test suite), and pushes multi-arch `linux/amd64` + `linux/arm64` images tagged
`:X.Y.Z`, `:<short-sha>`, and `:latest`. Marking the release as a pre-release
publishes the version tags but leaves `:latest` alone.

Forks publish to their own namespace by setting the repository variables
`IMAGE_NAME` and `REGISTRY`, plus the `DOCKERHUB_USERNAME` / `DOCKERHUB_TOKEN`
secrets. Versions follow [Semantic Versioning](https://semver.org/).

## Security

The short version: **your feed URL is a credential**, and the server is built so
it never escapes.

- **Read-only, single destination.** The only network request the server ever
  makes is a `GET` on the one configured `ICS_URL`. It writes nothing anywhere,
  calls no Microsoft 365 API, uses no OAuth, and needs no tenant permissions.
- **No persistence, no telemetry.** Calendar data lives only in an in-memory
  cache (≤ `CACHE_TTL_SECONDS`) inside the container; responses go only to the
  connected MCP client.
- **The URL never leaks.** It is never logged (errors mask it to the origin),
  never baked into the image, never placed on a command line, and is supplied
  only via the environment at runtime. The test suite asserts it.
- **Hardened container.** `FROM scratch` with one static binary, running as
  `USER 65534:65534`, exposing no ports in stdio mode, with a dependency tree
  pinned by `Cargo.lock` and built `--locked`.
- **Fails safe.** A feed that doesn't end in `END:VCALENDAR` is treated as
  truncated: tools return an error instead of silently serving partial data.

Full details, the HTTP-mode threat model, and how to report a vulnerability:
[SECURITY.md](SECURITY.md).

## License

[MIT](LICENSE) © Martin Hromádko

[rrule]: https://github.com/fmeringdal/rust-rrule
