//! Bidirectional MCP support: `envoy mcp serve` server + client (`stdio` & Streamable HTTP).

pub mod client;
pub mod config;
pub mod server;
mod server_tools;
#[cfg(test)]
mod tests_security;
pub mod transport;
