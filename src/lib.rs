//! Crate root for the brenn-forgejo-mcp binary. Exposed as a library so
//! integration tests can exercise `Tool::call` directly without spawning
//! the MCP stdio server.

pub mod client;
pub mod config;
pub mod error;
pub mod redact;
pub mod tools;
