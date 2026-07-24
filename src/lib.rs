//! calendar-ics-mcp — read-only MCP server for an Outlook/Exchange published
//! ICS calendar feed.
//!
//! Library crate so integration tests under `tests/` can exercise the
//! internals; `main.rs` is a thin binary wrapper.

#![warn(clippy::unwrap_used)]

pub mod config;
pub mod errors;
pub mod feed;
pub mod http;
pub mod ics;
pub mod logger;
pub mod mcp;
pub mod server;
