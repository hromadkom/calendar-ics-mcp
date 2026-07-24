//! Streamable HTTP smoke test: spawns the real binary in HTTP mode (with a
//! bearer token), serves the fixture over loopback HTTP, and exercises the
//! whole surface — /mcp round-trips, notifications → 202, GET → 405, auth,
//! Origin guard, /healthz, and the `--health` self-probe.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};

use serde_json::Value;

use common::FIXTURE;

const TOKEN: &str = "smoke-test-token";

fn spawn_fixture_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            while !buf.ends_with(b"\r\n\r\n")
                && stream.read(&mut byte).map(|n| n > 0).unwrap_or(false)
            {
                buf.push(byte[0]);
            }
            let body = FIXTURE.as_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/calendar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    port
}

/// Spawn the server with HTTP_BIND=127.0.0.1:0 and read the actual port from
/// the "http transport listening" stderr log line.
#[allow(clippy::zombie_processes)] // killed + waited at the end of the test
fn spawn_http_server(feed_port: u16) -> (Child, u16) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_calendar-ics-mcp"))
        .env_clear()
        .env("ICS_URL", format!("http://127.0.0.1:{feed_port}/feed.ics"))
        .env("HTTP_BIND", "127.0.0.1:0")
        .env("HTTP_BEARER_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut lines = stderr.lines();
    for line in &mut lines {
        let line = line.unwrap();
        let log: Value = serde_json::from_str(&line).unwrap();
        if log["msg"] == "http transport listening" {
            let bind = log["bind"].as_str().unwrap();
            let port = bind.rsplit(':').next().unwrap().parse().unwrap();
            // Relay any further child stderr (logs, panics) to the test's
            // stderr so failures are diagnosable.
            std::thread::spawn(move || {
                for line in lines.map_while(Result::ok) {
                    eprintln!("[child] {line}");
                }
            });
            return (child, port);
        }
    }
    panic!("server never logged the listening address");
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(
    agent: &ureq::Agent,
    port: u16,
    auth: Option<&str>,
    origin: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut req = agent.post(format!("http://127.0.0.1:{port}/mcp"));
    if let Some(token) = auth {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(origin) = origin {
        req = req.header("Origin", origin);
    }
    let mut res = req.send(body).unwrap();
    let status = res.status().as_u16();
    let text = res.body_mut().read_to_string().unwrap();
    (status, text)
}

#[test]
fn full_streamable_http_surface() {
    let feed_port = spawn_fixture_server();
    let (mut child, port) = spawn_http_server(feed_port);
    let agent = agent();

    // /healthz is unauthenticated and never touches the feed.
    let mut res = agent
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    let health: Value = serde_json::from_str(&res.body_mut().read_to_string().unwrap()).unwrap();
    assert_eq!(health["status"], "ok");

    // initialize round-trip.
    let (status, text) = post(
        &agent,
        port,
        Some(TOKEN),
        None,
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"http-smoke","version":"0"}}}"#,
    );
    assert_eq!(status, 200);
    let init: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");

    // Notifications-only body → 202 Accepted, empty.
    let (status, text) = post(
        &agent,
        port,
        Some(TOKEN),
        None,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert_eq!(status, 202);
    assert!(text.is_empty());

    // tools/call through the real pipeline.
    let (status, text) = post(
        &agent,
        port,
        Some(TOKEN),
        None,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_events","arguments":{"date":"2026-03-18"}}}"#,
    );
    assert_eq!(status, 200);
    let call: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        call["result"]["structuredContent"]["events"]
            .as_array()
            .unwrap()
            .len(),
        4
    );

    // Batch → array response with the notification dropped.
    let (status, text) = post(
        &agent,
        port,
        Some(TOKEN),
        None,
        r#"[{"jsonrpc":"2.0","id":3,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
    );
    assert_eq!(status, 200);
    let batch: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(batch.as_array().unwrap().len(), 1);

    // Auth: missing/wrong token → 401; /healthz stays open (checked above).
    let (status, _) = post(
        &agent,
        port,
        None,
        None,
        r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#,
    );
    assert_eq!(status, 401);
    let (status, _) = post(
        &agent,
        port,
        Some("wrong"),
        None,
        r#"{"jsonrpc":"2.0","id":5,"method":"ping"}"#,
    );
    assert_eq!(status, 401);

    // Origin guard: foreign origins are rejected, localhost is fine.
    let (status, _) = post(
        &agent,
        port,
        Some(TOKEN),
        Some("http://evil.example"),
        r#"{"jsonrpc":"2.0","id":6,"method":"ping"}"#,
    );
    assert_eq!(status, 403);
    let (status, _) = post(
        &agent,
        port,
        Some(TOKEN),
        Some("http://localhost:6274"),
        r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#,
    );
    assert_eq!(status, 200);

    // No SSE stream is offered.
    let res = agent
        .get(format!("http://127.0.0.1:{port}/mcp"))
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 405);
    let res = agent
        .get(format!("http://127.0.0.1:{port}/nope"))
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 404);

    // The self-probe exits 0 against the live server…
    let status = Command::new(env!("CARGO_BIN_EXE_calendar-ics-mcp"))
        .env_clear()
        .env("HTTP_BIND", format!("127.0.0.1:{port}"))
        .arg("--health")
        .status()
        .unwrap();
    assert!(status.success());

    // …and 1 against a dead port.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    let status = Command::new(env!("CARGO_BIN_EXE_calendar-ics-mcp"))
        .env_clear()
        .env("HTTP_BIND", format!("127.0.0.1:{dead_port}"))
        .arg("--health")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));

    child.kill().unwrap();
    let _ = child.wait();
}
