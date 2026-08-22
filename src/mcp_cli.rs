//! Shared MCP server management via the `crowded mcp` subcommand.

mod commands;

use std::path::Path;

pub(crate) use commands::command;

#[allow(dead_code)] // CLI contract fields are read by the config editing layer
pub(crate) struct NewMcpEntry {
    pub(crate) name: String,
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) url: Option<String>,
    pub(crate) transport: Option<crate::config::McpTransport>,
    pub(crate) clients: Vec<crate::config::McpClient>,
}

pub(crate) fn list_entries(_root: &Path) -> std::io::Result<Vec<crate::config::McpConfig>> {
    unimplemented!("shared MCP server editing")
}

pub(crate) fn add_entry(_root: &Path, _entry: NewMcpEntry) -> std::io::Result<()> {
    unimplemented!("shared MCP server editing")
}

pub(crate) fn remove_entry(_root: &Path, _name: &str) -> std::io::Result<()> {
    unimplemented!("shared MCP server editing")
}
