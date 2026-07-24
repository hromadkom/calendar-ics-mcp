<!--
  Thanks for contributing! Please read CONTRIBUTING.md if you haven't.
  Keep the PR focused — one concern per pull request.
-->

## What and why

<!-- What does this change, and what problem does it solve? Link the issue: Fixes #123 -->

## Checks

<!-- Nothing runs checks on your PR; the hermetic gate runs on your machine. -->

- [ ] `docker build --target test .` passes (fmt + clippy + full test suite)
- [ ] `docker compose run --rm -e TZ=Pacific/Kiritimati dev cargo test` passes
      (required for anything touching dates, recurrence, or formatting)

## Conventions

- [ ] No new `println!`/`print!`/`eprintln!` — diagnostics go through `src/logger.rs`
- [ ] No new path by which `ICS_URL` or `HTTP_BEARER_TOKEN` could reach logs,
      tool output, error messages, or a command line
- [ ] No `chrono::Local` and no host-timezone dependency
- [ ] New behavior is covered by a test; new fixture events use anonymized data only
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` if user-visible
- [ ] `README.md` / `AGENTS.md` updated if a documented behavior changed
- [ ] Version not bumped (releases are cut by publishing a GitHub Release)

## Notes for the reviewer

<!-- Anything non-obvious: tradeoffs, things you were unsure about, follow-ups. -->
