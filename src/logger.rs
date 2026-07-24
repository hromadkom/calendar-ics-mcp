//! stdout carries JSON-RPC on the stdio transport — every log line goes to
//! stderr (Claude Desktop shows stderr in its MCP logs). This module is the
//! only place allowed to write to stderr directly.

use std::io::Write as _;

use serde_json::{Map, Value};

fn write(level: &str, msg: &str, ctx: &[(&str, Value)]) {
    let mut obj = Map::new();
    obj.insert("level".to_string(), Value::String(level.to_string()));
    obj.insert("msg".to_string(), Value::String(msg.to_string()));
    obj.insert("time".to_string(), Value::String(now_iso()));
    for (key, value) in ctx {
        obj.insert((*key).to_string(), value.clone());
    }
    // Never `eprintln!` here: it panics when stderr is closed (e.g. a
    // supervisor dropped the pipe), which would kill a worker thread mid
    // request. Logging is best-effort by design.
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{}", Value::Object(obj));
}

/// UTC with milliseconds — the same shape as JS `Date.toISOString()`.
fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

pub fn error(msg: &str, ctx: &[(&str, Value)]) {
    write("error", msg, ctx);
}

pub fn info(msg: &str, ctx: &[(&str, Value)]) {
    write("info", msg, ctx);
}

pub fn warn(msg: &str, ctx: &[(&str, Value)]) {
    write("warn", msg, ctx);
}
