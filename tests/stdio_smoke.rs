//! True subprocess smoke test: spawns the release binary, serves the fixture
//! over a loopback HTTP socket, speaks newline-delimited JSON-RPC over
//! stdin/stdout, and asserts stdout hygiene plus the exit-0-on-EOF contract.

#![allow(clippy::unwrap_used)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use common::FIXTURE;

/// Minimal canned HTTP/1.1 fixture server on an OS-assigned port; serves
/// every connection until the listener is dropped.
fn spawn_fixture_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            // Read the request until the header terminator.
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
    (port, handle)
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn full_stdio_session_with_a_real_subprocess() {
    let (port, _server) = spawn_fixture_server();

    let mut child = Command::new(env!("CARGO_BIN_EXE_calendar-ics-mcp"))
        .env_clear()
        .env("ICS_URL", format!("http://127.0.0.1:{port}/feed.ics"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    let mut send = |line: &str| {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    };
    let mut recv = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        // Every stdout line must be a single JSON-RPC message — any log
        // pollution would fail this parse.
        serde_json::from_str::<Value>(&line).unwrap()
    };

    send(
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#,
    );
    let init = recv();
    assert_eq!(init["id"], 0);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "calendar-ics-mcp");

    send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let list = recv();
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["feed_info", "get_events", "get_events_range"]);

    send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_events","arguments":{"date":"2026-03-18"}}}"#,
    );
    let call = recv();
    let structured = &call["result"]["structuredContent"];
    assert_eq!(structured["date"], "2026-03-18");
    assert_eq!(structured["events"].as_array().unwrap().len(), 4);

    // Closing stdin is the shutdown signal — the process must exit 0.
    drop(stdin);
    let status = wait_for_exit(&mut child, Duration::from_secs(10))
        .expect("server did not exit after stdin EOF");
    assert!(status.success(), "exit status: {status:?}");

    // The start line went to stderr, not stdout.
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("calendar-ics-mcp started"));
}
