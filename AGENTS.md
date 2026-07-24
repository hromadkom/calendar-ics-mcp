# AGENTS.md

Guidance for AI coding agents working in this repository. If your agent reads
`CLAUDE.md` or `.cursorrules`, treat this file as the source of truth.

## Project

`calendar-ics-mcp` — read-only MCP server (**Rust**, musl-static, `FROM
scratch` image) that downloads an Outlook/Exchange published ICS feed in full,
expands recurring events, and exposes `get_events`, `get_events_range`, and
`feed_info`. Two transports from one binary:

- **stdio** (default) — Claude Desktop, via `docker run -i --rm --init
  --read-only --cap-drop=ALL --security-opt=no-new-privileges -e ICS_URL
  calendar-ics-mcp`.
- **MCP Streamable HTTP** (`HTTP_BIND` set, e.g. `0.0.0.0:8590`) — for a
  long-running agent; compose service `calendar-ics-http` with a
  `/healthz`-based healthcheck (`/calendar-ics-mcp --health` is the probe —
  the scratch image has no shell). Optional `HTTP_BEARER_TOKEN`.

Public repository (MIT, `github.com/hromadkom/calendar-ics-mcp`). See
`CONTRIBUTING.md` for the contributor-facing version of this file and
`SECURITY.md` for the threat model.

This is a rewrite of an earlier TypeScript/Node implementation, which is not
part of this repository's history. The fixture/acceptance suite carried over
verbatim and a differential sweep pinned output parity. Motivation: the Node
HTTP variant leaked to ~7 GB RSS in half a day; the Rust binary idles in
single-digit MB. Where behavior is described below as "node-ical parity", that
refers to the npm packages the TS version was built on (`node-ical`,
`rrule.js`), verified empirically at the time.

## Common commands (all dockerized — no host Rust toolchain assumed)

```bash
docker compose run --rm dev cargo test                          # full suite
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test # host-TZ independence proof
docker compose run --rm dev cargo clippy --all-targets -- -D warnings
docker compose run --rm dev cargo fmt --check                   # (or `cargo fmt` to fix)
docker compose run --rm -T dev cargo run                        # stdio dev session (.env)
docker compose run --rm -p 8590:8590 -e HTTP_BIND=0.0.0.0:8590 dev cargo run  # HTTP dev session

docker build --target test .                # hermetic gate (fmt+clippy+test) — nothing runs it on PRs
docker compose build                        # local release-image build
docker compose run --rm -T calendar-ics     # stdio session against the image
                                            # (-T required when piping; never `compose up` this one)
docker compose up -d calendar-ics-http      # HTTP daemon; healthcheck via --health
npx @modelcontextprotocol/inspector docker compose run --rm -T calendar-ics   # inspector (stdio)
```

The `dev` service caches the cargo registry and build tree in named volumes;
`/target` never exists on the host. The suite must pass under any host TZ —
nothing in the pipeline consults the host timezone (`chrono::Local` is banned
via clippy.toml, as are `println!`/`print!`).

## Releasing

Publishing is GitHub-Actions-only; there are no release scripts in the repo.
Bump `version` in Cargo.toml, refresh Cargo.lock (`docker compose run --rm dev
cargo check`), move `[Unreleased]` under the new version in `CHANGELOG.md`
(Keep a Changelog format), commit, then create a GitHub Release tagged
`vX.Y.Z`. `.github/workflows/release.yml` refuses to publish if the tag and
Cargo.toml disagree or the hermetic gate fails, then pushes
`docker.io/hromadkom/calendar-ics-mcp:{latest,<sha>,<version>}` for
linux/amd64+arm64. No QEMU: the builder stage is pinned to `$BUILDPLATFORM`
and cargo-zigbuild cross-compiles both musl targets natively — keep it that
way, emulation is roughly an order of magnitude slower. Repo variables
`IMAGE_NAME`/`REGISTRY` and secrets `DOCKERHUB_USERNAME`/`DOCKERHUB_TOKEN`
let a fork publish elsewhere. A pre-release publishes version tags but does
not move `:latest`.

## Architecture

One-directional dependencies:
`main → mcp/http/server → feed / ics::* ← config, errors, logger`.

- `src/mcp.rs` — hand-rolled JSON-RPC 2.0 / MCP protocol layer (initialize
  with version negotiation, ping, tools/list, tools/call, batches). The pure
  `handle_message` is shared by both transports and is the test entry point.
  Deliberately no SDK and no tokio — minimal RSS was the point.
- `src/http.rs` — stateless Streamable HTTP (`POST /mcp`; no sessions, no
  SSE; `GET /healthz` liveness that never touches the feed). Two worker
  threads over a mutex so `/healthz` stays responsive during a slow fetch.
- `src/ics/expand.rs` is the correctness core (RRULE/EXDATE/override merge).
  Read its doc comments before touching it; every acceptance criterion has a
  named test module in `tests/expand_test.rs`.
- `src/ics/rrule_slots.rs` wraps the `rrule` crate as a bare slot generator:
  EXDATE/RDATE are NEVER handed to the crate (expand.rs owns those, exactly
  like the TS layer did over rrule.js). A Z-less `UNTIL` is resolved in the
  event's DTSTART timezone and rewritten as UTC before the crate sees it
  (node-ical parity, verified empirically — the crate alone would use the
  machine tz).
- `src/ics/tzids.rs` rewrites Windows/unknown TZIDs to IANA **before** parsing
  (`windows-timezones` crate = the same CLDR 001 mapping as npm
  `windows-iana`), so results never depend on the host timezone.
- `src/ics/parse.rs` — hand-rolled content-line parser producing UID-grouped
  events; VTIMEZONE/VALARM are skipped structurally (post-rewrite every TZID
  is IANA). All-day values stay `NaiveDate` end to end. `DURATION` synthesizes
  the end when DTEND is absent (DTEND wins over both); duplicate masters and
  same-slot overrides are resolved by `SEQUENCE` (higher wins, ties → later
  component) — both node-ical-parity behaviors verified empirically.
- `src/feed.rs` owns fetching (ureq/rustls; the only ureq call site) + the
  single-slot TTL cache + the `END:VCALENDAR` integrity check. Fail-hard by
  design: expired cache is never served stale; a truncated snapshot is
  reportable by `feed_info` but never counts as fresh. Single-flight is
  structural: tool calls are serialized (stdio: one thread; HTTP: mutex).

## Conventions and gotchas

- **stdout is JSON-RPC only.** `src/logger.rs` (stderr, best-effort writes —
  never `eprintln!`, it panics on a closed pipe and killed an HTTP worker
  once) is the only stderr writer in the serving paths; clippy.toml bans
  `println!`/`print!`. The single exception is the `--health` probe in
  `main.rs`, which runs before the logger exists and exits immediately.
- **`ICS_URL` is a secret.** It must never reach logs, error messages, tool
  output, the image, or command lines. Fetch errors are reduced to an enum
  (`HttpErr`) before any formatting — never `Display` a ureq error — and
  `sanitize_text` scrubs every tool-facing message. Config errors never echo
  values. Tests assert the fixture's `SECRETPATH123` never leaks.
  `HTTP_BEARER_TOKEN` gets the same treatment.
- Domain errors return `{ isError: true }` tool results, never JSON-RPC
  errors and never panics across the boundary. `panic = "abort"` in release:
  handlers are `Result`-only, `clippy::unwrap_used` warns in src.
- Dedup key in expansion is `uid + original slot instant` (RECURRENCE-ID) —
  never the moved start. Override wins over EXDATE; an override consumes its
  base slot even when moved out of the window.
- Day windows are built via 'compatible' local-midnight resolution — 23 h/25 h
  on DST days; there is no `±86_400_000` day arithmetic anywhere.
- The `rrule` crate is pinned minor (`0.14`); bump deliberately and only with
  the fixture suite green (UNTIL inclusivity, WKST=MO default, and
  wall-clock-preserving DST instants are all load-bearing behaviors).
- Deviations from the TS implementation (documented, accepted): summary sort
  tiebreak is codepoint order (not `localeCompare`); IO error details are
  Rust `ErrorKind` names (`ConnectionRefused`, not `ECONNREFUSED`); the
  description cap counts UTF-16 units but never splits a scalar; floating
  datetimes resolve as UTC unconditionally (host-independent); multiple
  orphan overrides per UID each emit (node-ical kept only the first);
  override identity is the exact original slot instant — node-ical dedups
  same-UID overrides through a calendar-day key and silently drops a second
  override for a DIFFERENT slot on the same day (we keep both, deliberately);
  a master arriving after orphan overrides always installs (node-ical could
  drop it via a sequence comparison against the orphan parent object).

## Tests

`tests/fixtures/feed.ics` is an anonymized Exchange-style feed (CRLF, folded
lines, Windows TZIDs, `X-MICROSOFT-*` props) anchored around March 2026 (DST
change 2026-03-29) — carried over from the TS suite and byte-identical except
for two identifiers genericized before open-sourcing (the Zoom host in
`zoom-loc@fixture` and the summary of `teams-xprop@fixture`); treat it as
frozen. Never put real calendar data in it. Every VEVENT proves a specific
acceptance criterion: each has a
named module in `tests/expand_test.rs`, and `tests/acceptance_test.rs` pins
exact expected event lists for four hand-verified days. `feed-truncated.ics`
is the same file cut mid-VEVENT for the integrity check.

`tests/stdio_smoke.rs` and `tests/http_smoke.rs` spawn the real binary
(loopback fixture HTTP server) and assert framing, stdout hygiene, auth,
Origin guard, `--health`, and the exit-0-on-EOF contract. Unit tests live
in-module (`#[cfg(test)]`); `tests/common/mod.rs` is the shared helper port
of the old `tests/helpers.ts`.

Manual acceptance (after changes to expansion logic): run the image with the
real `ICS_URL` exported via `read -rs ICS_URL && export ICS_URL`, call
`feed_info`, then diff `get_events` for 2–3 known days against Outlook web.
