//! MCP Streamable HTTP transport (for long-running agents) — stateless,
//! spec-legal minimum: `POST /mcp` carries JSON-RPC (single message or
//! batch), no sessions are issued (`Mcp-Session-Id` ignored), no SSE stream
//! is offered (`GET /mcp` → 405). `GET /healthz` is a liveness probe that
//! never touches the feed — container health must not depend on feed health.
//!
//! A small worker pool (2 threads) over a mutex-guarded server keeps
//! `/healthz` responsive while a tool call holds the lock during a slow feed
//! fetch; the mutex also preserves the single-flight cache property.

use std::io::Read;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::logger;
use crate::mcp::McpServer;
use crate::server::ServerState;

/// Requests are single JSON-RPC messages — anything above this is abuse.
const MAX_BODY_BYTES: u64 = 1_048_576;

const WORKER_THREADS: usize = 2;

pub fn run_http(
    mcp: McpServer<ServerState>,
    bind: SocketAddr,
    bearer_token: Option<String>,
) -> Result<(), String> {
    let server = Server::http(bind).map_err(|e| format!("failed to bind {bind}: {e}"))?;
    let actual = server
        .server_addr()
        .to_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|| bind.to_string());
    logger::info("http transport listening", &[("bind", json!(actual))]);

    let server = Arc::new(server);
    let state = Arc::new(Mutex::new(mcp));
    let mut workers = Vec::new();
    for _ in 0..WORKER_THREADS {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        let token = bearer_token.clone();
        workers.push(std::thread::spawn(move || {
            for request in server.incoming_requests() {
                handle_request(request, &state, token.as_deref());
            }
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn handle_request(mut req: Request, state: &Mutex<McpServer<ServerState>>, token: Option<&str>) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::Get, "/healthz") => {
            let body = json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") });
            respond(req, 200, Some(body.to_string()));
        }
        (Method::Post, "/mcp") => {
            // Browser DNS-rebinding guard (spec): a present-but-foreign
            // Origin is rejected; non-browser clients send none.
            if !origin_allowed(&req) {
                return respond_error(req, 403, "forbidden origin");
            }
            if let Some(expected) = token {
                let authorized = header_value(&req, "Authorization")
                    .is_some_and(|v| v == format!("Bearer {expected}"));
                if !authorized {
                    return respond_error(req, 401, "missing or invalid bearer token");
                }
            }
            let mut body = String::new();
            let mut limited = req.as_reader().take(MAX_BODY_BYTES + 1);
            if limited.read_to_string(&mut body).is_err() {
                return respond_error(req, 400, "unreadable request body");
            }
            if body.len() as u64 > MAX_BODY_BYTES {
                return respond_error(req, 413, "request body too large");
            }
            let response = {
                let mut guard = match state.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.handle_message(&body)
            };
            match response {
                Some(value) => respond(req, 200, Some(value.to_string())),
                // Notifications only — accepted, nothing to say.
                None => respond(req, 202, None),
            }
        }
        (_, "/mcp") => {
            // No SSE stream is offered; only POST carries messages.
            respond_error(req, 405, "method not allowed; POST JSON-RPC to /mcp");
        }
        _ => respond_error(req, 404, "not found"),
    }
}

fn header_value(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// Absent Origin (non-browser client) is allowed. A present Origin must be a
/// localhost variant or match the request's Host — anything else looks like
/// DNS rebinding.
fn origin_allowed(req: &Request) -> bool {
    let Some(origin) = header_value(req, "Origin") else {
        return true;
    };
    let Some(origin_host) = crate::errors::parse_url(&origin).map(|p| p.host) else {
        return false;
    };
    if matches!(origin_host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return true;
    }
    header_value(req, "Host")
        .map(|h| h.split(':').next().unwrap_or("").to_ascii_lowercase())
        .is_some_and(|request_host| request_host == origin_host)
}

fn respond(req: Request, status: u16, json_body: Option<String>) {
    let mut response =
        Response::from_string(json_body.unwrap_or_default()).with_status_code(status);
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
        response = response.with_header(header);
    }
    if status == 401 {
        if let Ok(header) = Header::from_bytes(&b"WWW-Authenticate"[..], &b"Bearer"[..]) {
            response = response.with_header(header);
        }
    }
    let _ = req.respond(response);
}

fn respond_error(req: Request, status: u16, message: &str) {
    respond(req, status, Some(json!({ "error": message }).to_string()));
}
