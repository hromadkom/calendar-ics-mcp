//! Thin binary wrapper: load config (exit 1 on failure), install signal
//! handlers, log the start line, then run the selected transport —
//! `HTTP_BIND` set → MCP Streamable HTTP, unset → stdio JSON-RPC
//! until the client disconnects (stdin EOF → exit 0).
//!
//! `calendar-ics-mcp --health` is the compose healthcheck probe: a plain GET
//! /healthz against HTTP_BIND (the scratch image has no shell or curl, so
//! the binary doubles as its own probe).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::json;

use calendar_ics_mcp::feed::{FeedClient, SystemClock, UreqTransport};
use calendar_ics_mcp::{config, errors, http, logger, mcp, server};

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--health") {
        std::process::exit(health_probe());
    }

    let cfg = match config::load_config(|k| std::env::var(k).ok()) {
        Ok(cfg) => cfg,
        Err(err) => {
            logger::error("Startup failed", &[("detail", json!(err.message()))]);
            std::process::exit(1);
        }
    };

    // Mirror the TS `process.exit(0)` on SIGINT/SIGTERM: nothing needs
    // flushing (the cache is memory-only), so shutdown is immediate.
    let always = Arc::new(AtomicBool::new(true));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        let _ = signal_hook::flag::register_conditional_shutdown(sig, 0, Arc::clone(&always));
    }

    let feed = FeedClient::new(
        cfg.clone(),
        UreqTransport::new(cfg.fetch_timeout_ms),
        SystemClock,
    );
    let mut server = mcp::McpServer {
        name: "calendar-ics-mcp",
        version: env!("CARGO_PKG_VERSION"),
        tools: server::ServerState::new(cfg.clone(), feed),
    };

    logger::info(
        "calendar-ics-mcp started",
        &[
            ("cacheTtlSeconds", json!(cfg.cache_ttl_seconds)),
            ("fetchTimeoutMs", json!(cfg.fetch_timeout_ms)),
            ("source", json!(errors::mask_ics_url(&cfg.ics_url))),
            ("tz", json!(cfg.tz_default.name())),
        ],
    );

    if let Some(bind) = cfg.http_bind {
        if let Err(detail) = http::run_http(server, bind, cfg.http_bearer_token.clone()) {
            logger::error("http transport failed", &[("detail", json!(detail))]);
            std::process::exit(1);
        }
        return;
    }

    let stdin = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    if let Err(err) = server.run_stdio(stdin, stdout) {
        logger::error(
            "stdio transport failed",
            &[("detail", json!(err.to_string()))],
        );
        std::process::exit(1);
    }
}

/// Liveness probe against the local /healthz. Exit 0 on HTTP 200, 1 on
/// anything else. Needs only HTTP_BIND (a wildcard bind is probed via
/// loopback).
fn health_probe() -> i32 {
    use std::io::{Read as _, Write as _};

    let Ok(bind) = std::env::var("HTTP_BIND") else {
        eprintln!(r#"{{"level":"error","msg":"--health requires HTTP_BIND"}}"#);
        return 1;
    };
    let addr = bind
        .trim()
        .replace("0.0.0.0", "127.0.0.1")
        .replace("[::]", "[::1]");
    let Ok(parsed) = addr.parse::<std::net::SocketAddr>() else {
        return 1;
    };
    let timeout = std::time::Duration::from_secs(3);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&parsed, timeout) else {
        return 1;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: healthcheck\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return 1;
    }
    let mut response = String::new();
    let _ = stream.take(1024).read_to_string(&mut response);
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        0
    } else {
        1
    }
}
