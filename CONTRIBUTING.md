# Contributing

Thanks for your interest in `calendar-ics-mcp`. It's a small, deliberately
narrow tool — a read-only MCP server over a published ICS feed — so the bar for
new surface area is high, but bug reports, correctness fixes, and documentation
improvements are very welcome.

## Before you start

- **Bugs and correctness issues**: open an issue with the smallest ICS snippet
  that reproduces it (anonymized — see [SECURITY.md](SECURITY.md) before
  pasting anything from a real calendar).
- **New features**: open an issue first. This server intentionally exposes only
  three tools and performs exactly one kind of network call (an HTTPS `GET` on
  the configured feed URL). Changes that widen that are unlikely to be merged.

## Development setup

The only prerequisite is Docker with Compose. **No host Rust toolchain is
needed or assumed** — every cargo invocation runs inside the `dev` compose
service, which caches the registry and build tree in named volumes.

```bash
git clone https://github.com/hromadkom/calendar-ics-mcp.git
cd calendar-ics-mcp
cp .env.example .env          # fill in ICS_URL for manual runs; tests don't need it

docker compose run --rm dev cargo test
```

Do not run `cargo` directly on the host. Build artifacts live only in the
`cargo-target` volume, never in the working tree.

## The checks your change must pass

The gate is a single hermetic Docker build that runs formatting, lints, and the
full test suite in one pass. `.github/workflows/ci.yml` runs it on every pull
request, and the release workflow runs it again before publishing — but run it
yourself first. A round trip through CI to learn that a file needs
reformatting is a slow way to find out.

```bash
docker build --target test .
```

That is exactly equivalent to:

```bash
docker compose run --rm dev cargo fmt --check
docker compose run --rm dev cargo clippy --all-targets --locked -- -D warnings
docker compose run --rm dev cargo test --locked
```

Clippy warnings are errors. `clippy.toml` additionally bans `println!`/`print!`
(stdout is reserved for JSON-RPC framing) and `chrono::Local` (the host
timezone must never influence results).

One more check matters for anything touching dates, recurrence, or formatting —
the suite must pass under **any** host timezone:

```bash
docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test
```

## Project conventions

These are load-bearing. [AGENTS.md](AGENTS.md) documents the architecture and
the reasoning behind each in more depth; read it before a non-trivial change.

- **stdout is JSON-RPC only.** All diagnostics go to stderr via `src/logger.rs`.
  Never `eprintln!` either — it panics on a closed pipe.
- **`ICS_URL` is a credential.** It must never reach logs, error messages, tool
  output, the image, or a command line. Fetch errors are reduced to an enum
  before formatting, and `sanitize_text` scrubs every tool-facing message.
  `HTTP_BEARER_TOKEN` gets the same treatment. Tests assert the fixture's
  `SECRETPATH123` never leaks — keep them passing.
- **No panics across the tool boundary.** Handlers are `Result`-only and domain
  errors return `{ isError: true }` tool results, never JSON-RPC errors. The
  release profile is `panic = "abort"`.
- **No host timezone, ever.** Day windows use 'compatible' local-midnight
  resolution (so DST days are 23 h or 25 h); there is no `±86_400_000` day
  arithmetic anywhere.
- `src/ics/expand.rs` is the correctness core. Read its doc comments first —
  every acceptance criterion has a named test module in
  `tests/expand_test.rs`.

## Tests

`tests/fixtures/feed.ics` is an anonymized Exchange-style feed anchored around
March 2026 (covering the 2026-03-29 DST change). Treat it as frozen: every
VEVENT in it proves a specific acceptance criterion and has a corresponding
named module in `tests/expand_test.rs`. If you add a behavior, add a VEVENT
*and* its test module rather than editing an existing event.

Never add real calendar data to a fixture. UIDs use the `@fixture` suffix and
example domains only.

## Pull requests

- Keep commits focused; the history uses
  [Conventional Commits](https://www.conventionalcommits.org/) (`fix:`,
  `feat:`, `docs:`, `build:`, `chore:`).
- Update `CHANGELOG.md` under an `## [Unreleased]` heading for anything
  user-visible ([Keep a Changelog](https://keepachangelog.com/) format).
- If you change a documented behavior, update `README.md` and `AGENTS.md` in
  the same PR.
- Don't bump the version yourself — releases are cut by publishing a GitHub
  Release, which triggers `.github/workflows/release.yml`.

## License

By contributing, you agree that your contributions will be licensed under the
[MIT License](LICENSE) that covers this project.
