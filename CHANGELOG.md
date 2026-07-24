# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Open-sourced under the MIT license. Added `CONTRIBUTING.md`, `SECURITY.md`,
  issue and pull-request templates, and Dependabot configuration.
- The reference `calendar-ics-http` compose service now publishes port 8590 on
  loopback only (`127.0.0.1:8590:8590`) instead of every host interface —
  `HTTP_BEARER_TOKEN` is unset by default, so the previous binding exposed an
  unauthenticated read of the calendar to the local network.
- The stdio `calendar-ics` compose service no longer carries a `restart`
  policy (a stray `docker compose up` turned it into a crash loop), and `.env`
  is now optional for every service, so a fresh clone can run the test suite
  before writing one.
- Releases moved from the local `bin/release` and `bin/docker-build` scripts
  (both removed) to `.github/workflows/release.yml`, triggered by publishing a
  GitHub Release. The workflow refuses to publish when the tag and
  `Cargo.toml` version disagree or the hermetic gate fails, pushes multi-arch
  `linux/amd64` + `linux/arm64` images with build provenance, and leaves
  `:latest` alone for pre-releases. `IMAGE_NAME` and `REGISTRY` are repository
  variables so forks can publish to their own namespace.
- Two identifiers in `tests/fixtures/feed.ics` were genericized for the public
  release (the Zoom host in `zoom-loc@fixture` and the summary of
  `teams-xprop@fixture`). No behavior changed.

## [0.2.0] - 2026-07-24

Initial release. The server was first implemented in TypeScript/Node and
rewritten in Rust before ever shipping (July 2026); the TS implementation is
not part of this repository. The fixture/acceptance suite carried over
verbatim, and a differential sweep against the TS build pinned byte-identical
`structuredContent` over Jan–Jul 2026. Motivation for the rewrite: memory
footprint — the Node HTTP variant leaked to ~7 GB RSS in half a day; the Rust
binary idles at ~3 MB RSS and the whole image is ~4 MB.

### Added

- Read-only MCP server exposing `get_events`, `get_events_range`, and
  `feed_info` over a published Outlook/Exchange ICS feed. Rust, no async
  runtime: a hand-rolled JSON-RPC 2.0 / MCP protocol layer (initialize with
  version negotiation, ping, tools/list, tools/call, batches) shared by both
  transports.
- Two transports from one ~4 MB static binary:
  - **stdio** (default) for Claude Desktop — exits 0 on stdin EOF and
    SIGINT/SIGTERM;
  - **MCP Streamable HTTP** (`HTTP_BIND`) for a long-running agent — stateless
    `POST /mcp`, `GET /healthz` liveness (never touches the feed), optional
    `HTTP_BEARER_TOKEN`, browser-Origin guard, and a shell-less container
    healthcheck (`calendar-ics-mcp --health`); reference deployment in the
    `calendar-ics-http` compose service.
- Full-feed download (ureq/rustls, embedded roots) with `END:VCALENDAR`
  integrity check (fails loudly on truncation; a truncated snapshot is
  reportable by `feed_info` but never cached) and a single-slot in-memory TTL
  cache — fail-hard, never served stale.
- Recurrence expansion: RRULE via the `rrule` crate as a bare slot generator
  (weekly/biweekly/monthly, UNTIL inclusive — a Z-less UNTIL resolves in the
  event's timezone, node-ical parity), first-party EXDATE (multi-value,
  TZID-qualified, date-only), RECURRENCE-ID overrides (in place, moved across
  days, moved into the query window, override wins over EXDATE), orphan
  overrides, all-day events with exclusive DTEND, `DURATION`-only events
  (end synthesized; DTEND wins), `SEQUENCE`-aware duplicate master/override
  resolution, DST-correct local times (23 h/25 h day windows), host-TZ
  independent by construction.
- Deterministic timezone handling: pre-parse Windows→IANA TZID rewrite
  (`windows-timezones`, the CLDR mapping), unknown TZIDs fall back to
  `TZ_DEFAULT` with a stderr warning.
- Output shaping: `TZ_DEFAULT` offsets, `busy_status` from
  `X-MICROSOFT-CDO-BUSYSTATUS`, Teams/Zoom/Meet `meeting_url` extraction,
  Teams boilerplate stripping (~2000 char cap).
- Secret hygiene: `ICS_URL` masked to hostname in every log/error path; fetch
  errors reduced to an enum before any formatting; config errors never echo
  values.
- Docker: multi-stage `cargo-zigbuild` builder cross-compiles static musl
  binaries for linux/amd64+arm64 natively (no QEMU) into `FROM scratch`;
  hermetic `--target test` CI stage; `bin/docker-build` publishes
  multi-arch OCI-labeled images to `docker.io` (refuses `--push`
  from a dirty tree); `bin/release` bumps `Cargo.toml`/`Cargo.lock`, tags, and
  pushes. Hardened runtime: `USER 65534`, no ports (stdio) — documented
  invocation adds `--read-only`, `--cap-drop=ALL`, `no-new-privileges`.
- Dockerized development workflow (`dev` compose service with cargo cache
  volumes) — no host Rust toolchain assumed; Exchange-style test fixture with
  135 tests including real-subprocess stdio and HTTP smoke tests.

[Unreleased]: https://github.com/hromadkom/calendar-ics-mcp/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/hromadkom/calendar-ics-mcp/releases/tag/v0.2.0
