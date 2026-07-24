# syntax=docker/dockerfile:1
# The builder always runs on the build host's native platform and
# cross-compiles a fully static musl binary per $TARGETPLATFORM with
# cargo-zigbuild (zig cc handles ring's C/asm for both musl targets) — no QEMU
# anywhere. rust-toolchain.toml pins the actual compiler; rustup installs it
# into a cached layer on first build.
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.0 AS base
WORKDIR /app
COPY rust-toolchain.toml ./
ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo x86_64-unknown-linux-musl  > /rust-target ;; \
      "linux/arm64") echo aarch64-unknown-linux-musl > /rust-target ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac \
 && rustup target add "$(cat /rust-target)"

# Hermetic CI gate — not part of the release path (multi-arch pushes would run
# it under emulation): docker build --target test .
FROM base AS test
RUN rustup component add clippy rustfmt
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY src ./src
COPY tests ./tests
RUN cargo fmt --check \
 && cargo clippy --all-targets --locked -- -D warnings \
 && cargo test --locked

FROM base AS build
ARG TARGETPLATFORM
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Per-platform cache IDs: multi-arch pushes run both platform builds
# concurrently, and a shared cargo registry/target cache mount makes the two
# cargo processes race on crate unpacking ("File exists").
RUN --mount=type=cache,id=cargo-registry-${TARGETPLATFORM},target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-target-${TARGETPLATFORM},target=/app/target \
    cargo zigbuild --release --locked --target "$(cat /rust-target)" \
 && cp "/app/target/$(cat /rust-target)/release/calendar-ics-mcp" /calendar-ics-mcp

# The binary is self-contained: rustls + webpki-roots (no CA files), chrono-tz
# (no tzdata), musl-static (no libc) — scratch needs nothing else.
FROM scratch AS release
COPY --from=build /calendar-ics-mcp /calendar-ics-mcp
USER 65534:65534
# ICS_URL is intentionally NOT baked into the image — supply it at runtime.
# stdio by default; set HTTP_BIND for the Streamable HTTP mode, where the same
# binary also serves GET /healthz and doubles as the healthcheck
# probe (`/calendar-ics-mcp --health`) since there is no shell in here.
# The binary handles SIGINT/SIGTERM and exits on stdin EOF; `--init` remains
# defense-in-depth for zombie reaping only.
ENTRYPOINT ["/calendar-ics-mcp"]
