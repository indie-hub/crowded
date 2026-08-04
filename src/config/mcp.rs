//! Shared MCP and OpenCode-plugin configuration, independent of room parsing.

use std::{collections::HashSet, ffi::OsString, io, path::PathBuf};

use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpConfig {
    pub(crate) name: String,
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) url: Option<String>,
    pub(crate) transport: Option<McpTransport>,
    #[serde(default)]
    pub(crate) clients: Vec<McpClient>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpTransport {
    Http,
    Sse,
}

impl McpTransport {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Sse => "sse",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpClient {
    Claude,
    Codex,
    Opencode,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenCodePluginConfig {
    pub(crate) package: String,
}

impl McpConfig {
    pub(crate) fn supports(&self, client: McpClient) -> bool {
        self.clients.is_empty() || self.clients.contains(&client)
    }

    pub(crate) fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    pub(crate) fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
}

fn has_http_host(url: &str) -> bool {
    let Some(after_scheme) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let host_and_port = authority.rsplit('@').next().unwrap_or_default();
    if let Some(bracketed) = host_and_port.strip_prefix('[') {
        return bracketed.split_once(']').is_some_and(|(host, suffix)| {
            !host.is_empty() && (suffix.is_empty() || suffix.starts_with(':'))
        });
    }
    host_and_port
        .split(':')
        .next()
        .is_some_and(|host| !host.is_empty())
}

pub(crate) fn validate_mcp_servers(servers: &[McpConfig]) -> io::Result<()> {
    let mut names = HashSet::new();
    for server in servers {
        if server.name.is_empty()
            || server.name.len() > 64
            || !server
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP name must contain 1..=64 ASCII letters, digits, hyphens, or underscores",
            ));
        }
        if !names.insert(&server.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate MCP name: {}", server.name),
            ));
        }
        match (
            server.command.as_deref(),
            server.url.as_deref(),
            server.transport,
        ) {
            (Some(command), None, None) => {
                if command.trim().is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("MCP {} command cannot be empty", server.name),
                    ));
                }
            }
            (None, Some(url), Some(transport)) => {
                if !has_http_host(url) || url.chars().any(char::is_whitespace) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("MCP {} URL must be an HTTP(S) URL", server.name),
                    ));
                }
                if !server.args.is_empty() || server.cwd.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "MCP {} remote transport cannot use args or cwd",
                            server.name
                        ),
                    ));
                }
                if transport == McpTransport::Sse && server.supports(McpClient::Codex) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "MCP {} uses legacy SSE, which cannot target Codex; set clients to claude and/or opencode",
                            server.name
                        ),
                    ));
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "MCP {} must set command for local stdio, or url and transport for remote HTTP/SSE",
                        server.name
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_opencode_plugins(plugins: &[OpenCodePluginConfig]) -> io::Result<()> {
    let mut packages = HashSet::new();
    for plugin in plugins {
        if plugin.package.trim().is_empty() || plugin.package.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OpenCode plugin package cannot be empty or contain control characters",
            ));
        }
        if !packages.insert(&plugin.package) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate OpenCode plugin package: {}", plugin.package),
            ));
        }
    }
    Ok(())
}

pub(crate) fn claude_mcp_config(servers: &[McpConfig]) -> io::Result<String> {
    let mut configured = serde_json::Map::new();
    for server in servers
        .iter()
        .filter(|server| server.supports(McpClient::Claude))
    {
        let mut entry = if let (Some(url), Some(transport)) = (&server.url, server.transport) {
            serde_json::json!({
                "type": transport.as_str(),
                "url": url,
            })
        } else {
            serde_json::json!({
                "command": server.command,
                "args": server.args,
            })
        };
        if server.transport.is_none()
            && let Some(cwd) = &server.cwd
        {
            entry["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
        }
        configured.insert(server.name.clone(), entry);
    }
    serde_json::to_string(&serde_json::json!({ "mcpServers": configured }))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn codex_mcp_args(servers: &[McpConfig]) -> Vec<OsString> {
    let mut args = Vec::new();
    for server in servers
        .iter()
        .filter(|server| server.supports(McpClient::Codex))
    {
        let prefix = format!("mcp_servers.{}", server.name);
        if let Some(url) = &server.url {
            args.extend([
                "-c".into(),
                format!("{prefix}.url={}", toml::Value::String(url.clone())).into(),
            ]);
        } else {
            args.extend([
                "-c".into(),
                format!(
                    "{prefix}.command={}",
                    toml::Value::String(server.command.clone().unwrap_or_default())
                )
                .into(),
                "-c".into(),
                format!(
                    "{prefix}.args={}",
                    toml::Value::Array(
                        server
                            .args
                            .iter()
                            .cloned()
                            .map(toml::Value::String)
                            .collect()
                    )
                )
                .into(),
            ]);
            if let Some(cwd) = &server.cwd {
                args.extend([
                    "-c".into(),
                    format!(
                        "{prefix}.cwd={}",
                        toml::Value::String(cwd.to_string_lossy().into_owned())
                    )
                    .into(),
                ]);
            }
        }
    }
    args
}

pub(crate) fn opencode_mcp_config(
    existing: Option<&str>,
    servers: &[McpConfig],
    plugins: &[OpenCodePluginConfig],
) -> io::Result<String> {
    let mut config = match existing {
        Some(existing) => serde_json::from_str(existing)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        None => serde_json::json!({}),
    };
    let root = config.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "OPENCODE_CONFIG_CONTENT must contain a JSON object",
        )
    })?;
    let open_code_servers: Vec<_> = servers
        .iter()
        .filter(|server| server.supports(McpClient::Opencode))
        .collect();
    if !open_code_servers.is_empty() {
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OPENCODE_CONFIG_CONTENT mcp must be a JSON object",
                )
            })?;
        for server in open_code_servers {
            let mut entry = if let Some(url) = &server.url {
                serde_json::json!({
                    "type": "remote",
                    "url": url,
                    "enabled": true,
                })
            } else {
                let mut command = vec![server.command.clone().unwrap_or_default()];
                command.extend(server.args.iter().cloned());
                serde_json::json!({
                    "type": "local",
                    "command": command,
                    "enabled": true,
                })
            };
            if server.transport.is_none()
                && let Some(cwd) = &server.cwd
            {
                entry["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
            }
            mcp.insert(server.name.clone(), entry);
        }
    }
    if !plugins.is_empty() {
        let configured = root
            .entry("plugin")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OPENCODE_CONFIG_CONTENT plugin must be an array",
                )
            })?;
        for plugin in plugins {
            let package = serde_json::Value::String(plugin.package.clone());
            if !configured.contains(&package) {
                configured.push(package);
            }
        }
    }

    serde_json::to_string(&config)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
