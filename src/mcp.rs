//! Minimal MCP server protocol layer: JSON-RPC 2.0, newline-delimited over
//! stdio (and reused verbatim by the Streamable HTTP transport).
//!
//! Deliberately hand-rolled instead of an SDK: the surface this read-only
//! server needs is five methods, and dropping the async machinery is the
//! biggest part of the minimal-RSS budget. `handle_message` is pure — every
//! transport and test drives it directly.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Newest protocol revision this server speaks; also the fallback offered
/// when a client asks for something unknown.
pub const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

pub trait ToolProvider {
    /// The complete `tools` array for `tools/list`.
    fn list_tools(&self) -> Value;
    /// Execute a tool; `Err` becomes a JSON-RPC error (unknown tool), domain
    /// failures are `Ok` results carrying `isError: true`.
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, RpcError>;
}

pub struct McpServer<T: ToolProvider> {
    pub name: &'static str,
    pub version: &'static str,
    pub tools: T,
}

impl<T: ToolProvider> McpServer<T> {
    /// Serial blocking loop: one JSON-RPC message per line in, one line out.
    /// Returns cleanly on stdin EOF — the stdio shutdown contract (the
    /// process exits 0 when the client disconnects).
    pub fn run_stdio(
        &mut self,
        mut input: impl BufRead,
        mut output: impl Write,
    ) -> std::io::Result<()> {
        let mut line = String::new();
        loop {
            line.clear();
            if input.read_line(&mut line)? == 0 {
                return Ok(());
            }
            if let Some(response) = self.handle_message(&line) {
                let payload = serde_json::to_string(&response).map_err(std::io::Error::other)?;
                output.write_all(payload.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
    }

    /// Handle one raw line. `None` means "no response" (notification, blank
    /// line, or a client response we never asked for).
    pub fn handle_message(&mut self, raw: &str) -> Option<Value> {
        let raw = raw.trim_start_matches('\u{feff}').trim();
        if raw.is_empty() {
            return None;
        }
        let Ok(msg) = serde_json::from_str::<Value>(raw) else {
            return Some(error_response(Value::Null, -32700, "Parse error"));
        };
        match msg {
            // Pre-2025-06-18 clients may send JSON-RPC batches.
            Value::Array(batch) => {
                let responses: Vec<Value> =
                    batch.iter().filter_map(|m| self.handle_value(m)).collect();
                (!responses.is_empty()).then(|| Value::Array(responses))
            }
            other => self.handle_value(&other),
        }
    }

    fn handle_value(&mut self, msg: &Value) -> Option<Value> {
        if !msg.is_object() {
            return Some(error_response(Value::Null, -32600, "Invalid Request"));
        }
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            // Notification (or a malformed/unanswerable message): never respond.
            // notifications/initialized and notifications/cancelled land here.
            return None;
        }
        let Some(method) = method else {
            // A response from the client — we never send requests, ignore.
            return None;
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        let result = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let negotiated = match requested {
                    Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
                    _ => LATEST_PROTOCOL_VERSION,
                };
                json!({
                    "protocolVersion": negotiated,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": self.name, "version": self.version },
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tools.list_tools() }),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error_response(
                        id,
                        -32602,
                        "Invalid params: missing tool name",
                    ));
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.tools.call_tool(name, &args) {
                    Ok(result) => result,
                    Err(rpc) => return Some(error_response(id, rpc.code, &rpc.message)),
                }
            }
            _ => return Some(error_response(id, -32601, "Method not found")),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct DummyTools;

    impl ToolProvider for DummyTools {
        fn list_tools(&self) -> Value {
            json!([{ "name": "dummy" }])
        }

        fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, RpcError> {
            match name {
                "dummy" => Ok(json!({ "content": [], "echo": args })),
                _ => Err(RpcError {
                    code: -32602,
                    message: format!("Unknown tool: {name}"),
                }),
            }
        }
    }

    fn server() -> McpServer<DummyTools> {
        McpServer {
            name: "calendar-ics-mcp",
            version: "0.0.0-test",
            tools: DummyTools,
        }
    }

    fn handle(raw: &str) -> Option<Value> {
        server().handle_message(raw)
    }

    #[test]
    fn initialize_echoes_a_supported_protocol_version() {
        let res = handle(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        )
        .unwrap();
        assert_eq!(res["id"], 0);
        assert_eq!(res["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(res["result"]["serverInfo"]["name"], "calendar-ics-mcp");
        assert!(res["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialize_falls_back_to_latest_on_unknown_version() {
        let res = handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .unwrap();
        assert_eq!(res["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
    }

    #[test]
    fn ping_returns_an_empty_result() {
        let res = handle(r#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#).unwrap();
        assert_eq!(res["result"], json!({}));
        assert_eq!(res["id"], 7);
    }

    #[test]
    fn string_ids_round_trip() {
        let res = handle(r#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap();
        assert_eq!(res["id"], "abc");
    }

    #[test]
    fn unknown_method_with_id_is_32601() {
        let res = handle(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#).unwrap();
        assert_eq!(res["error"]["code"], -32601);
    }

    #[test]
    fn notifications_get_no_response() {
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(
            handle(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{}}"#).is_none()
        );
    }

    #[test]
    fn blank_lines_are_ignored_and_garbage_is_32700() {
        assert!(handle("").is_none());
        assert!(handle("   \n").is_none());
        let res = handle("{not json").unwrap();
        assert_eq!(res["error"]["code"], -32700);
        assert_eq!(res["id"], Value::Null);
    }

    #[test]
    fn tools_list_passes_through_the_provider() {
        let res = handle(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#).unwrap();
        assert_eq!(res["result"]["tools"][0]["name"], "dummy");
    }

    #[test]
    fn tools_call_dispatches_and_defaults_missing_arguments() {
        let res =
            handle(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"dummy"}}"#)
                .unwrap();
        assert_eq!(res["result"]["echo"], json!({}));
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let res = handle(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
        )
        .unwrap();
        assert_eq!(res["error"]["code"], -32602);
        assert!(res["error"]["message"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn batches_collect_responses_and_drop_notifications() {
        let res = handle(
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
        )
        .unwrap();
        let arr = res.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
    }

    #[test]
    fn run_stdio_frames_one_response_per_line_and_stops_on_eof() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n".to_vec();
        let mut out = Vec::new();
        server().run_stdio(&input[..], &mut out).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .unwrap()
            .trim_end()
            .lines()
            .collect();
        assert_eq!(lines.len(), 1);
        let parsed: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["result"], json!({}));
    }
}
